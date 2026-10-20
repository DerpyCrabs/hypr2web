use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub const MAX_IN_FLIGHT: usize = 2;
#[derive(Default)]
pub struct Flow {
    pending: VecDeque<(u32, Instant)>,
    pub presented_roundtrip_ms: f64,
    pub acknowledged: u64,
}
impl Flow {
    pub fn available(&self) -> bool {
        self.pending.len() < MAX_IN_FLIGHT
    }
    pub fn sent(&mut self, seq: u32, at: Instant) {
        self.pending.push_back((seq, at));
    }
    pub fn ack(&mut self, seq: u32) -> bool {
        let Some(index) = self.pending.iter().position(|(id, _)| *id == seq) else {
            return false;
        };
        let (_, at) = self.pending[index];
        self.presented_roundtrip_ms = at.elapsed().as_secs_f64() * 1000.;
        self.acknowledged += (index + 1) as u64;
        self.pending.drain(..=index);
        true
    }
    pub fn stalled(&self) -> bool {
        self.pending
            .front()
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(3))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_window_and_cumulative_ack() {
        let mut f = Flow::default();
        f.sent(1, Instant::now());
        f.sent(2, Instant::now());
        assert!(!f.available());
        assert!(!f.ack(99));
        assert!(!f.available());
        assert!(f.ack(2));
        assert!(f.available());
        assert_eq!(f.acknowledged, 2);
        assert!(!f.ack(1));
    }
}
