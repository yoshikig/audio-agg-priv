use crate::packet::{encode_sync, SyncMessage};
use crate::timesync::TimeSync;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) trait SyncController {
    fn register_sender(&mut self, addr: SocketAddr);
    fn on_pong(&mut self, t0_ms: u64, t1_ms: u64, t2_ms: u64);
    fn compute_latency_ms(&self, sent_ts_ms: u64) -> f64;
    fn offset_ms(&self) -> f64;
    fn drift_ppm(&self) -> f64;
    fn maybe_send_ping(&mut self, sock: &UdpSocket);
}

pub struct DefaultSyncController {
    ts: Box<dyn TimeSync>,
    last_sender: Option<SocketAddr>,
    last_ping_ms: u64,
    ping_interval_ms: u64,
}

impl DefaultSyncController {
    pub fn new(ts: Box<dyn TimeSync>, ping_interval_ms: u64) -> Self {
        Self { ts, last_sender: None, last_ping_ms: 0, ping_interval_ms }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_millis(0))
            .as_millis() as u64
    }

    /// Convenience: build with the default estimator
    pub fn with_default_estimator(alpha: f64, beta: f64, ping_interval_ms: u64) -> Self {
        Self::new(Box::new(crate::timesync::TimeSyncEstimator::new(alpha, beta)), ping_interval_ms)
    }
}

impl SyncController for DefaultSyncController {
    fn register_sender(&mut self, addr: SocketAddr) { self.last_sender = Some(addr); }

    fn on_pong(&mut self, t0_ms: u64, t1_ms: u64, t2_ms: u64) {
        let t3_ms = Self::now_ms();
        let _ = self.ts.update(t0_ms, t1_ms, t2_ms, t3_ms);
    }

    fn compute_latency_ms(&self, sent_ts_ms: u64) -> f64 {
        let now_ms = Self::now_ms();
        let offset = self.ts.state().offset_ms;
        let adj_now_ms = (now_ms as i128 - offset as i128).max(0) as u64;
        adj_now_ms.saturating_sub(sent_ts_ms) as f64
    }

    fn offset_ms(&self) -> f64 { self.ts.state().offset_ms }
    fn drift_ppm(&self) -> f64 { self.ts.state().drift_ppm }

    fn maybe_send_ping(&mut self, sock: &UdpSocket) {
        if let Some(addr) = self.last_sender {
            let now_ms = Self::now_ms();
            if now_ms.saturating_sub(self.last_ping_ms) >= self.ping_interval_ms {
                let ping = SyncMessage::Ping { t0_ms: now_ms };
                let v = encode_sync(&ping);
                let _ = sock.send_to(&v, addr);
                self.last_ping_ms = now_ms;
            }
        }
    }
}

