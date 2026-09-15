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

/// Logical BUD channels. Video/audio are unreliable; input/control are reliable.
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
}
