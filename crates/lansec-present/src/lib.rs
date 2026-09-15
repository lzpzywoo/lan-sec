use lansec_decode::DecodedFrame;
use lansec_protocol::{FrameTimes, FrameTimingPolicy, SessionClock};

#[derive(Debug, thiserror::Error)]
pub enum PresentError {
    #[error("{0}")]
    Message(String),
}

pub struct PresentStats {
    pub presented: u64,
    pub dropped: u64,
    pub last: FrameTimes,
}

pub struct Presenter {
    policy: FrameTimingPolicy,
    clock: SessionClock,
    pending: Option<DecodedFrame>,
    stats: PresentStats,
}

impl Presenter {
    pub fn new(vsync_hz: f32) -> Self {
        Self {
            policy: FrameTimingPolicy { vsync_hz },
            clock: SessionClock::new(),
            pending: None,
            stats: PresentStats {
                presented: 0,
                dropped: 0,
                last: FrameTimes::default(),
            },
        }
    }

    /// Queue at most one decoded frame. Late frames are dropped.
    pub fn submit(&mut self, frame: DecodedFrame, mut times: FrameTimes) {
        let now = self.clock.now_us();
        if self.policy.should_drop(frame.decode_done_us, now) {
            self.stats.dropped += 1;
            tracing::debug!(dropped = self.stats.dropped, "drop late frame");
            return;
        }
        if self.pending.is_some() {
            self.stats.dropped += 1;
        }
        times.decode_done_us = frame.decode_done_us;
        self.pending = Some(frame);
        self.stats.last = times;
    }

    pub fn take(&mut self) -> Option<DecodedFrame> {
        if let Some(frame) = self.pending.take() {
            self.stats.presented += 1;
            self.stats.last.present_us = self.clock.now_us();
            if self.stats.presented % 120 == 1 {
                tracing::info!(
                    capture_encode_ms = self.stats.last.capture_to_encode_ms(),
                    net_ms = self.stats.last.net_ms(),
                    decode_ms = self.stats.last.decode_ms(),
                    glass_ms = self.stats.last.glass_ms(),
                    presented = self.stats.presented,
                    dropped = self.stats.dropped,
                    "frame timing"
                );
            }
            Some(frame)
        } else {
            None
        }
    }

    pub fn stats(&self) -> &PresentStats {
        &self.stats
    }

    pub fn last_times(&self) -> FrameTimes {
        self.stats.last
    }
}

#[cfg(windows)]
pub mod windows_swapchain;
#[cfg(target_os = "macos")]
pub mod macos_metal;
