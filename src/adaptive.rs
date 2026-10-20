use serde::Deserialize;
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Feedback {
    pub decode_ms: f64,
    pub jitter_ms: f64,
    pub loss: f64,
    pub rtt_ms: f64,
}
#[derive(Debug)]
pub struct Adaptive {
    pub bitrate: u32,
    pub width: u32,
    pub fps: u32,
    cap: u32,
    enabled: bool,
    slow: u8,
    good: u8,
    cooldown: u8,
}
impl Adaptive {
    pub fn new(config: crate::StreamConfig) -> Self {
        Self {
            bitrate: config.bitrate,
            width: config.width,
            fps: config.fps,
            cap: config.bitrate,
            enabled: config.adaptive,
            slow: 0,
            good: 0,
            cooldown: 0,
        }
    }
    pub fn feedback(&mut self, f: Feedback) {
        if !self.enabled
            || [f.decode_ms, f.jitter_ms, f.loss, f.rtt_ms]
                .iter()
                .any(|n| !n.is_finite() || *n < 0.)
        {
            return;
        }
        // Called at most once per second; reduce quickly, recover slowly.
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return;
        }
        if f.loss > 0.02 || f.jitter_ms > 60. || f.rtt_ms > 100. {
            self.bitrate = (self.bitrate * 3 / 4).max(2_000_000);
            self.good = 0;
            self.cooldown = 2;
        } else if f.loss < 0.005 && f.jitter_ms < 30. && f.rtt_ms < 50. {
            self.good += 1;
            if self.good >= 5 {
                self.bitrate = (self.bitrate + self.bitrate / 20).min(self.cap);
                self.good = 0;
            }
        } else {
            self.good = 0;
        }
        if f.decode_ms > 1000. / self.fps as f64 * 0.9 {
            self.slow += 1;
        } else {
            self.slow = 0;
        }
        if self.slow >= 3 {
            if self.width > 1920 {
                self.width = if self.width > 2560 { 2560 } else { 1920 };
            } else {
                self.fps = 30;
            }
            self.slow = 0;
            self.cooldown = 3;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backs_off_and_recovers_within_user_cap() {
        let mut a = Adaptive::new(crate::StreamConfig::default());
        a.feedback(Feedback {
            loss: 0.1,
            ..Default::default()
        });
        assert_eq!(a.bitrate, 12_000_000);
        for _ in 0..100 {
            a.feedback(Feedback::default());
        }
        assert_eq!(a.bitrate, 16_000_000);
    }
    #[test]
    fn slow_decoder_reduces_resolution_and_invalid_feedback_is_ignored() {
        let mut a = Adaptive::new(crate::StreamConfig {
            width: 3840,
            ..Default::default()
        });
        a.feedback(Feedback {
            decode_ms: f64::NAN,
            ..Default::default()
        });
        for _ in 0..3 {
            a.feedback(Feedback {
                decode_ms: 30.,
                ..Default::default()
            });
        }
        assert_eq!(a.width, 2560);
        let mut fixed = Adaptive::new(crate::StreamConfig {
            adaptive: false,
            ..Default::default()
        });
        fixed.feedback(Feedback {
            loss: 1.,
            ..Default::default()
        });
        assert_eq!(fixed.bitrate, 16_000_000);
    }
}
