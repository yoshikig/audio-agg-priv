use sound_send::packet::{
  decode_message, respond_to_ping, Message, SyncMessage,
};
use sound_send::payload_sink::BinarySink;
use sound_send::recv_stats::RecvStats;
use sound_send::sync_controller::DefaultSyncController;
use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::net::UdpSocket;
use std::time::{Duration, Instant};
// no local process spawning; handled by payload_sink

// RecvStats moved to sound_send::recv_stats

// Sync controller moved to sound_send::sync_controller

fn main() -> io::Result<()> {
  // 1. Parse listening address and options
  let mut args = env::args();
  let prog = args.next().unwrap_or_else(|| "udp_reciever".into());
  let mut listen_addr: Option<String> = None;
  let mut use_pipewire = false;
  let mut show_progress = false;
  for arg in args {
    match arg.as_str() {
      "--pipewire" => use_pipewire = true,
      "--progress" => show_progress = true,
      "-h" | "--help" => {
        eprintln!("Usage: {} <listen_addr:port> [--pipewire]", prog);
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
  // stats update interval (0.2s)
  const UPDATE_INTERVAL: Duration = Duration::from_millis(200);
  const WINDOW: Duration = Duration::from_secs(10);
  const VOLUME_WINDOW: Duration = Duration::from_secs(1);

  // Per-client context: sink + stats + expected seq + last seen time
  struct ClientCtx {
    sink: BinarySink,
    stats: RecvStats,
    expected_seq: u64,
    last_seen: Instant,
  }

  let mut clients: HashMap<std::net::SocketAddr, ClientCtx> = HashMap::new();
  const SINK_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

  // Render state for multi-line display
  let mut rendered_lines: usize = 0;
  let mut last_render = Instant::now();
  // Hide cursor for smoother refresh
  eprint!("\x1b[?25l");

  // 4. Receive loop
  loop {
    // Receive data; get byte count and source address
    let (bytes_received, src_addr) = socket.recv_from(&mut buf)?;

    // Decode control or audio packet in a unified match
    let ctx = clients.entry(src_addr).or_insert_with(|| ClientCtx {
      sink: BinarySink::new(use_pipewire),
      stats: RecvStats::new(
        WINDOW,
        VOLUME_WINDOW,
        DefaultSyncController::with_default_estimator(0.2, 0.2, 1_000),
      ),
      expected_seq: 0,
      last_seen: Instant::now(),
    });
    ctx.stats.register_sender(src_addr);

    let data = &buf[..bytes_received];
    match decode_message(data) {
      Ok(Message::Sync(SyncMessage::Pong {
        t0_ms,
        t1_ms,
        t2_ms,
      })) => {
        ctx.stats.on_pong(t0_ms, t1_ms, t2_ms);
      }
      Ok(Message::Sync(SyncMessage::Ping { t0_ms })) => {
        respond_to_ping(&socket, src_addr, t0_ms);
      }
      Ok(Message::Data(decoded)) => {
        let received_sequence = decoded.seq;
        let payload = decoded.payload;
        let sent_ts_ms = decoded.timestamp_ms;

        // Update rolling byte rate, latency, and volume
        let now_inst = Instant::now();
        let latency_ms = ctx.stats.compute_latency_ms(sent_ts_ms);
        ctx.stats.on_packet(
          bytes_received,
          payload.len(),
          latency_ms,
          now_inst,
        );
        match decoded.meta.sample_format {
          sound_send::packet::SampleFormat::F32 => {
            let samples: &[f32] = unsafe {
              std::slice::from_raw_parts(
                payload.as_ptr() as *const f32,
                payload.len() / 4,
              )
            };
            ctx.stats.volume.add_samples_f32(now_inst, samples);
          }
          sound_send::packet::SampleFormat::I16 => {
            let mut v = Vec::with_capacity(payload.len() / 2);
            for b in payload.chunks_exact(2) {
              v.push(i16::from_ne_bytes([b[0], b[1]]));
            }
            ctx.stats.volume.add_samples_i16(now_inst, &v);
          }
          sound_send::packet::SampleFormat::U16 => {
            let mut v = Vec::with_capacity(payload.len() / 2);
            for b in payload.chunks_exact(2) {
              v.push(u16::from_ne_bytes([b[0], b[1]]));
            }
            ctx.stats.volume.add_samples_u16(now_inst, &v);
          }
          sound_send::packet::SampleFormat::U32 => {
            let mut v = Vec::with_capacity(payload.len() / 4);
            for b in payload.chunks_exact(4) {
              v.push(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
            }
            ctx.stats.volume.add_samples_u32(now_inst, &v);
          }
          _ => {}
        }

        // Check packet loss/order; write payload only for in-order packets
        if received_sequence == ctx.expected_seq {
          // In-order packet: write payload to the client-specific sink
          ctx.sink.process(&decoded.meta, payload)?;
          ctx.expected_seq = ctx.expected_seq.wrapping_add(1);
        } else if received_sequence > ctx.expected_seq {
          // Some packets were lost.
          // This packet is in-order relative to its sequence; write it
          ctx.sink.process(&decoded.meta, payload)?;
          // Do not count initial gap as loss if this is the
          // first packet observed for this client
          if ctx.expected_seq != 0 {
            let lost_count = received_sequence - ctx.expected_seq;
            ctx.stats.mark_lost(lost_count);
          }
          ctx.expected_seq = received_sequence + 1;
        } else {
          // received_sequence < expected_sequence
          // Late/out-of-order packet: count it but do not write payload
          ctx.stats.mark_out_of_order();
        }
      }
      Err(_) => {
        // Unknown payload; skip
        continue;
      }
    }

    // Update and print stats periodically
    let now = Instant::now();
    ctx.last_seen = now;

    // Close and remove clients that have been idle for too long
    clients
      .retain(|_, ctx| now.duration_since(ctx.last_seen) < SINK_IDLE_TIMEOUT);

    // Trigger pings independent of rendering
    for ctx in clients.values_mut() {
      ctx.stats.maybe_ping(&socket);
    }

    if show_progress && now.duration_since(last_render) >= UPDATE_INTERVAL {
      // Deterministic order by address
      let mut addrs: Vec<_> = clients.keys().cloned().collect();
      addrs.sort_by_key(|a| (a.ip().to_string(), a.port()));

      // Move cursor up to the start of the previous block
      if rendered_lines > 0 {
        eprint!("\x1b[{}A", rendered_lines);
      }

      // Render each client's line and maybe send ping
      let mut printed = 0usize;
      for addr in addrs.iter() {
        if let Some(ctx) = clients.get_mut(addr) {
          let line = ctx.stats.format_status_line(
            now,
            ctx.expected_seq,
            addr,
            ctx.stats.offset_ms(),
            ctx.stats.drift_ppm(),
          );
          // Clear line and print
          eprint!("\r\x1b[2K{}\n", line);
          printed += 1;
        }
      }

      // If fewer lines than before, clear the remaining old lines
      for _ in printed..rendered_lines {
        eprint!("\r\x1b[2K\n");
      }
      io::stderr().flush()?;
      rendered_lines = printed;
      last_render = now;
    }
  }
  // This loop is typically interrupted with Ctrl+C
}
