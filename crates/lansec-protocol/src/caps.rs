use serde::{Deserialize, Serialize};

/// HEVC is the only codec in v1. Hardware 4:4:4 is HEVC RExt / Main 4:4:4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    Hevc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Chroma {
    /// True-color path. Prefer this when both peers advertise it.
    Yuv444,
    /// Hardware-universal fallback on Apple Media Engine and older GPUs.
    Yuv420,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EncodeBackend {
    Nvenc,
    Qsv,
    VideoToolbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DecodeBackend {
    Nvdec,
    D3d11va,
    Qsv,
    VideoToolbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCap {
    pub codec: Codec,
    pub chroma: Chroma,
    pub bit_depth: u8,
    pub max_width: u16,
    pub max_height: u16,
    pub encode: Option<EncodeBackend>,
    pub decode: Option<DecodeBackend>,
}

impl CodecCap {
    pub fn encode(backend: EncodeBackend, chroma: Chroma, max_width: u16, max_height: u16) -> Self {
        Self {
            codec: Codec::Hevc,
            chroma,
            bit_depth: 8,
            max_width,
            max_height,
            encode: Some(backend),
            decode: None,
        }
    }

    pub fn decode(backend: DecodeBackend, chroma: Chroma, max_width: u16, max_height: u16) -> Self {
        Self {
            codec: Codec::Hevc,
            chroma,
            bit_depth: 8,
            max_width,
            max_height,
            encode: None,
            decode: Some(backend),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Caps {
    pub encode: Vec<CodecCap>,
    pub decode: Vec<CodecCap>,
    pub audio: bool,
    pub input: bool,
    pub platform: Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Platform {
    #[default]
    Unknown,
    Windows,
    Macos,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Unknown
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedFormat {
    pub codec: Codec,
    pub chroma: Chroma,
    pub bit_depth: u8,
    pub encode: EncodeBackend,
    pub decode: DecodeBackend,
    pub width: u16,
    pub height: u16,
}

impl NegotiatedFormat {
    pub fn chroma_label(&self) -> &'static str {
        match self.chroma {
            Chroma::Yuv444 => "4:4:4 true color",
            Chroma::Yuv420 => "4:2:0 (chroma subsampled fallback)",
        }
    }
}

/// Prefer HEVC 4:4:4 8-bit, then HEVC 4:2:0. Intersection of host encode and client decode.
///
/// Mac host → Windows client stays on 4:2:0 for now. Intel D3D11VA advertises HEVC
/// Main 4:4:4, but VideoToolbox Main 4:4:4 bitstreams present as a white frame
/// (mouse/keyboard still work). Windows host → Mac client remains the 4:4:4 path.
pub fn negotiate(host: &Caps, client: &Caps) -> Option<NegotiatedFormat> {
    let order: &[Chroma] = if host.platform == Platform::Macos && client.platform == Platform::Windows {
        &[Chroma::Yuv420, Chroma::Yuv444]
    } else {
        &[Chroma::Yuv444, Chroma::Yuv420]
    };
    for chroma in order.iter().copied() {
        for enc in host
            .encode
            .iter()
            .filter(|c| c.chroma == chroma && c.codec == Codec::Hevc && c.encode.is_some())
        {
            if let Some(dec) = client.decode.iter().find(|d| {
                d.chroma == chroma && d.codec == Codec::Hevc && d.decode.is_some() && d.bit_depth == enc.bit_depth
            }) {
                return Some(NegotiatedFormat {
                    codec: Codec::Hevc,
                    chroma,
                    bit_depth: enc.bit_depth,
                    encode: enc.encode.unwrap(),
                    decode: dec.decode.unwrap(),
                    width: enc.max_width.min(dec.max_width),
                    height: enc.max_height.min(dec.max_height),
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_444_when_both_sides_have_it() {
        let host = Caps {
            encode: vec![
                CodecCap::encode(EncodeBackend::Nvenc, Chroma::Yuv444, 3840, 2160),
                CodecCap::encode(EncodeBackend::Nvenc, Chroma::Yuv420, 3840, 2160),
            ],
            decode: vec![],
            audio: true,
            input: true,
            platform: Platform::Windows,
        };
        let client = Caps {
            encode: vec![],
            decode: vec![
                CodecCap::decode(DecodeBackend::VideoToolbox, Chroma::Yuv444, 3840, 2160),
                CodecCap::decode(DecodeBackend::VideoToolbox, Chroma::Yuv420, 3840, 2160),
            ],
            audio: true,
            input: true,
            platform: Platform::Macos,
        };
        let fmt = negotiate(&host, &client).unwrap();
        assert_eq!(fmt.chroma, Chroma::Yuv444);
        assert_eq!(fmt.encode, EncodeBackend::Nvenc);
        assert_eq!(fmt.decode, DecodeBackend::VideoToolbox);
    }

    #[test]
    fn mac_host_windows_client_prefers_420() {
        let host = Caps {
            encode: vec![
                CodecCap::encode(EncodeBackend::VideoToolbox, Chroma::Yuv444, 2560, 1600),
                CodecCap::encode(EncodeBackend::VideoToolbox, Chroma::Yuv420, 2560, 1600),
            ],
            decode: vec![],
            audio: true,
            input: true,
            platform: Platform::Macos,
        };
        let client = Caps {
            encode: vec![],
            decode: vec![
                CodecCap::decode(DecodeBackend::D3d11va, Chroma::Yuv444, 3840, 2160),
                CodecCap::decode(DecodeBackend::D3d11va, Chroma::Yuv420, 3840, 2160),
            ],
            audio: true,
            input: true,
            platform: Platform::Windows,
        };
        let fmt = negotiate(&host, &client).unwrap();
        assert_eq!(fmt.chroma, Chroma::Yuv420);
        assert_eq!(fmt.encode, EncodeBackend::VideoToolbox);
        assert_eq!(fmt.decode, DecodeBackend::D3d11va);
    }

    #[test]
    fn falls_back_to_420() {
        let host = Caps {
            encode: vec![CodecCap::encode(
                EncodeBackend::VideoToolbox,
                Chroma::Yuv420,
                2560,
                1600,
            )],
            decode: vec![],
            audio: false,
            input: true,
            platform: Platform::Macos,
        };
        let client = Caps {
            encode: vec![],
            decode: vec![
                CodecCap::decode(DecodeBackend::Nvdec, Chroma::Yuv444, 3840, 2160),
                CodecCap::decode(DecodeBackend::D3d11va, Chroma::Yuv420, 3840, 2160),
            ],
            audio: false,
            input: true,
            platform: Platform::Windows,
        };
        let fmt = negotiate(&host, &client).unwrap();
        assert_eq!(fmt.chroma, Chroma::Yuv420);
    }
}
