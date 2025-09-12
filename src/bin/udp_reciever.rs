use std::env;
use std::io::{self, Write};
use std::net::UdpSocket;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sound_send::rate::{RollingRate, RollingMean};
use sound_send::sync::{decode as sync_decode, encode as sync_encode, SyncMessage};
use sound_send::timesync::TimeSyncEstimator;
use sound_send::packet::decode_packet;
use std::process::{Command, Stdio};

fn main() -> io::Result<()> {
    // 1. Parse listening address and options
    let mut args = env::args();
    let prog = args.next().unwrap_or_else(|| "udp_reciever".into());
    let mut listen_addr: Option<String> = None;
    let mut use_pipewire = false;
    for arg in args {
        match arg.as_str() {
            "--pipewire" => use_pipewire = true,
            "-h" | "--help" => {
                eprintln!(
                    "Usage: {} <listen_addr:port> [--pipewire]",
                    prog
                );
                eprintln!("Example: {} 127.0.0.1:12345", prog);
                return Ok(());
            }
            s if s.starts_with('-') => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown flag: {}", s),
                ));
            }
            s => {
                if listen_addr.is_none() {
                    listen_addr = Some(s.to_string());
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unexpected argument: {}", s),
                    ));
                }
            }
        }
    }
    let listen_addr = listen_addr.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "missing listen address")
    })?;

    // 2. Bind UDP socket and start listening
    let socket = UdpSocket::bind(listen_addr)?;
    eprintln!("Listening on {} ...", socket.local_addr()?);

    // 3. Prepare receive buffer and statistics
    // UDP max payload is 65507 bytes, but typical MTU is ~1500
    // Use a buffer larger than the client's chunk size to be safe
    let mut buf = [0; 2048];
    let mut total_bytes_received: u64 = 0;
    let mut total_packets_received: u64 = 0;
    let mut expected_sequence: u64 = 0;
    let mut lost_packets: u64 = 0;
    let mut out_of_order_packets: u64 = 0;
    let mut last_update_time = Instant::now();
    const UPDATE_INTERVAL: Duration = Duration::from_millis(200); // stats update interval (0.2s)
    const WINDOW: Duration = Duration::from_secs(10);

    // Rolling byte rate over the last WINDOW
    let mut byte_rate = RollingRate::new(WINDOW);
    // Rolling latency mean (ms) over the last WINDOW
    let mut latency_mean = RollingMean::new(WINDOW);
    // Time sync estimator (offset/drift)
    let mut ts_est = TimeSyncEstimator::new(0.2, 0.2);
    let mut last_sender: Option<std::net::SocketAddr> = None;
    let mut last_ping_ms: u64 = 0;

    // Output sinks (stdout or PipeWire via pw-cat)
    let mut stdout = io::stdout().lock();
    let mut pw_stdin: Option<std::process::ChildStdin> = None;

    // 4. Receive loop
    loop {
        // Receive data; get byte count and source address
        let (bytes_received, src_addr) = socket.recv_from(&mut buf)?;

        // First, check for sync control message
        if let Some(msg) = sync_decode(&buf[..bytes_received]) {
            match msg {
                SyncMessage::Pong { t0_ms, t1_ms, t2_ms } => {
                    // t3 is now
                    let t3_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_else(|_| Duration::from_millis(0))
                        .as_millis() as u64;
                    let _state = ts_est.update(t0_ms, t1_ms, t2_ms, t3_ms);
                }
                _ => {}
            }
            continue;
        }

        // Decode audio packet (magic, length, meta, sequence, payload)
        let decoded = match decode_packet(&buf[..bytes_received]) {
            Ok(d) => d,
            Err(_) => continue,
        };
        last_sender = Some(src_addr);
        let received_sequence = decoded.seq;
        let payload = decoded.payload;
        let sent_ts_ms = decoded.timestamp_ms;

        total_bytes_received += bytes_received as u64;
        total_packets_received += 1;

        // Update rolling byte rate
        let now_inst = Instant::now();
        byte_rate.record(now_inst, payload.len() as u64);
        // Compute latency in ms using system clock; saturate at 0 if clock skew
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_millis(0))
            .as_millis() as u64;
        // Adjust latency by current offset estimate
        let offset_ms = ts_est.state().offset_ms;
        let adj_now_ms = (now_ms as i128 - offset_ms as i128).max(0) as u64;
        let latency_ms = adj_now_ms.saturating_sub(sent_ts_ms);
        latency_mean.record(now_inst, latency_ms as f64);

        // Check packet loss/order; write payload only for in-order packets
        if received_sequence == expected_sequence {
            // In-order packet: write payload
            if use_pipewire {
                if pw_stdin.is_none() {
                    let fmt = match decoded.meta.sample_format {
                        sound_send::packet::SampleFormat::F32 => "f32",
                        sound_send::packet::SampleFormat::I16 => "s16",
                        sound_send::packet::SampleFormat::U16 => "u16",
                        _ => "f32",
                    };
                    let rate = (decoded.meta.sample_rate.0).to_string();
                    let ch = decoded.meta.channels.to_string();
                    let mut child = Command::new("pw-cat")
                        .arg("--playback")
                        .arg("--rate")
                        .arg(rate)
                        .arg("--channels")
                        .arg(ch)
                        .arg("--format")
                        .arg(fmt)
                        .arg("-")
                        .stdin(Stdio::piped())
                        .spawn()?;
                    pw_stdin = child.stdin.take();
                }
                if let Some(stdin) = pw_stdin.as_mut() {
                    stdin.write_all(payload)?;
                }
            } else {
                stdout.write_all(payload)?;
            }
            expected_sequence += 1;
        } else if received_sequence > expected_sequence {
            // Some packets were lost.
            // This packet is in-order relative to its sequence; write payload
            if use_pipewire {
                if let Some(stdin) = pw_stdin.as_mut() {
                    stdin.write_all(payload)?;
                }
            } else {
                stdout.write_all(payload)?;
            }
            let lost_count = received_sequence - expected_sequence;
            lost_packets += lost_count;
            expected_sequence = received_sequence + 1;
        } else { // received_sequence < expected_sequence
            // Late/out-of-order packet: count it but do not write payload
            out_of_order_packets += 1;
        }

        // Update and print stats periodically
        let now = Instant::now();
        if now.duration_since(last_update_time) >= UPDATE_INTERVAL {
            // Rolling average over the last WINDOW seconds
            let bytes_per_sec = byte_rate.rate_per_sec(now);
            let average_rate_kbs = bytes_per_sec / 1024.0;
            let avg_latency_ms = latency_mean.average(now);
            let off = ts_est.state().offset_ms;
            let drift = ts_est.state().drift_ppm;

            // Print stats in a single line (carriage return to overwrite)
            let total_expected_packets = expected_sequence;
            let loss_percentage = if total_expected_packets > 0 {
                (lost_packets as f64 / total_expected_packets as f64) * 100.0
            } else {
                0.0
            };

            eprint!(
                "\rRecv: {} | Lost: {} ({:.2}%) | Late: {} | Total: {:.2} MB | \
                 Avg10s: {:.2} KB/s | Lat10s: {:.2} ms | Off: {:+.2} ms | \
                 Drift: {:+.1} ppm from {}   ",
                total_packets_received,
                lost_packets,
                loss_percentage,
                out_of_order_packets,
                total_bytes_received as f64 / (1024.0 * 1024.0),
                average_rate_kbs,
                avg_latency_ms,
                off,
                drift,
                src_addr
            );
            // Flush to stderr immediately
            io::stderr().flush()?;

            last_update_time = now;
            // Periodically send sync ping to the last sender
            if let Some(addr) = last_sender {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_else(|_| Duration::from_millis(0))
                    .as_millis() as u64;
                if now_ms.saturating_sub(last_ping_ms) >= 1_000 {
                    let ping = SyncMessage::Ping { t0_ms: now_ms };
                    let v = sync_encode(ping);
                    let _ = socket.send_to(&v, addr);
                    last_ping_ms = now_ms;
                }
            }
        }
    }
    // This loop is typically interrupted with Ctrl+C
}
