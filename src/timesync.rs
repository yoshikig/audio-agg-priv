// Time synchronization estimator: offset (ms) and drift (ppm).

#[derive(Debug, Default, Clone, Copy)]
pub struct TimeSyncState {
  pub offset_ms: f64,
  pub delay_ms: f64,
  pub drift_ppm: f64,
}

pub trait TimeSync {
  fn update(
    &mut self,
    t0_ms: u64,
    t1_ms: u64,
    t2_ms: u64,
    t3_ms: u64,
  ) -> TimeSyncState;
  fn state(&self) -> TimeSyncState;
}

#[derive(Debug)]
pub struct TimeSyncEstimator {
  alpha: f64,
  beta: f64,
  last_offset_ms: Option<f64>,
  last_t3_ms: Option<u64>,
  state: TimeSyncState,
}

impl TimeSyncEstimator {
  pub fn new(alpha: f64, beta: f64) -> Self {
    Self {
      alpha,
      beta,
      last_offset_ms: None,
      last_t3_ms: None,
      state: Default::default(),
    }
  }

  // NTP-like: t0=client send, t1=server recv, t2=server send, t3=client recv
  pub fn update(
    &mut self,
    t0_ms: u64,
    t1_ms: u64,
    t2_ms: u64,
    t3_ms: u64,
  ) -> TimeSyncState {
    let t0 = t0_ms as f64;
    let t1 = t1_ms as f64;
    let t2 = t2_ms as f64;
    let t3 = t3_ms as f64;

    let delay = (t3 - t0) - (t2 - t1);
    let offset = ((t1 - t0) + (t2 - t3)) / 2.0;

    // EWMA for offset/delay
    let a = self.alpha;
    if self.last_offset_ms.is_none() {
      self.state.offset_ms = offset;
      self.state.delay_ms = delay.max(0.0);
    } else {
      self.state.offset_ms = (1.0 - a) * self.state.offset_ms + a * offset;
      self.state.delay_ms =
        (1.0 - a) * self.state.delay_ms + a * delay.max(0.0);
    }

    // Drift as change in offset over change in t3
    if let (Some(prev_off), Some(prev_t3)) =
      (self.last_offset_ms, self.last_t3_ms)
    {
      let dt = (t3_ms - prev_t3) as f64;
      if dt > 0.0 {
        let doff = offset - prev_off;
        let ppm = (doff / dt) * 1_000_000.0;
        let b = self.beta;
        self.state.drift_ppm = (1.0 - b) * self.state.drift_ppm + b * ppm;
      }
    }

    self.last_offset_ms = Some(offset);
    self.last_t3_ms = Some(t3_ms);
    self.state
  }

  pub fn state(&self) -> TimeSyncState {
    self.state
  }
}

impl TimeSync for TimeSyncEstimator {
  fn update(
    &mut self,
    t0_ms: u64,
    t1_ms: u64,
    t2_ms: u64,
    t3_ms: u64,
  ) -> TimeSyncState {
    TimeSyncEstimator::update(self, t0_ms, t1_ms, t2_ms, t3_ms)
  }
  fn state(&self) -> TimeSyncState {
    TimeSyncEstimator::state(self)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn zero_drift_zero_offset() {
    let mut est = TimeSyncEstimator::new(0.2, 0.2);
    // perfect round-trip of 20ms, zero offset
    let s = est.update(1000, 1010, 1010, 1020);
    assert!((s.offset_ms - 0.0).abs() < 1e-9);
    assert!((s.delay_ms - 20.0).abs() < 1e-9);
  }

  #[test]
  fn positive_offset() {
    let mut est = TimeSyncEstimator::new(0.5, 0.5);
    // server clock ahead by 5ms
    let _ = est.update(1000, 1015, 1015, 1020);
    assert!(est.state().offset_ms > 0.0);
  }
}
