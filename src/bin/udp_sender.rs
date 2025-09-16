use anyhow::{bail, Context, Result};
use bytemuck;
use sound_send::packet::{
  decode_message, encode_sync, respond_to_ping, Message, SampleFormat,
  SampleRate, SyncMessage,
};
use sound_send::packet::{encode_packet, Meta};
use sound_send::rate::RollingRate;
use sound_send::send_stats::SendStats;
use sound_send::volume::VolumeMeter;
use std::env;
use std::io::{self, Read};
use std::net::UdpSocket;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thread_priority::*;

const MAX_PAYLOAD: usize = 1024 + 256; // payload only (excludes our header)
                                       // Static asserts: ensure MAX_PAYLOAD aligns to all supported sample sizes
const _: [(); MAX_PAYLOAD % 2] = [(); 0];
const _: [(); MAX_PAYLOAD % 4] = [(); 0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
  Cpal,
  Stdin,
}

#[cfg(feature = "cpal")]
type Stream = cpal::Stream;
#[cfg(not(feature = "cpal"))]
type Stream = ();

#[cfg(feature = "cpal")]
fn generate_cpal_stream(tx: &mpsc::Sender<Vec<u8>>) -> Result<(Stream, Meta)> {
  use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

  // Metadata to include in each packet
  let mut packet_meta = Meta {
    channels: 0,
    sample_rate: SampleRate(0),
    sample_format: SampleFormat::F32,
  };

  println!("Input: CPAL (default audio input)");
  let host = cpal::default_host();
  let device = host
    .default_input_device()
    .context("no default input device found")?;

  let supported_config = device
    .default_input_config()
    .context("failed to get default input config")?;

  let config = supported_config.config();
  eprintln!("Device: {:?}", device.name().ok());
  eprintln!(
    "  Sample Format: {:?}\n  Sample Rate: {} Hz\n  Channels: {}",
    supported_config.sample_format(),
    config.sample_rate.0,
    config.channels
  );

  // Build metadata (1 byte each)
  packet_meta.channels = config.channels.min(255) as u8;
  packet_meta.sample_rate = config.sample_rate.into();
  packet_meta.sample_format = match supported_config.sample_format() {
    cpal::SampleFormat::F32 => SampleFormat::F32,
    cpal::SampleFormat::I16 => SampleFormat::I16,
    cpal::SampleFormat::U16 => SampleFormat::U16,
    _ => SampleFormat::Unknown,
  };

  let stream: cpal::Stream = match supported_config.sample_format() {
    cpal::SampleFormat::F32 => {
      build_cpal_input_stream::<f32>(&device, &config, tx.clone())?
    }
    cpal::SampleFormat::I16 => {
      build_cpal_input_stream::<i16>(&device, &config, tx.clone())?
    }
    cpal::SampleFormat::U16 => {
      build_cpal_input_stream::<u16>(&device, &config, tx.clone())?
    }
    other => bail!("unsupported sample format: {:?}", other),
  };
  stream.play().context("failed to start input stream")?;

  Ok((stream, packet_meta))
}

fn main() -> Result<()> {
  // --- 1. Parse args and set up socket ---
  let mut args = env::args().skip(1); // skip program name
  let mut input_mode = if cfg!(feature = "cpal") {
    InputMode::Cpal
  } else {
    InputMode::Stdin
  };
  let mut server_addr: Option<String> = None;
  let mut show_status_icon = false;
  // stdin metadata options
  let mut opt_channels: Option<u8> = None;
  let mut opt_sample_rate: Option<u32> = None;
  let mut opt_format: Option<SampleFormat> = None;

  while let Some(arg) = args.next() {
    match arg.as_str() {
      "-h" | "--help" => {
        print_usage();
        return Ok(());
      }
      "-s" | "--status-icon" => {
        show_status_icon = true;
      }
      "-c" | "--channels" => {
        let val = args
          .next()
          .ok_or_else(|| anyhow::anyhow!("--channels requires a value"))?;
        let n: u16 = val.parse().context("invalid --channels value")?;
        if n == 0 || n > 255 {
          bail!("--channels must be 1..=255");
        }
        opt_channels = Some(n as u8);
      }
      _ if arg.starts_with("--channels=") => {
        let val = &arg[11..];
        let n: u16 = val.parse().context("invalid --channels value")?;
        if n == 0 || n > 255 {
          bail!("--channels must be 1..=255");
        }
        opt_channels = Some(n as u8);
      }
      "-r" | "--rate" | "--sample-rate" => {
        let val = args.next().ok_or_else(|| {
          anyhow::anyhow!("--rate requires a value (e.g., 48000)")
        })?;
        let sr: u32 = val.parse().context("invalid --rate value")?;
        opt_sample_rate = Some(sr);
      }
      _ if arg.starts_with("--rate=") => {
        let val = &arg[7..];
        let sr: u32 = val.parse().context("invalid --rate value")?;
        opt_sample_rate = Some(sr);
      }
      _ if arg.starts_with("--sample-rate=") => {
        let val = &arg[14..];
        let sr: u32 = val.parse().context("invalid --sample-rate value")?;
        opt_sample_rate = Some(sr);
      }
      "-f" | "--format" => {
        let val = args.next().ok_or_else(|| {
          anyhow::anyhow!("--format requires a value (f32|i16|u16|u32)")
        })?;
        opt_format = Some(parse_sample_format(&val)?);
      }
      _ if arg.starts_with("--format=") => {
        let val = &arg[9..];
        opt_format = Some(parse_sample_format(val)?);
      }
      "-i" | "--input" => {
        let val = args.next().ok_or_else(|| {
          anyhow::anyhow!("--input requires a value: cpal|stdin")
        })?;
        input_mode = parse_input_mode(&val)?;
      }
      _ if arg.starts_with("--input=") => {
        let val = &arg[8..];
        input_mode = parse_input_mode(val)?;
      }
      s if s.starts_with('-') => {
        bail!("unknown flag: {}", s);
      }
      s => {
        if server_addr.is_none() {
          server_addr = Some(s.to_string());
        } else {
          bail!("unexpected argument: {}", s);
        }
      }
    }
  }
  let server_addr = server_addr.ok_or_else(|| {
    anyhow::anyhow!(
      "missing destination. Usage: udp_sender <addr:port> [--input cpal|stdin]"
    )
  })?;

  // Create UDP socket (OS picks an ephemeral local port)
  let socket =
    UdpSocket::bind("0.0.0.0:0").context("failed to bind UDP socket")?;
  println!("Destination: {}", server_addr);

  // Channel used to pass input chunks to the main thread
  let (tx, rx) = mpsc::channel::<Vec<u8>>();
  let meter = Arc::new(Mutex::new(VolumeMeter::new(VOLUME_WINDOW)));

  // --- 2. Configure input source ---
  let _maybe_stream: Option<Stream>; // keep stream alive when in CPAL mode
                                     // Metadata to include in each packet
  let mut packet_meta = Meta {
    channels: 0,
    sample_rate: SampleRate(0),
    sample_format: SampleFormat::F32,
  };
  match input_mode {
    InputMode::Cpal => {
      #[cfg(feature = "cpal")]
      {
        if opt_channels.is_some()
          || opt_sample_rate.is_some()
          || opt_format.is_some()
        {
          bail!("--channels/--rate/--format are only valid with --input stdin");
        }
        let (stream, meta) = generate_cpal_stream(&tx)?;
        packet_meta = meta;
        _maybe_stream = Some(stream);
      }
      #[cfg(not(feature = "cpal"))]
      {
        _maybe_stream = None;
        bail!("CPAL feature not enabled in this build");
      }
    }
    InputMode::Stdin => {
      println!("Input: stdin (reading raw bytes)");
      // Fill packet_meta from CLI flags with defaults if missing
      packet_meta.channels = opt_channels.unwrap_or(2);
      packet_meta.sample_rate = SampleRate(opt_sample_rate.unwrap_or(48_000));
      packet_meta.sample_format = opt_format.unwrap_or(SampleFormat::U32);
      std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; MAX_PAYLOAD];
        loop {
          match stdin.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
              // Send exactly the bytes read
              if tx.send(buf[..n].to_vec()).is_err() {
                break;
              }
            }
            Err(_) => break,
          }
        }
      });
      _maybe_stream = None;
    }
  }

  // Perform handshake: wait for a Pong reply before starting data send
  wait_for_pong_handshake(&socket, &server_addr)?;

  // Make socket nonblocking for send/recv after handshake
  socket
    .set_nonblocking(true)
    .context("failed to set UDP socket nonblocking")?;

  // Spawn responder to handle time-sync pings from receiver (after handshake)
  spawn_timesync_responder(&socket);

  // --- 3. Move sending to a worker thread; main prints stats ---
  const UPDATE_INTERVAL: Duration = Duration::from_millis(200);
  const WINDOW: Duration = Duration::from_secs(10);
  const VOLUME_WINDOW: Duration = Duration::from_secs(1);

  let (stats_tx, stats_rx) = mpsc::channel::<SendStats>();
  let send_sock = socket
    .try_clone()
    .context("failed to clone socket for sender thread")?;
  let server_addr_cloned = server_addr.clone();

  let meter_cloned = meter.clone();
  let _thread = ThreadBuilder::default()
    .name("AudioSender")
    .priority(ThreadPriority::Max)
    .spawn_careless(move || {
      let mut total_bytes_sent: u64 = 0;
      let mut sequence_number: u64 = 0;
      let mut last_update_time = Instant::now();
      let mut byte_rate = RollingRate::new(WINDOW);
      let mut warned_sample_align = false;

      let mut prev_silent = false;
      for audio_chunk in rx {
        let now_ts = SystemTime::now()
          .duration_since(UNIX_EPOCH)
          .unwrap_or_else(|_| Duration::from_millis(0));
        let ts_ms = now_ts.as_millis() as u64;
        // Determine if this chunk is silence and collapse repeated silence
        let bps = bytes_per_sample(packet_meta.sample_format);
        let aligned = bps == 1 || (audio_chunk.len() % bps == 0);
        let is_silent = aligned && is_silent_chunk(packet_meta.sample_format, &audio_chunk);
        let payload: &[u8] = if is_silent && prev_silent { &[] } else { &audio_chunk };
        let send_buf = encode_packet(sequence_number, payload, packet_meta, ts_ms);
        prev_silent = is_silent;

        if send_sock.send_to(&send_buf, &server_addr_cloned).is_err() {
          // Ignore send errors and continue
        }

        {
          let mut guard = meter_cloned.lock().unwrap();
          let now = Instant::now();
          let bps = bytes_per_sample(packet_meta.sample_format);
          let aligned = bps == 1 || (audio_chunk.len() % bps == 0);
          if !aligned && !warned_sample_align {
            eprintln!(
              "warning: payload length {} is not a multiple of 1-sample ({} bytes)",
              audio_chunk.len(), bps
            );
            warned_sample_align = true;
          }
          if aligned {
            if packet_meta.sample_format == SampleFormat::F32 {
              let f: &[f32] = bytemuck::cast_slice(&audio_chunk);
              guard.add_samples_f32(now, f);
            } else if packet_meta.sample_format == SampleFormat::I16 {
              let f: &[i16] = bytemuck::cast_slice(&audio_chunk);
              guard.add_samples_i16(now, f);
            } else if packet_meta.sample_format == SampleFormat::U16 {
              let f: &[u16] = bytemuck::cast_slice(&audio_chunk);
              guard.add_samples_u16(now, f);
            } else if packet_meta.sample_format == SampleFormat::U32 {
              let f: &[u32] = bytemuck::cast_slice(&audio_chunk);
              guard.add_samples_u32(now, f);
            }
          }
        }

        let now = Instant::now();
        let sent_packet_size = send_buf.len();
        total_bytes_sent += sent_packet_size as u64;
        byte_rate.record(now, sent_packet_size as u64);

        if now.duration_since(last_update_time) >= UPDATE_INTERVAL {
          let average_rate_bps = byte_rate.rate_per_sec(now);
          let _ = stats_tx.send(SendStats {
            total_bytes_sent,
            average_rate_bps,
          });
          last_update_time = now;
        }

        sequence_number = sequence_number.wrapping_add(1);
      }
      // Drop the stats channel to signal completion
      drop(stats_tx);
    });

  // --- 4. Show status icon on macOS, or print stats on other OSes ---
  // On macOS, spawn a status icon in the main thread and let it run there
  // On other OSes, print stats in the main thread
  if show_status_icon {
    #[cfg(target_os = "macos")]
    {
      println!("Sending started.");
      sound_send::status_icon_mac::show_status_icon(stats_rx);
    }

    if !cfg!(target_os = "macos") {
      bail!("Status icon is only supported on macOS.");
    }
  } else {
    use std::io::Write;

    println!("Sending started. Press Ctrl+C to stop.");

    // Main thread: receive stats and render
    while let Ok(stats) = stats_rx.recv() {
      let now: Instant = Instant::now();
      let db = meter.lock().unwrap().dbfs(now);
      print!(
                "\rTotal: {:>7.2} MB | Last 10s avg: {:>7.2} KB/s | Vol1s: {:>6.1} dBFS   ",
                stats.total_bytes_sent as f64 / (1024.0 * 1024.0),
                stats.average_rate_bps / 1024.0,
                db
            );
      let _ = io::stdout().flush();
    }
  }

  Ok(())
}

#[cfg(feature = "cpal")]
fn build_cpal_input_stream<T>(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  tx: mpsc::Sender<Vec<u8>>,
) -> Result<cpal::Stream>
where
  T: cpal::Sample + cpal::SizedSample + bytemuck::Pod + bytemuck::Zeroable,
{
  use cpal::traits::DeviceTrait;

  // Cast &[T] -> &[u8] safely via bytemuck
  let err_fn = |err| eprintln!("input stream error: {err}");

  let stream = device.build_input_stream(
    config,
    move |data: &[T], _| {
      // Data is interleaved. Send in reasonably small chunks.
      // For now, split the current callback buffer into UDP-sized chunks.
      let bytes: &[u8] = bytemuck::cast_slice(data);
      // Split to avoid exceeding typical MTU when adding our ~24-byte header
      let mut offset = 0;
      while offset < bytes.len() {
        let end = (offset + MAX_PAYLOAD).min(bytes.len());
        let chunk = &bytes[offset..end];
        if tx.send(chunk.to_vec()).is_err() {
          break;
        }
        offset = end;
      }
    },
    err_fn,
    None,
  )?;
  Ok(stream)
}

fn parse_input_mode(s: &str) -> Result<InputMode> {
  match s.to_ascii_lowercase().as_str() {
    "cpal" => Ok(InputMode::Cpal),
    "stdin" => Ok(InputMode::Stdin),
    _ => bail!("invalid input mode: {} (expected: cpal|stdin)", s),
  }
}

fn parse_sample_format(s: &str) -> Result<SampleFormat> {
  match s.to_ascii_lowercase().as_str() {
    "f32" => Ok(SampleFormat::F32),
    "i16" => Ok(SampleFormat::I16),
    "u16" => Ok(SampleFormat::U16),
    "u32" => Ok(SampleFormat::U32),
    other => bail!(
      "invalid sample format: {} (expected: f32|i16|u16|u32)",
      other
    ),
  }
}

fn bytes_per_sample(fmt: SampleFormat) -> usize {
  match fmt {
    SampleFormat::F32 => 4,
    SampleFormat::I16 => 2,
    SampleFormat::U16 => 2,
    SampleFormat::U32 => 4,
    _ => 1,
  }
}

fn is_silent_chunk(fmt: SampleFormat, data: &[u8]) -> bool {
  match fmt {
    SampleFormat::F32 => {
      if data.len() % 4 != 0 { return false; }
      let s: &[f32] = bytemuck::cast_slice(data);
      s.iter().all(|&v| v == 0.0)
    }
    SampleFormat::I16 => {
      if data.len() % 2 != 0 { return false; }
      let s: &[i16] = bytemuck::cast_slice(data);
      s.iter().all(|&v| v == 0)
    }
    SampleFormat::U16 => {
      if data.len() % 2 != 0 { return false; }
      let s: &[u16] = bytemuck::cast_slice(data);
      s.iter().all(|&v| v == 0x8000)
    }
    SampleFormat::U32 => {
      if data.len() % 4 != 0 { return false; }
      let s: &[u32] = bytemuck::cast_slice(data);
      s.iter().all(|&v| v == 0x8000_0000)
    }
    _ => false,
  }
}

fn print_usage() {
  eprintln!(
        "Usage: udp_sender <server_addr:port> [options]\n\
         Required:\n\
           <server_addr:port>          Destination address\n\
         Options:\n\
           -i, --input <cpal|stdin>    Input source (default: cpal)\n\
           -c, --channels <1..255>     Channels for stdin (default: 2)\n\
           -r, --rate <hz>             Sample rate for stdin (default: 48000)\n\
           -f, --format <f32|i16|u16|u32>  Sample format for stdin (default: u32)\n\
           -h, --help                  Show this help"
    );
}

fn wait_for_pong_handshake(
  socket: &UdpSocket,
  server_addr: &str,
) -> Result<()> {
  // Temporarily set a read timeout for handshake retries
  let original_timeout = socket.read_timeout().unwrap_or(None);
  socket.set_read_timeout(Some(Duration::from_millis(500)))?;

  // Send Ping and wait for corresponding Pong
  // Try a few times before giving up
  const MAX_ATTEMPTS: usize = 20; // ~10 seconds total
  for attempt in 1..=MAX_ATTEMPTS {
    let now = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_else(|_| Duration::from_millis(0))
      .as_millis() as u64;
    let ping = SyncMessage::Ping { t0_ms: now };
    let v = encode_sync(&ping);
    let _ = socket.send_to(&v, server_addr);

    let mut buf = [0u8; 128];
    match socket.recv_from(&mut buf) {
      Ok((n, _addr)) => {
        if let Ok(Message::Sync(SyncMessage::Pong { t0_ms, .. })) =
          decode_message(&buf[..n])
        {
          if t0_ms == now {
            // Matched our ping; handshake complete
            println!("Handshake complete: received Pong (attempt {attempt})");
            // Restore timeout before returning
            socket.set_read_timeout(original_timeout)?;
            return Ok(());
          }
        }
        // Not a matching pong; continue trying within this attempt window
      }
      Err(ref e)
        if e.kind() == std::io::ErrorKind::WouldBlock
          || e.kind() == std::io::ErrorKind::TimedOut =>
      {
        // Timed out; try next attempt
      }
      Err(e) => {
        // Unexpected error; restore timeout and propagate
        socket.set_read_timeout(original_timeout)?;
        return Err(e).context("handshake recv failed");
      }
    }
  }

  // Restore timeout before failing
  socket.set_read_timeout(original_timeout)?;
  bail!("failed to complete ping/pong handshake with receiver");
}

fn spawn_timesync_responder(socket: &UdpSocket) {
  let ts_sock = socket
    .try_clone()
    .expect("failed to clone udp socket for timesync");

  std::thread::spawn(move || loop {
    let mut buf = [0u8; 64];
    match ts_sock.recv_from(&mut buf) {
      Ok((n, addr)) => {
        if let Ok(Message::Sync(SyncMessage::Ping { t0_ms })) =
          decode_message(&buf[..n])
        {
          respond_to_ping(&ts_sock, addr, t0_ms);
        }
      }
      Err(ref e)
        if e.kind() == std::io::ErrorKind::WouldBlock
          || e.kind() == std::io::ErrorKind::TimedOut =>
      {
        // Nonblocking poll; back off briefly
        std::thread::sleep(std::time::Duration::from_millis(2));
        continue;
      }
      Err(_) => break,
    }
  });
}
