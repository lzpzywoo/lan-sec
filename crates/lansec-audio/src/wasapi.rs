//! WASAPI loopback capture of the default render device.

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

use crate::AudioError;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

pub struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: u16,
    rate: u32,
    bits: u16,
    is_float: bool,
}

impl Loopback {
    pub fn open() -> Result<Self, AudioError> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| AudioError::Message(e.to_string()))?;
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| AudioError::Message(e.to_string()))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| AudioError::Message(e.to_string()))?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| AudioError::Message(e.to_string()))?;
            let mix = client.GetMixFormat().map_err(|e| AudioError::Message(e.to_string()))?;
            if mix.is_null() {
                return Err(AudioError::Message("GetMixFormat null".into()));
            }
            let fmt: WAVEFORMATEX = *mix;
            let is_float = fmt.wFormatTag == WAVE_FORMAT_IEEE_FLOAT
                || fmt.wFormatTag == WAVE_FORMAT_EXTENSIBLE
                || fmt.wBitsPerSample == 32;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    10_000_000,
                    0,
                    mix,
                    None,
                )
                .map_err(|e| AudioError::Message(e.to_string()))?;
            CoTaskMemFree(Some(mix as *const _));
            let capture: IAudioCaptureClient = client.GetService().map_err(|e| AudioError::Message(e.to_string()))?;
            client.Start().map_err(|e| AudioError::Message(e.to_string()))?;
            let channels = fmt.nChannels;
            let rate = fmt.nSamplesPerSec;
            let bits = fmt.wBitsPerSample;
            tracing::info!(channels, rate, bits, "WASAPI loopback started");
            Ok(Self {
                client,
                capture,
                channels: channels.max(1),
                rate,
                bits,
                is_float: is_float || fmt.wFormatTag != WAVE_FORMAT_PCM,
            })
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub fn read_f32(&mut self) -> Result<Vec<f32>, AudioError> {
        unsafe {
            let packets = self
                .capture
                .GetNextPacketSize()
                .map_err(|e| AudioError::Message(e.to_string()))?;
            if packets == 0 {
                return Ok(Vec::new());
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            self.capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                .map_err(|e| AudioError::Message(e.to_string()))?;
            let ch = self.channels as usize;
            let mut stereo = Vec::with_capacity(frames as usize * 2);
            if self.is_float || self.bits == 32 {
                let samples = std::slice::from_raw_parts(data as *const f32, frames as usize * ch);
                for frame in samples.chunks(ch) {
                    stereo.push(*frame.first().unwrap_or(&0.0));
                    stereo.push(if ch > 1 { frame[1] } else { frame[0] });
                }
            } else {
                let samples = std::slice::from_raw_parts(data as *const i16, frames as usize * ch);
                for frame in samples.chunks(ch) {
                    stereo.push(*frame.first().unwrap_or(&0) as f32 / 32768.0);
                    stereo.push(if ch > 1 { frame[1] as f32 / 32768.0 } else { stereo.last().copied().unwrap_or(0.0) });
                }
            }
            self.capture
                .ReleaseBuffer(frames)
                .map_err(|e| AudioError::Message(e.to_string()))?;
            Ok(stereo)
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}
