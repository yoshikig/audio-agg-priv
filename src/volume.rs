use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct VolumeMeter {
  window: Duration,
  history: VecDeque<(Instant, f64, usize)>,
  sum_sq: f64,
  count: usize,
}

impl VolumeMeter {
  pub fn new(window: Duration) -> Self {
    Self {
      window,
      history: VecDeque::new(),
      sum_sq: 0.0,
      count: 0,
    }
  }

  pub fn add_samples_f32(&mut self, now: Instant, data: &[f32]) {
    let sum_sq = data.iter().map(|&v| (v as f64) * (v as f64)).sum();
    self.push(now, sum_sq, data.len());
  }

  pub fn add_samples_i16(&mut self, now: Instant, data: &[i16]) {
    let norm = 32768.0f64;
    let sum_sq = data
      .iter()
      .map(|&v| {
        let x = (v as f64) / norm;
        x * x
      })
      .sum();
    self.push(now, sum_sq, data.len());
  }

  pub fn add_samples_u16(&mut self, now: Instant, data: &[u16]) {
    let center = 32768.0f64;
    let norm = 32768.0f64;
    let sum_sq = data
      .iter()
      .map(|&v| {
        let x = ((v as f64) - center) / norm;
        x * x
      })
      .sum();
    self.push(now, sum_sq, data.len());
  }

  pub fn add_samples_u32(&mut self, now: Instant, data: &[u32]) {
    let center = 2_147_483_648.0f64; // 2^31
    let norm = 2_147_483_648.0f64; // scale to approx [-1,1]
    let sum_sq = data
      .iter()
      .map(|&v| {
        let x = ((v as f64) - center) / norm;
        x * x
      })
      .sum();
    self.push(now, sum_sq, data.len());
  }

  fn push(&mut self, now: Instant, sum_sq: f64, n: usize) {
    self.history.push_back((now, sum_sq, n));
    self.sum_sq += sum_sq;
    self.count += n;
    self.prune(now);
  }

  fn prune(&mut self, now: Instant) {
    while let Some(&(t, s, n)) = self.history.front() {
      if now.duration_since(t) > self.window {
        self.sum_sq -= s;
        self.count -= n;
        self.history.pop_front();
      } else {
        break;
      }
    }
  }

  pub fn rms(&mut self, now: Instant) -> f64 {
    self.prune(now);
    if self.count == 0 {
      0.0
    } else {
      (self.sum_sq / self.count as f64).sqrt()
    }
  }

  pub fn dbfs(&mut self, now: Instant) -> f64 {
    let rms = self.rms(now);
    if rms <= 0.0 {
      -120.0
    } else {
      20.0 * rms.log10()
    }
  }
}
