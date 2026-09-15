use lansec_protocol::AudioPacket;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("{0}")]
    Message(String),
}

const SAMPLE_RATE: i32 = 48_000;
const CHANNELS: usize = 2;
pub const FRAME_SAMPLES: usize = 480; // 10 ms at 48 kHz

pub struct PcmGather {
    buf: Vec<f32>,
}

impl Default for PcmGather {
    fn default() -> Self {
        Self {
            buf: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 4),
        }
    }
}

impl PcmGather {
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        self.buf.extend_from_slice(samples);
        let mut out = Vec::new();
        let need = FRAME_SAMPLES * CHANNELS;
        while self.buf.len() >= need {
            out.push(self.buf.drain(..need).collect());
        }
        out
    }
}

pub struct OpusRoundtrip {
    encoder: opus_rs::OpusEncoder,
    decoder: opus_rs::OpusDecoder,
    seq: u32,
}

impl OpusRoundtrip {
    pub fn new() -> Result<Self, AudioError> {
        let encoder = opus_rs::OpusEncoder::new(
            SAMPLE_RATE,
            CHANNELS,
            opus_rs::Application::RestrictedLowDelay,
        )
        .map_err(|e| AudioError::Message(e.to_string()))?;
        let decoder =
            opus_rs::OpusDecoder::new(SAMPLE_RATE, CHANNELS).map_err(|e| AudioError::Message(e.to_string()))?;
        Ok(Self {
            encoder,
            decoder,
            seq: 0,
        })
    }

    pub fn encode_pcm(&mut self, pcm: &[f32]) -> Result<AudioPacket, AudioError> {
        let mut opus = vec![0u8; 4000];
        let n = self
            .encoder
            .encode(pcm, FRAME_SAMPLES, &mut opus)
            .map_err(|e| AudioError::Message(e.to_string()))?;
        opus.truncate(n);
        self.seq = self.seq.wrapping_add(1);
        Ok(AudioPacket {
            seq: self.seq,
            samples: FRAME_SAMPLES as u16,
            opus,
        })
    }

    pub fn decode_opus(&mut self, pkt: &AudioPacket) -> Result<Vec<f32>, AudioError> {
        let mut pcm = vec![0f32; FRAME_SAMPLES * CHANNELS];
        let n = self
            .decoder
            .decode(&pkt.opus, FRAME_SAMPLES, &mut pcm)
            .map_err(|e| AudioError::Message(e.to_string()))?;
        pcm.truncate(n * CHANNELS);
        Ok(pcm)
    }
}

pub mod play;

#[cfg(windows)]
pub mod wasapi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_silence_roundtrip() {
        let mut rt = OpusRoundtrip::new().unwrap();
        let pcm = vec![0f32; FRAME_SAMPLES * 2];
        let pkt = rt.encode_pcm(&pcm).unwrap();
        assert!(!pkt.opus.is_empty());
        let out = rt.decode_opus(&pkt).unwrap();
        assert!(!out.is_empty());
    }
}
