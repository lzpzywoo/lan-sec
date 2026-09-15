use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Monotonic timestamps in microseconds, origin = host capture Instant mapped to u64.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTimes {
    pub capture_us: u64,
    pub encode_done_us: u64,
    pub send_us: u64,
    pub recv_us: u64,
    pub decode_done_us: u64,
    pub present_us: u64,
}

impl FrameTimes {
    pub fn stage_ms(&self, from: u64, to: u64) -> f32 {
        if to <= from {
            0.0
        } else {
            (to - from) as f32 / 1000.0
        }
    }

    pub fn capture_to_encode_ms(&self) -> f32 {
        self.stage_ms(self.capture_us, self.encode_done_us)
    }

    pub fn encode_to_send_ms(&self) -> f32 {
        self.stage_ms(self.encode_done_us, self.send_us)
    }

    pub fn net_ms(&self) -> f32 {
        self.stage_ms(self.send_us, self.recv_us)
    }

    pub fn decode_ms(&self) -> f32 {
        self.stage_ms(self.recv_us, self.decode_done_us)
    }

    pub fn present_queue_ms(&self) -> f32 {
        self.stage_ms(self.decode_done_us, self.present_us)
    }

    pub fn glass_ms(&self) -> f32 {
        self.stage_ms(self.capture_us, self.present_us)
    }
}

/// Maps Instant to a session-relative microsecond clock.
#[derive(Debug, Clone, Copy)]
pub struct SessionClock {
    origin: Instant,
}

impl SessionClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    pub fn now_us(&self) -> u64 {
        self.origin.elapsed().as_micros() as u64
    }
}

impl Default for SessionClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Client presentation policy: never hold more than one decoded frame, drop if late.
#[derive(Debug, Clone, Copy)]
pub struct FrameTimingPolicy {
    pub vsync_hz: f32,
}

impl Default for FrameTimingPolicy {
    fn default() -> Self {
        Self { vsync_hz: 60.0 }
    }
}

impl FrameTimingPolicy {
    pub fn frame_budget_us(&self) -> u64 {
        (1_000_000.0 / self.vsync_hz.max(1.0)) as u64
    }

    /// Drop if the frame would miss the next vsync by more than one full period.
    pub fn should_drop(&self, decode_done_us: u64, now_us: u64) -> bool {
        now_us.saturating_sub(decode_done_us) > self.frame_budget_us()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_late_frames() {
        let policy = FrameTimingPolicy { vsync_hz: 60.0 };
        assert!(!policy.should_drop(0, 8_000));
        assert!(policy.should_drop(0, 20_000));
    }
}
