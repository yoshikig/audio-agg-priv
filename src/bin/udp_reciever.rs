use std::env;
use std::io;
use std::net::UdpSocket;
use std::time::{Duration, Instant};
use sound_send::packet::{decode_message, Message, SyncMessage};
use sound_send::payload_sink::BinarySink;
use sound_send::sync_controller::DefaultSyncController;
use sound_send::recv_stats::RecvStats;
// no local process spawning; handled by payload_sink

// RecvStats moved to sound_send::recv_stats

// Sync controller moved to sound_send::sync_controller

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
    let mut expected_sequence: u64 = 0;
    const UPDATE_INTERVAL: Duration = Duration::from_millis(200); // stats update interval (0.2s)
    const WINDOW: Duration = Duration::from_secs(10);
    let sync = DefaultSyncController::with_default_estimator(0.2, 0.2, 1_000);
    let mut stats = RecvStats::new(WINDOW, UPDATE_INTERVAL, sync);

    // Output sink (stdout or PipeWire via pw-cat)
    let mut sink = BinarySink::new(use_pipewire);

    // 4. Receive loop
    loop {
        // Receive data; get byte count and source address
        let (bytes_received, src_addr) = socket.recv_from(&mut buf)?;

        // Decode control or audio packet in a unified match
        let data = &buf[..bytes_received];
        match decode_message(data) {
            Ok(Message::Sync(SyncMessage::Pong { t0_ms, t1_ms, t2_ms })) => {
                stats.on_pong(t0_ms, t1_ms, t2_ms);
            }
            Ok(Message::Sync(SyncMessage::Ping { .. })) => {
                // Ignore pings on receiver side
            }
            Ok(Message::Data(decoded)) => {
                stats.register_sender(src_addr);
                let received_sequence = decoded.seq;
                let payload = decoded.payload;
                let sent_ts_ms = decoded.timestamp_ms;

                // Update rolling byte rate and latency
                let now_inst = Instant::now();
                let latency_ms = stats.compute_latency_ms(sent_ts_ms);
                stats.on_packet(bytes_received, payload.len(), latency_ms, now_inst);

                // Check packet loss/order; write payload only for in-order packets
                if received_sequence == expected_sequence {
                    // In-order packet: write payload
                    sink.process(&decoded.meta, payload)?;
                    expected_sequence += 1;
                } else if received_sequence > expected_sequence {
                    // Some packets were lost.
                    // This packet is in-order relative to its sequence; write payload
                    sink.process(&decoded.meta, payload)?;
                    let lost_count = received_sequence - expected_sequence;
                    stats.mark_lost(lost_count);
                    expected_sequence = received_sequence + 1;
                } else { // received_sequence < expected_sequence
                    // Late/out-of-order packet: count it but do not write payload
                    stats.mark_out_of_order();
                }
            }
            Err(_) => {
                // Unknown payload; skip
                continue;
            }
        }

        // Update and print stats periodically
        let now = Instant::now();
        let printed = stats.maybe_print(now, expected_sequence, &src_addr)?;
        if printed {
            // Periodically send sync ping to the last sender
            stats.maybe_send_ping(&socket);
        }
    }
    // This loop is typically interrupted with Ctrl+C
}
