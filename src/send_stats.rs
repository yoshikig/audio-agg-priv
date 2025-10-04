#[derive(Debug, Clone, Copy)]
pub struct SendStats {
  pub total_bytes_sent: u64,
  pub average_rate_bps: f64,
  pub average_packets_per_sec: f64,
}
