use std::env;
use std::io;
use std::net::UdpSocket;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bytemuck;
use sound_send::packet::{
  Message, SampleFormat, SyncMessage, decode_message, encode_sync,
  respond_to_ping,
};
use sound_send::packet::{Meta, encode_packet};
use sound_send::rate::RollingRate;
use sound_send::send_stats::SendStats;
use sound_send::volume::VolumeMeter;

// 1024 bytes: every 2.67ms in 48kHz stereo f32
const MAX_PAYLOAD: usize = 1024; // payload only (excludes our header)
// Static asserts: ensure MAX_PAYLOAD aligns to all supported sample sizes
const PAYLOAD_ALIGNMENT: usize = 8;
const _: [(); MAX_PAYLOAD % PAYLOAD_ALIGNMENT] = [(); 0];

const UPDATE_INTERVAL: Duration = Duration::from_millis(200);
const STATS_WINDOW: Duration = Duration::from_secs(10);
const VOLUME_WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
  #[cfg(feature = "cpal")]
  Cpal,

  #[cfg(target_os = "windows")]
  WasapiLoopback,

  Stdin,
}

mod audio_sources;

use audio_sources::{InputOptions, InputSource, ProcessChunk, StdinInput};

fn build_input_source(input_mode: InputMode) -> Result<Box<dyn InputSource>> {
  match input_mode {
    #[cfg(feature = "cpal")]
    InputMode::Cpal => {
      use audio_sources::cpal::CpalInput;
      use cpal::traits::HostTrait;

      let host = cpal::default_host();
      let device = host
        .default_input_device()
        .context("no default input device found")?;
      Ok(Box::new(CpalInput::new(device)))
    }
    #[cfg(target_os = "windows")]
    InputMode::WasapiLoopback => {
      use audio_sources::WasapiInput;
      Ok(Box::new(WasapiInput::default()))
    },
    InputMode::Stdin => Ok(Box::new(StdinInput)),
  }
}

#[cfg(target_os = "windows")]
fn boost_current_thread_priority() {
  use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
  };

  unsafe {
    if let Err(err) =
      SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)
    {
      eprintln!("warning: failed to raise thread priority: {err}");
    }
  }
}

#[cfg(not(target_os = "windows"))]
fn boost_current_thread_priority() {}

#[cfg(target_os = "windows")]
fn boost_process_priority() {
  use windows::Win32::System::Threading::{
    GetCurrentProcess, HIGH_PRIORITY_CLASS, SetPriorityClass,
  };

  unsafe {
    if let Err(err) = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS)
    {
      eprintln!("warning: failed to raise process priority: {err}");
    }
  }
}

#[cfg(not(target_os = "windows"))]
fn boost_process_priority() {}

fn main() -> Result<()> {
  // --- 1. Parse args and set up socket ---
  let mut args = env::args().skip(1); // skip program name
  boost_process_priority();
  #[allow(unused_mut)]
  let mut input_mode: InputMode = {
    #[cfg(feature = "cpal")]
    {
      InputMode::Cpal
    }
    #[cfg(all(not(feature = "cpal"), target_os = "windows"))]
    {
      InputMode::WasapiLoopback
    }
    #[cfg(all(not(feature = "cpal"), not(target_os = "windows")))]
    {
      InputMode::Stdin
    }
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
          anyhow::anyhow!("--input requires a value: {}", input_mode_options())
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
      "missing destination. Usage: udp_sender <addr:port> [--input {}]",
      input_mode_options()
    )
  })?;

  // Create UDP socket (OS picks an ephemeral local port)
  let socket =
    UdpSocket::bind("0.0.0.0:0").context("failed to bind UDP socket")?;
  println!("Destination: {}", server_addr);

  let meter = Arc::new(Mutex::new(VolumeMeter::new(VOLUME_WINDOW)));

  // --- 2. Configure input source ---
  let input_options = InputOptions {
    channels: opt_channels,
    sample_rate: opt_sample_rate,
    format: opt_format,
  };
  let mut input_source = build_input_source(input_mode)?;
  input_source.validate_options(&input_options)?;
  let packet_meta = input_source.prepare_meta(&input_options)?;

  // --- 3. Move sending to a worker thread; main prints stats ---
  let (stats_tx, stats_rx) = mpsc::channel::<SendStats>();
  let send_sock = socket
    .try_clone()
    .context("failed to clone socket for sender thread")?;

  let mut worker: SendWorker = SendWorker::new(
    send_sock,
    server_addr.clone(),
    packet_meta,
    meter.clone(),
    stats_tx,
    STATS_WINDOW,
    UPDATE_INTERVAL,
  );

  let process_chunk: ProcessChunk =
    Box::new(move |audio_chunk: &[u8]| worker.process_chunk(audio_chunk));
  let _input_guard = input_source.start(&packet_meta, process_chunk)?;

  // Perform handshake: wait for a Pong reply before starting data send
  wait_for_pong_handshake(&socket, &server_addr)?;

  // Spawn responder to handle time-sync pings from receiver (after handshake)
  spawn_timesync_responder(&socket);

  // Make socket nonblocking for send/recv after handshake
  socket
    .set_nonblocking(true)
    .context("failed to set UDP socket nonblocking")?;

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
        "\rTotal: {:>7.2} MB | Last 10s avg: {:>7.2} KB/s | Pkts/s: {:>6.1} | \
         Vol1s: {:>6.1} dBFS   ",
        stats.total_bytes_sent as f64 / (1024.0 * 1024.0),
        stats.average_rate_bps / 1024.0,
        stats.average_packets_per_sec,
        db
      );
      let _ = io::stdout().flush();
    }
  }

  Ok(())
}

fn parse_input_mode(s: &str) -> Result<InputMode> {
  match s.to_ascii_lowercase().as_str() {
    #[cfg(feature = "cpal")]
    "cpal" => Ok(InputMode::Cpal),
    #[cfg(target_os = "windows")]
    "wasapi" | "loopback" => Ok(InputMode::WasapiLoopback),
    "stdin" => Ok(InputMode::Stdin),
    other => bail!(
      "invalid input mode: {} (expected: {})",
      other,
      input_mode_options()
    ),
  }
}

fn input_mode_options() -> &'static str {
  #[cfg(all(feature = "cpal", target_os = "windows"))]
  {
    "cpal|wasapi|stdin"
  }
  #[cfg(all(feature = "cpal", not(target_os = "windows")))]
  {
    "cpal|stdin"
  }
  #[cfg(all(not(feature = "cpal"), target_os = "windows"))]
  {
    "wasapi|stdin"
  }
  #[cfg(all(not(feature = "cpal"), not(target_os = "windows")))]
  {
    "stdin"
  }
}

fn default_input_mode_name() -> &'static str {
  #[cfg(feature = "cpal")]
  {
    "cpal"
  }
  #[cfg(all(not(feature = "cpal"), target_os = "windows"))]
  {
    "wasapi"
  }
  #[cfg(all(not(feature = "cpal"), not(target_os = "windows")))]
  {
    "stdin"
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
      if data.len() % 4 != 0 {
        return false;
      }
      let s: &[f32] = bytemuck::cast_slice(data);
      s.iter().all(|&v| v == 0.0)
    }
    SampleFormat::I16 => {
      if data.len() % 2 != 0 {
        return false;
      }
      let s: &[i16] = bytemuck::cast_slice(data);
      s.iter().all(|&v| v == 0)
    }
    SampleFormat::U16 => {
      if data.len() % 2 != 0 {
        return false;
      }
      let s: &[u16] = bytemuck::cast_slice(data);
      s.iter().all(|&v| v == 0x8000)
    }
    SampleFormat::U32 => {
      if data.len() % 4 != 0 {
        return false;
      }
      let s: &[u32] = bytemuck::cast_slice(data);
      s.iter().all(|&v| v == 0x8000_0000)
    }
    _ => false,
  }
}

struct SendWorker {
  send_sock: UdpSocket,
  server_addr: String,
  packet_meta: Meta,
  meter: Arc<Mutex<VolumeMeter>>,
  stats_tx: mpsc::Sender<SendStats>,
  total_bytes_sent: u64,
  sequence_number: u64,
  last_update_time: Instant,
  byte_rate: RollingRate,
  packet_rate: RollingRate,
  warned_sample_align: bool,
  prev_silent: bool,
  update_interval: Duration,
}

impl SendWorker {
  fn new(
    send_sock: UdpSocket,
    server_addr: String,
    packet_meta: Meta,
    meter: Arc<Mutex<VolumeMeter>>,
    stats_tx: mpsc::Sender<SendStats>,
    window: Duration,
    update_interval: Duration,
  ) -> Self {
    Self {
      send_sock,
      server_addr,
      packet_meta,
      meter,
      stats_tx,
      total_bytes_sent: 0,
      sequence_number: 0,
      last_update_time: Instant::now(),
      byte_rate: RollingRate::new(window),
      packet_rate: RollingRate::new(window),
      warned_sample_align: false,
      prev_silent: false,
      update_interval,
    }
  }

  fn process_chunk(&mut self, audio_chunk: &[u8]) -> Result<()> {
    let now_ts = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_else(|_| Duration::from_millis(0));
    let ts_ms = now_ts.as_millis() as u64;
    // Determine if this chunk is silence and collapse repeated silence
    let bps = bytes_per_sample(self.packet_meta.sample_format);
    let aligned = bps == 1 || (audio_chunk.len() % bps == 0);
    let is_silent =
      aligned && is_silent_chunk(self.packet_meta.sample_format, audio_chunk);
    let payload: &[u8] = if is_silent && self.prev_silent {
      &[]
    } else {
      audio_chunk
    };
    let send_buf =
      encode_packet(self.sequence_number, payload, self.packet_meta, ts_ms);
    self.prev_silent = is_silent;

    if self
      .send_sock
      .send_to(&send_buf, &self.server_addr)
      .is_err()
    {
      // Ignore send errors and continue (nonblocking)
    }

    {
      let mut guard = self.meter.lock().unwrap();
      let now = Instant::now();
      let bps = bytes_per_sample(self.packet_meta.sample_format);
      let aligned = bps == 1 || (audio_chunk.len() % bps == 0);
      if !aligned && !self.warned_sample_align {
        eprintln!(
          "warning: payload length {} is not a multiple of 1-sample ({} bytes)",
          audio_chunk.len(),
          bps
        );
        self.warned_sample_align = true;
      }
      if aligned {
        if self.packet_meta.sample_format == SampleFormat::F32 {
          let f: &[f32] = bytemuck::cast_slice(audio_chunk);
          guard.add_samples_f32(now, f);
        } else if self.packet_meta.sample_format == SampleFormat::I16 {
          let f: &[i16] = bytemuck::cast_slice(audio_chunk);
          guard.add_samples_i16(now, f);
        } else if self.packet_meta.sample_format == SampleFormat::U16 {
          let f: &[u16] = bytemuck::cast_slice(audio_chunk);
          guard.add_samples_u16(now, f);
        } else if self.packet_meta.sample_format == SampleFormat::U32 {
          let f: &[u32] = bytemuck::cast_slice(audio_chunk);
          guard.add_samples_u32(now, f);
        }
      }
    }

    let now = Instant::now();
    let sent_packet_size = send_buf.len();
    self.total_bytes_sent += sent_packet_size as u64;
    self.byte_rate.record(now, sent_packet_size as u64);
    self.packet_rate.record(now, 1);

    if now.duration_since(self.last_update_time) >= self.update_interval {
      let average_rate_bps = self.byte_rate.rate_per_sec(now);
      let average_packets_per_sec = self.packet_rate.rate_per_sec(now);
      let _ = self.stats_tx.send(SendStats {
        total_bytes_sent: self.total_bytes_sent,
        average_rate_bps,
        average_packets_per_sec,
      });
      self.last_update_time = now;
    }

    self.sequence_number = self.sequence_number.wrapping_add(1);

    Ok(())
  }
}

fn print_usage() {
  let input_modes = input_mode_options();
  let default_mode = default_input_mode_name();
  eprintln!(
    "Usage: udp_sender <server_addr:port> \
     [options]\nRequired:\n<server_addr:port>          Destination \
     address\nOptions:\n-i, --input <{input_modes}>    Input source (default: \
     {default_mode})\n-c, --channels <1..255>     Channels for stdin \
     (default: 2)\n-r, --rate <hz>             Sample rate for stdin \
     (default: 48000)\n-f, --format <f32|i16|u16|u32>  Sample format for \
     stdin (default: u32)\n-h, --help                  Show this help"
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

  std::thread::spawn(move || {
    loop {
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
    }
  });
}
