use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Rolling window rate calculator.
/// Records timestamped counts and computes average rate per second
/// over the given window.
#[derive(Debug)]
pub struct RollingRate {
    window: Duration,
    history: VecDeque<(Instant, u64)>,
    sum: u64,
}

impl RollingRate {
    pub fn new(window: Duration) -> Self {
        Self { window, history: VecDeque::new(), sum: 0 }
    }

    pub fn record(&mut self, now: Instant, count: u64) {
        self.history.push_back((now, count));
        self.sum = self.sum.saturating_add(count);
        self.prune(now);
    }

    pub fn total_in_window(&mut self, now: Instant) -> u64 {
        self.prune(now);
        self.sum
    }

    pub fn rate_per_sec(&mut self, now: Instant) -> f64 {
        self.prune(now);
        if self.window.is_zero() { return 0.0; }
        self.sum as f64 / self.window.as_secs_f64()
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(t, c)) = self.history.front() {
            if now.duration_since(t) > self.window {
                self.sum = self.sum.saturating_sub(c);
                self.history.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_packet_rate() {
        let base = Instant::now();
        let mut r = RollingRate::new(Duration::from_secs(10));
        for i in 0..10u64 {
            let t = base.checked_add(Duration::from_secs(i)).unwrap();
            r.record(t, 1);
        }
        let now = base.checked_add(Duration::from_secs(10)).unwrap();
        let rate = r.rate_per_sec(now);
        assert!((rate - 1.0).abs() < 1e-9, "rate was {rate}");
    }

    #[test]
    fn pruning_works() {
        let base = Instant::now();
        let mut r = RollingRate::new(Duration::from_secs(5));
        r.record(base, 10);
        let now = base.checked_add(Duration::from_secs(6)).unwrap();
        assert_eq!(r.total_in_window(now), 0);
        assert_eq!(r.rate_per_sec(now), 0.0);
    }

    #[test]
    fn byte_rate_example() {
        let base = Instant::now();
        let mut r = RollingRate::new(Duration::from_secs(10));
        for i in 0..10u64 {
            let t = base.checked_add(Duration::from_secs(i)).unwrap();
            r.record(t, 100);
        }
        let now = base.checked_add(Duration::from_secs(10)).unwrap();
        let rate = r.rate_per_sec(now);
        assert!((rate - 100.0).abs() < 1e-9, "rate was {rate}");
    }
}

