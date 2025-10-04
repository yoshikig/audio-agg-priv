use std::env;
use std::io::{self, Read};
use std::net::UdpSocket;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bytemuck;
use sound_send::packet::{
  Message, SampleFormat, SampleRate, SyncMessage, decode_message, encode_sync,
  respond_to_ping,
};
use sound_send::packet::{Meta, encode_packet};
use sound_send::rate::RollingRate;
use sound_send::send_stats::SendStats;
use sound_send::volume::VolumeMeter;

const MAX_PAYLOAD: usize = 1024 + 256; // payload only (excludes our header)
// Static asserts: ensure MAX_PAYLOAD aligns to all supported sample sizes
const _: [(); MAX_PAYLOAD % 2] = [(); 0];
const _: [(); MAX_PAYLOAD % 4] = [(); 0];

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

#[cfg(feature = "cpal")]
type Stream = cpal::Stream;
#[cfg(not(feature = "cpal"))]
type Stream = ();

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

#[cfg(feature = "cpal")]
fn generate_cpal_stream(
  device: &cpal::Device,
  sample_format: SampleFormat,
  process_chunk: impl FnMut(&[u8]) -> Result<()> + Send + 'static,
) -> Result<Stream> {
  use cpal::traits::{DeviceTrait, StreamTrait};

  let supported_config = device
    .default_input_config()
    .context("failed to get default input config")?;
  let config = supported_config.config();

  let stream: cpal::Stream = match sample_format {
    SampleFormat::F32 => {
      build_cpal_input_stream::<f32>(&device, &config, process_chunk)?
    }
    SampleFormat::I16 => {
      build_cpal_input_stream::<i16>(&device, &config, process_chunk)?
    }
    SampleFormat::U16 => {
      build_cpal_input_stream::<u16>(&device, &config, process_chunk)?
    }
    other => bail!("unsupported sample format: {:?}", other),
  };
  stream.play().context("failed to start input stream")?;

  Ok(stream)
}

#[cfg(feature = "cpal")]
fn generate_cpal_meta(device: &cpal::Device) -> Result<Meta> {
  use cpal::traits::DeviceTrait;

  // Metadata to include in each packet
  let mut packet_meta = Meta {
    channels: 0,
    sample_rate: SampleRate(0),
    sample_format: SampleFormat::F32,
  };

  println!("Input: CPAL (default audio input)");
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

  Ok(packet_meta)
}

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
  let _maybe_stream: Option<Stream>; // keep stream alive when in CPAL mode
  // Metadata to include in each packet
  let mut packet_meta = Meta {
    channels: 0,
    sample_rate: SampleRate(0),
    sample_format: SampleFormat::F32,
  };

  #[cfg(target_os = "windows")]
  let mut wasapi_config: Option<wasapi_loopback::LoopbackConfig> = None;

  // --- 3. Move sending to a worker thread; main prints stats ---
  let (stats_tx, stats_rx) = mpsc::channel::<SendStats>();
  let send_sock = socket
    .try_clone()
    .context("failed to clone socket for sender thread")?;

  #[cfg(feature = "cpal")]
  let device = match input_mode {
    InputMode::Cpal => {
      use cpal::traits::HostTrait;

      let host = cpal::default_host();
      let device = host
        .default_input_device()
        .context("no default input device found")?;
      Some(device)
    }
    _ => None,
  };

  match input_mode {
    #[cfg(feature = "cpal")]
    InputMode::Cpal => {
      if opt_channels.is_some()
        || opt_sample_rate.is_some()
        || opt_format.is_some()
      {
        bail!("--channels/--rate/--format are only valid with --input stdin");
      }

      packet_meta = generate_cpal_meta(device.as_ref().unwrap())?;
    }
    #[cfg(target_os = "windows")]
    InputMode::WasapiLoopback => {
      if opt_channels.is_some()
        || opt_sample_rate.is_some()
        || opt_format.is_some()
      {
        bail!("--channels/--rate/--format are only valid with --input stdin");
      }

      let (meta, config) = wasapi_loopback::prepare_loopback()?;
      packet_meta = meta;
      wasapi_config = Some(config);
    }
    InputMode::Stdin => {
      // Fill packet_meta from CLI flags with defaults if missing
      packet_meta.channels = opt_channels.unwrap_or(2);
      packet_meta.sample_rate = SampleRate(opt_sample_rate.unwrap_or(48_000));
      packet_meta.sample_format = opt_format.unwrap_or(SampleFormat::U32);
    }
  }

  let mut worker: SendWorker = SendWorker::new(
    send_sock,
    server_addr.clone(),
    packet_meta,
    meter.clone(),
    stats_tx,
    STATS_WINDOW,
    UPDATE_INTERVAL,
  );

  let mut process_chunk = move |audio_chunk: &[u8]| -> Result<()> {
    worker.process_chunk(audio_chunk)
  };

  match input_mode {
    #[cfg(feature = "cpal")]
    InputMode::Cpal => {
      let stream = generate_cpal_stream(
        device.as_ref().unwrap(),
        packet_meta.sample_format,
        process_chunk,
      )?;
      _maybe_stream = Some(stream);
    }

    #[cfg(target_os = "windows")]
    InputMode::WasapiLoopback => {
      println!("Input: WASAPI loopback (default render mix)");
      let config = wasapi_config
        .take()
        .expect("wasapi configuration missing before capture start");

      wasapi_loopback::spawn_loopback_capture(config, process_chunk)?;
      _maybe_stream = None;
    }

    InputMode::Stdin => {
      println!("Input: stdin (reading raw bytes)");
      std::thread::spawn(move || {
        boost_current_thread_priority();
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; MAX_PAYLOAD];
        loop {
          match stdin.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
              // Send exactly the bytes read
              if process_chunk(&buf[..n]).is_err() {
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

#[cfg(feature = "cpal")]
fn build_cpal_input_stream<T>(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  mut process_chunk: impl FnMut(&[u8]) -> Result<()> + Send + 'static,
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
        if process_chunk(chunk).is_err() {
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

#[cfg(target_os = "windows")]
mod wasapi_loopback {
  use std::cmp;
  use std::thread;

  use anyhow::Context;
  use wasapi::{
    AudioCaptureClient, Direction, SampleType, StreamMode, WasapiError,
    WaveFormat, deinitialize, get_default_device, initialize_mta,
  };

  use super::{MAX_PAYLOAD, Meta, Result, SampleFormat, SampleRate};

  pub(super) struct LoopbackConfig {
    format: WaveFormat,
  }

  struct ComGuard;

  impl ComGuard {
    fn init_mta() -> Result<Self> {
      initialize_mta()
        .ok()
        .context("failed to initialize COM for WASAPI loopback")?;
      Ok(Self)
    }
  }

  impl Drop for ComGuard {
    fn drop(&mut self) {
      deinitialize();
    }
  }

  pub(super) fn prepare_loopback() -> Result<(Meta, LoopbackConfig)> {
    let _com = ComGuard::init_mta()?;
    let device = get_default_device(&Direction::Render)
      .context("no default render device for loopback")?;
    let audio_client = device
      .get_iaudioclient()
      .context("failed to get IAudioClient for loopback")?;
    let mix_format = audio_client
      .get_mixformat()
      .context("failed to query mix format for loopback")?;

    let sample_rate = mix_format.get_samplespersec();
    let channels = mix_format.get_nchannels();
    let channel_mask = mix_format.get_dwchannelmask();

    let desired_format = WaveFormat::new(
      32,
      32,
      &SampleType::Float,
      sample_rate as usize,
      channels as usize,
      Some(channel_mask),
    );

    let meta = Meta {
      channels: channels.min(255) as u8,
      sample_rate: SampleRate(sample_rate),
      sample_format: SampleFormat::F32,
    };

    Ok((
      meta,
      LoopbackConfig {
        format: desired_format,
      },
    ))
  }

  pub(super) fn spawn_loopback_capture<F>(
    config: LoopbackConfig,
    process_chunk: F,
  ) -> Result<()>
  where
    F: FnMut(&[u8]) -> Result<()> + Send + 'static,
  {
    eprintln!("Channels: {:?}", config.format.get_nchannels());
    eprintln!("Sample Rate: {:?}", config.format.get_samplespersec());
    eprintln!("Sample Bits: {:?}", config.format.get_bitspersample());
    eprintln!("Sample Format: {:?}", config.format.get_subformat());
    eprintln!("Block Align: {:?}", config.format.get_blockalign());

    thread::Builder::new()
      .name("wasapi-loopback".to_string())
      .spawn(move || {
        super::boost_current_thread_priority();
        if let Err(err) = run_loopback_capture(config.format, process_chunk) {
          eprintln!("WASAPI loopback capture error: {err:?}");
        }
      })
      .context("failed to spawn WASAPI loopback thread")?;
    Ok(())
  }

  fn run_loopback_capture<F>(
    format: WaveFormat,
    mut process_chunk: F,
  ) -> Result<()>
  where
    F: FnMut(&[u8]) -> Result<()>,
  {
    let _com = ComGuard::init_mta()?;
    let device = get_default_device(&Direction::Render)
      .context("no default render device for loopback")?;
    let mut audio_client = device
      .get_iaudioclient()
      .context("failed to get IAudioClient for loopback")?;
    let (_default_period, min_period) = audio_client
      .get_device_period()
      .context("failed to query device period for loopback")?;

    let stream_mode = StreamMode::EventsShared {
      autoconvert: true,
      buffer_duration_hns: min_period,
    };

    audio_client
      .initialize_client(&format, &Direction::Capture, &stream_mode)
      .context("failed to initialize WASAPI loopback client")?;

    let event = audio_client
      .set_get_eventhandle()
      .context("failed to create loopback event handle")?;
    let capture_client = audio_client
      .get_audiocaptureclient()
      .context("failed to get AudioCaptureClient for loopback")?;

    let start_result = audio_client.start_stream();
    if let Err(err) = start_result {
      let _ = audio_client.stop_stream();
      return Err(err).context("failed to start WASAPI loopback stream");
    }

    let frame_bytes = format.get_blockalign() as usize;
    let frames_per_chunk = cmp::max(1, MAX_PAYLOAD / frame_bytes);
    let chunk_stride = frames_per_chunk * frame_bytes;

    let run_result = loop {
      if let Err(err) = drain_packets(
        &capture_client,
        chunk_stride,
        frame_bytes,
        &mut process_chunk,
      ) {
        break Err(err);
      }
      match event.wait_for_event(2000) {
        Ok(()) => {}
        Err(WasapiError::EventTimeout) => continue,
        Err(other) => break Err(other.into()),
      }
    };

    let _ = audio_client.stop_stream();
    run_result
  }

  fn drain_packets<F>(
    capture_client: &AudioCaptureClient,
    chunk_stride: usize,
    frame_bytes: usize,
    process_chunk: &mut F,
  ) -> Result<()>
  where
    F: FnMut(&[u8]) -> Result<()>,
  {
    loop {
      let packet = capture_client
        .get_next_packet_size()
        .context("failed to query next packet size")?;
      let Some(frames) = packet else { break };
      if frames == 0 {
        break;
      }

      let mut buffer = vec![0u8; frames as usize * frame_bytes];
      let (frames_read, info) = capture_client
        .read_from_device(&mut buffer)
        .context("failed to read loopback packet")?;

      let used = frames_read as usize * frame_bytes;
      if used == 0 {
        continue;
      }

      if info.flags.silent {
        buffer[..used].fill(0);
      }

      let stride = cmp::max(chunk_stride, frame_bytes);
      let mut offset = 0;
      while offset < used {
        let end = (offset + stride).min(used);
        process_chunk(&buffer[offset..end])?;
        offset = end;
      }
    }

    Ok(())
  }
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
