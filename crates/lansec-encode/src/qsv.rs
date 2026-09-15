use lansec_protocol::{Chroma, EncodeBackend};
use tracing::warn;

use crate::{EncodeError, EncodedAu, EncoderConfig, HardwareEncoder, Result};
use lansec_capture::GpuFrame;

pub fn open(cfg: EncoderConfig) -> Result<Box<dyn HardwareEncoder>> {
    warn!(
        width = cfg.width,
        height = cfg.height,
        "Intel QSV oneVPL is not linked in this build; use NVENC on NVIDIA hosts"
    );
    Err(EncodeError::Unavailable)
}

#[allow(dead_code)]
pub struct QsvEncoder {
    chroma: Chroma,
}

impl HardwareEncoder for QsvEncoder {
    fn encode(&mut self, _frame: &GpuFrame, _force_idr: bool) -> Result<Option<EncodedAu>> {
        Err(EncodeError::Unavailable)
    }

    fn set_bitrate(&mut self, _bps: u32) {}

    fn backend(&self) -> EncodeBackend {
        EncodeBackend::Qsv
    }

    fn chroma(&self) -> Chroma {
        self.chroma
    }
}
