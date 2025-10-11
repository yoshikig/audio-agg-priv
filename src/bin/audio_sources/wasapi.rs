use std::thread;

use anyhow::{Context, Result, bail};
use sound_send::packet::{Meta, SampleFormat, SampleRate};
use wasapi::{
  AudioCaptureClient, Direction, SampleType, StreamMode, WasapiError,
  WaveFormat, deinitialize, get_default_device, initialize_mta,
};

use super::{InputOptions, InputSource, ProcessChunk};
use crate::{MAX_PAYLOAD, PAYLOAD_ALIGNMENT};

#[derive(Default)]
pub struct WasapiInput {
  config: Option<LoopbackConfig>,
}

impl InputSource for WasapiInput {
  fn validate_options(&self, opts: &InputOptions) -> Result<()> {
    if opts.channels.is_some()
      || opts.sample_rate.is_some()
      || opts.format.is_some()
    {
      bail!("--channels/--rate/--format are only valid with --input stdin");
    }
    Ok(())
  }

  fn prepare_meta(&mut self, _opts: &InputOptions) -> Result<Meta> {
    let (meta, config) = prepare_loopback()?;
    self.config = Some(config);
    Ok(meta)
  }

  fn start(
    &mut self,
    _meta: &Meta,
    process_chunk: ProcessChunk,
  ) -> Result<()> {
    println!("Input: WASAPI loopback (default render mix)");
    let config = self
      .config
      .take()
      .expect("wasapi configuration missing before capture start");
    spawn_loopback_capture(config, process_chunk)?;
    Ok(())
  }
}

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

pub(super) fn spawn_loopback_capture(
  config: LoopbackConfig,
  process_chunk: ProcessChunk,
) -> Result<()> {
  eprintln!("Channels: {:?}", config.format.get_nchannels());
  eprintln!("Sample Rate: {:?}", config.format.get_samplespersec());
  eprintln!("Sample Bits: {:?}", config.format.get_bitspersample());
  eprintln!("Sample Format: {:?}", config.format.get_subformat());
  eprintln!("Block Align: {:?}", config.format.get_blockalign());

  thread::Builder::new()
    .name("wasapi-loopback".to_string())
    .spawn(move || {
      crate::boost_current_thread_priority();
      let mut chunker = process_chunk;
      if let Err(err) = run_loopback_capture(config.format, &mut chunker) {
        eprintln!("WASAPI loopback capture error: {err:?}");
      }
    })
    .context("failed to spawn WASAPI loopback thread")?;
  Ok(())
}

fn run_loopback_capture(
  format: WaveFormat,
  process_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
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
  assert!(PAYLOAD_ALIGNMENT % frame_bytes == 0);
  assert!(MAX_PAYLOAD % frame_bytes == 0);

  let run_result = loop {
    if let Err(err) =
      drain_packets(&capture_client, MAX_PAYLOAD, frame_bytes, process_chunk)
    {
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

fn drain_packets(
  capture_client: &AudioCaptureClient,
  chunk_stride: usize,
  frame_bytes: usize,
  process_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
  assert!(chunk_stride % frame_bytes == 0);
  assert!(chunk_stride >= frame_bytes);

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

    let mut offset = 0;
    while offset < used {
      let end = (offset + chunk_stride).min(used);
      process_chunk(&buffer[offset..end])?;
      offset = end;
    }
  }

  Ok(())
}
