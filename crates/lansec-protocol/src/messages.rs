use serde::{Deserialize, Serialize};

use crate::caps::{Caps, NegotiatedFormat};
use crate::timing::FrameTimes;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("postcard encode: {0}")]
    Encode(postcard::Error),
    #[error("postcard decode: {0}")]
    Decode(postcard::Error),
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_allocvec(value).map_err(ProtocolError::Encode)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ProtocolError> {
    postcard::from_bytes(bytes).map_err(ProtocolError::Decode)
}

/// Decode a control datagram.
///
/// Accepts Caps with an optional trailing `chroma_pref` byte from briefly-shipped
/// V2 peers. Caps itself no longer carries that field on the wire.
pub fn decode_control(bytes: &[u8]) -> Result<ControlMsg, ProtocolError> {
    // Prefer exact consume. Trailing chroma_pref from briefly-shipped V2 Caps must
    // not be ignored via from_bytes (leftover OK) — re-parse as V2 when rest remains.
    match postcard::take_from_bytes::<ControlMsg>(bytes) {
        Ok((msg, rest)) if rest.is_empty() => Ok(msg),
        Ok((ControlMsg::CapsOffer(_), rest)) if !rest.is_empty() => {
            decode_control_with_chroma_pref(bytes)
        }
        Ok((msg, _)) => Ok(msg),
        Err(_) => decode_control_with_chroma_pref(bytes),
    }
}

fn decode_control_with_chroma_pref(bytes: &[u8]) -> Result<ControlMsg, ProtocolError> {
    #[derive(Deserialize)]
    struct CapsV2 {
        encode: Vec<crate::caps::CodecCap>,
        decode: Vec<crate::caps::CodecCap>,
        audio: bool,
        input: bool,
        platform: crate::caps::Platform,
        #[allow(dead_code)]
        chroma_pref: crate::caps::ChromaPref,
    }

    #[derive(Deserialize)]
    enum ControlV2 {
        CapsOffer(CapsV2),
        CapsAccept { format: NegotiatedFormat },
        RequestIdr,
        Congestion(CongestionReport),
        Bye,
    }

    let v2: ControlV2 = postcard::from_bytes(bytes).map_err(ProtocolError::Decode)?;
    Ok(match v2 {
        ControlV2::CapsOffer(c) => ControlMsg::CapsOffer(Caps {
            encode: c.encode,
            decode: c.decode,
            audio: c.audio,
            input: c.input,
            platform: c.platform,
        }),
        ControlV2::CapsAccept { format } => ControlMsg::CapsAccept { format },
        ControlV2::RequestIdr => ControlMsg::RequestIdr,
        ControlV2::Congestion(r) => ControlMsg::Congestion(r),
        ControlV2::Bye => ControlMsg::Bye,
    })
}

/// Logical BUD channels. Video/audio are unreliable. Control is reliable.
/// Input clicks/keys are reliable; mouse moves are sent unreliable (latest-wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Channel {
    Control = 0,
    Video = 1,
    Audio = 2,
    Input = 3,
}

impl Channel {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Control),
            1 => Some(Self::Video),
            2 => Some(Self::Audio),
            3 => Some(Self::Input),
            _ => None,
        }
    }

    pub fn is_reliable(self) -> bool {
        matches!(self, Self::Control | Self::Input)
    }

    pub fn idx(self) -> usize {
        self as u8 as usize
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMsg {
    CapsOffer(Caps),
    CapsAccept { format: NegotiatedFormat },
    RequestIdr,
    Congestion(CongestionReport),
    Bye,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CongestionReport {
    pub rtt_us: u32,
    pub loss_ppm: u32,
    pub suggested_bitrate_bps: u32,
}

/// Encoded access unit. Annex-B HEVC NALUs concatenated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoAccessUnit {
    pub frame_id: u32,
    pub is_keyframe: bool,
    pub width: u16,
    pub height: u16,
    pub times: FrameTimes,
    pub annexb: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioPacket {
    pub seq: u32,
    pub samples: u16,
    pub opus: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMoveAbs { x: u16, y: u16, host_w: u16, host_h: u16 },
    MouseMoveRel { dx: i16, dy: i16 },
    MouseButton { button: u8, down: bool },
    MouseWheel { dx: i16, dy: i16 },
    Key { vk: u16, down: bool, scancode: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_control() {
        let msg = ControlMsg::RequestIdr;
        let bytes = encode(&msg).unwrap();
        let back: ControlMsg = decode(&bytes).unwrap();
        assert!(matches!(back, ControlMsg::RequestIdr));
    }

    #[test]
    fn decode_control_accepts_caps_with_or_without_chroma_pref() {
        use crate::caps::{Chroma, CodecCap, DecodeBackend, EncodeBackend, Platform};

        // Current wire (no chroma_pref).
        let v1 = ControlMsg::CapsOffer(Caps {
            encode: vec![],
            decode: vec![CodecCap::decode(DecodeBackend::D3d11va, Chroma::Yuv420, 3840, 2160)],
            audio: true,
            input: true,
            platform: Platform::Windows,
        });
        let bytes = encode(&v1).unwrap();
        let msg = decode_control(&bytes).unwrap();
        assert!(matches!(msg, ControlMsg::CapsOffer(_)));

        // Brief V2 (with chroma_pref) from peers that still send it.
        #[derive(Serialize)]
        struct CapsV2 {
            encode: Vec<CodecCap>,
            decode: Vec<CodecCap>,
            audio: bool,
            input: bool,
            platform: Platform,
            chroma_pref: crate::caps::ChromaPref,
        }
        #[derive(Serialize)]
        enum ControlV2 {
            CapsOffer(CapsV2),
        }
        let v2 = ControlV2::CapsOffer(CapsV2 {
            encode: vec![CodecCap::encode(
                EncodeBackend::VideoToolbox,
                Chroma::Yuv420,
                1920,
                1080,
            )],
            decode: vec![],
            audio: true,
            input: true,
            platform: Platform::Macos,
            chroma_pref: crate::caps::ChromaPref::Auto,
        });
        let bytes2 = postcard::to_allocvec(&v2).unwrap();
        let back = decode_control(&bytes2).unwrap();
        assert!(matches!(back, ControlMsg::CapsOffer(_)));
    }
}
