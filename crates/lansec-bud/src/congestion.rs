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
        Self {
            target_bps: 60_000_000,
            min_bps: 8_000_000,
            max_bps: 150_000_000,
            rtt_us_ewma: 1_000.0,
            loss_ppm_ewma: 0.0,
            in_flight_bytes: 0,
        }
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
        if self.loss_ppm_ewma > 5_000.0 {
            self.target_bps = (self.target_bps as f32 * 0.85) as u32;
        } else if self.loss_ppm_ewma < 200.0 && self.rtt_us_ewma < 8_000.0 {
            self.target_bps = (self.target_bps as f32 * 1.05) as u32;
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
