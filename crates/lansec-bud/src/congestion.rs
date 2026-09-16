use std::time::{Duration, Instant};

use lansec_protocol::CongestionReport;

/// Delay/loss based controller. Encoder bitrate is the congestion lever.
#[derive(Debug, Clone)]
pub struct CongestionController {
    pub target_bps: u32,
    pub min_bps: u32,
    pub max_bps: u32,
    rtt_us_ewma: f32,
    loss_ppm_ewma: f32,
    in_flight_bytes: u32,
    last_adjust: Option<Instant>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CongestionStats {
    pub rtt_us: u32,
    pub loss_ppm: u32,
    pub target_bps: u32,
    pub in_flight_bytes: u32,
}

impl CongestionController {
    pub fn lan_default() -> Self {
        Self::from_bps(50_000_000, 40_000_000, 100_000_000)
    }

    /// Build controller from absolute bitrates (bits/sec). Clamps target into [min, max].
    pub fn from_bps(target_bps: u32, min_bps: u32, max_bps: u32) -> Self {
        let min_bps = min_bps.max(1_000_000);
        let max_bps = max_bps.max(min_bps);
        let target_bps = target_bps.clamp(min_bps, max_bps);
        Self {
            target_bps,
            min_bps,
            max_bps,
            rtt_us_ewma: 1_000.0,
            loss_ppm_ewma: 0.0,
            in_flight_bytes: 0,
            last_adjust: None,
        }
    }

    /// Convenience: Mbps values from the GUI / SessionConfig.
    pub fn from_mbps(target_mbps: u32, min_mbps: u32, max_mbps: u32) -> Self {
        Self::from_bps(
            target_mbps.saturating_mul(1_000_000),
            min_mbps.saturating_mul(1_000_000),
            max_mbps.saturating_mul(1_000_000),
        )
    }

    pub fn on_rtt_sample(&mut self, rtt_us: u32) {
        let r = rtt_us as f32;
        self.rtt_us_ewma = if self.rtt_us_ewma == 0.0 {
            r
        } else {
            self.rtt_us_ewma * 0.8 + r * 0.2
        };
    }

    pub fn on_loss(&mut self, lost: u32, sent: u32) {
        if sent == 0 {
            return;
        }
        let ppm = (lost as u64 * 1_000_000 / sent as u64) as f32;
        self.loss_ppm_ewma = self.loss_ppm_ewma * 0.7 + ppm * 0.3;
        self.adjust();
    }

    fn adjust(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_adjust {
            if now.duration_since(prev) < Duration::from_millis(400) {
                return;
            }
        }
        if self.loss_ppm_ewma > 20_000.0 {
            self.target_bps = (self.target_bps as f32 * 0.92) as u32;
            self.last_adjust = Some(now);
        } else if self.loss_ppm_ewma < 1_000.0 && self.rtt_us_ewma < 8_000.0 {
            self.target_bps = (self.target_bps as f32 * 1.03) as u32;
            self.last_adjust = Some(now);
        }
        self.target_bps = self.target_bps.clamp(self.min_bps, self.max_bps);
    }

    pub fn set_in_flight(&mut self, bytes: u32) {
        self.in_flight_bytes = bytes;
    }

    pub fn stats(&self) -> CongestionStats {
        CongestionStats {
            rtt_us: self.rtt_us_ewma as u32,
            loss_ppm: self.loss_ppm_ewma as u32,
            target_bps: self.target_bps,
            in_flight_bytes: self.in_flight_bytes,
        }
    }

    pub fn report(&self) -> CongestionReport {
        let s = self.stats();
        CongestionReport {
            rtt_us: s.rtt_us,
            loss_ppm: s.loss_ppm,
            suggested_bitrate_bps: s.target_bps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_cuts_bitrate() {
        let mut cc = CongestionController::lan_default();
        let before = cc.target_bps;
        cc.on_loss(20, 100);
        assert!(cc.target_bps < before);
    }
}
