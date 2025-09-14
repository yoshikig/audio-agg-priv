use crate::rate::{RollingMean, RollingRate};
use crate::sync_controller::{DefaultSyncController, SyncController};
use crate::volume::VolumeMeter;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

// Collects, computes and prints rolling statistics for the receiver.
pub struct RecvStats {
    total_bytes_received: u64,
    total_packets_received: u64,
    lost_packets: u64,
    out_of_order_packets: u64,
    byte_rate: RollingRate,
    latency_mean: RollingMean,
    sync: DefaultSyncController,
    pub volume: VolumeMeter,
}

impl RecvStats {
    pub fn new(window: Duration, volume_window: Duration, sync: DefaultSyncController) -> Self {
        Self {
            total_bytes_received: 0,
            total_packets_received: 0,
            lost_packets: 0,
            out_of_order_packets: 0,
            byte_rate: RollingRate::new(window),
            latency_mean: RollingMean::new(window),
            sync,
            volume: VolumeMeter::new(volume_window),
        }
    }

    pub fn on_packet(
        &mut self,
        bytes_received: usize,
        payload_len: usize,
        latency_ms: f64,
        now: Instant,
    ) {
        self.total_bytes_received += bytes_received as u64;
        self.total_packets_received += 1;
        self.byte_rate.record(now, payload_len as u64);
        self.latency_mean.record(now, latency_ms);
    }

    pub fn mark_lost(&mut self, lost_count: u64) {
        self.lost_packets += lost_count;
    }

    pub fn mark_out_of_order(&mut self) { self.out_of_order_packets += 1; }

    pub fn format_status_line(
        &mut self,
        now: Instant,
        expected_sequence: u64,
        src_addr: &SocketAddr,
        offset_ms: f64,
        drift_ppm: f64,
    ) -> String {
        let bytes_per_sec = self.byte_rate.rate_per_sec(now);
        let average_rate_kbs = bytes_per_sec / 1024.0;
        let avg_latency_ms = self.latency_mean.average(now);
        let db = self.volume.dbfs(now);
        let total_expected_packets = expected_sequence;
        let loss_percentage = if total_expected_packets > 0 {
            (self.lost_packets as f64 / total_expected_packets as f64) * 100.0
        } else {
            0.0
        };
        let total_mb = self.total_bytes_received as f64 / (1024.0 * 1024.0);

        format!(
            "\r[{}] Recv: {} | Lost: {} ({:.2}%) | Late: {} | Total: {:.2} MB | \
             Avg10s: {:.2} KB/s | Lat10s: {:.2} ms | Vol10s: {:>6.1} dBFS | \
             Off: {:+.2} ms | Drift: {:+.1} ppm   ",
            src_addr,
            self.total_packets_received,
            self.lost_packets,
            loss_percentage,
            self.out_of_order_packets,
            total_mb,
            average_rate_kbs,
            avg_latency_ms,
            db,
            offset_ms,
            drift_ppm,
        )
    }

    // Lightweight wrappers to access sync controller from main
    pub fn register_sender(&mut self, addr: SocketAddr) {
        self.sync.register_sender(addr);
    }
    pub fn on_pong(&mut self, t0_ms: u64, t1_ms: u64, t2_ms: u64) {
        self.sync.on_pong(t0_ms, t1_ms, t2_ms);
    }
    pub fn compute_latency_ms(&self, sent_ts_ms: u64) -> f64 {
        self.sync.compute_latency_ms(sent_ts_ms)
    }
    pub fn maybe_ping(&mut self, sock: &UdpSocket) {
        self.sync.maybe_send_ping(sock)
    }

    pub fn offset_ms(&self) -> f64 { self.sync.offset_ms() }
    pub fn drift_ppm(&self) -> f64 { self.sync.drift_ppm() }
}
