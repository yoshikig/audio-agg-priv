use std::{ffi::c_void, thread};

use anyhow::{Context, Result, anyhow, bail};
use sound_send::packet::{Meta, SampleFormat, SampleRate};
use windows::Win32::{
  Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
  Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    IAudioCaptureClient, IAudioClient3, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole,
  },
  Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM,
  Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
  System::{
    Com::{
      CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
      CoTaskMemFree, CoUninitialize,
    },
    Threading::{CreateEventW, WaitForSingleObject},
  },
};

use super::{InputOptions, InputSource, ProcessChunk};
use crate::{MAX_PAYLOAD, PAYLOAD_ALIGNMENT};

const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

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

  fn start(&mut self, _meta: &Meta, process_chunk: ProcessChunk) -> Result<()> {
    println!("Input: WASAPI loopback (default render mix)");
    let config = self
      .config
      .take()
      .expect("wasapi configuration missing before capture start");
    spawn_loopback_capture(config, process_chunk)?;
    Ok(())
  }
}

struct AudioFormat {
  data: AudioFormatData,
}

enum AudioFormatData {
  WaveFormat(Box<WAVEFORMATEX>),
  WaveFormatExtensible(Box<WAVEFORMATEXTENSIBLE>),
}

impl AudioFormat {
  unsafe fn from_mix_format(ptr: *mut WAVEFORMATEX) -> Result<Self> {
    if ptr.is_null() {
      bail!("mix format pointer was null");
    }
    if (*ptr).wFormatTag == WAVE_FORMAT_EXTENSIBLE_TAG
      && (*ptr).cbSize as usize
        >= std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
          - std::mem::size_of::<WAVEFORMATEX>()
    {
      let format = *(ptr as *const WAVEFORMATEXTENSIBLE);
      Ok(Self {
        data: AudioFormatData::WaveFormatExtensible(Box::new(format)),
      })
    } else {
      let format = *ptr;
      Ok(Self {
        data: AudioFormatData::WaveFormat(Box::new(format)),
      })
    }
  }

  fn as_waveformatex_ptr(&self) -> *const WAVEFORMATEX {
    match &self.data {
      AudioFormatData::WaveFormat(format) => &**format as *const WAVEFORMATEX,
      AudioFormatData::WaveFormatExtensible(format) => {
        &format.Format as *const WAVEFORMATEX
      }
    }
  }

  fn channels(&self) -> u16 {
    match &self.data {
      AudioFormatData::WaveFormat(format) => format.nChannels,
      AudioFormatData::WaveFormatExtensible(format) => format.Format.nChannels,
    }
  }

  fn sample_rate(&self) -> u32 {
    match &self.data {
      AudioFormatData::WaveFormat(format) => format.nSamplesPerSec,
      AudioFormatData::WaveFormatExtensible(format) => {
        format.Format.nSamplesPerSec
      }
    }
  }

  fn block_align(&self) -> u16 {
    match &self.data {
      AudioFormatData::WaveFormat(format) => format.nBlockAlign,
      AudioFormatData::WaveFormatExtensible(format) => {
        format.Format.nBlockAlign
      }
    }
  }

  fn bits_per_sample(&self) -> u16 {
    match &self.data {
      AudioFormatData::WaveFormat(format) => format.wBitsPerSample,
      AudioFormatData::WaveFormatExtensible(format) => {
        format.Format.wBitsPerSample
      }
    }
  }

  fn is_float(&self) -> bool {
    match &self.data {
      AudioFormatData::WaveFormat(format) => {
        format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT_TAG
      }
      AudioFormatData::WaveFormatExtensible(format) => unsafe {
        std::ptr::addr_of!(format.SubFormat).read_unaligned()
          == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
      },
    }
  }

  fn subformat_label(&self) -> &'static str {
    match &self.data {
      AudioFormatData::WaveFormat(format) => {
        if format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT_TAG {
          "IEEE Float"
        } else {
          "PCM"
        }
      }
      AudioFormatData::WaveFormatExtensible(format) => unsafe {
        let subformat = std::ptr::addr_of!(format.SubFormat).read_unaligned();
        if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
          "IEEE Float"
        } else if subformat == KSDATAFORMAT_SUBTYPE_PCM {
          "PCM"
        } else {
          "Other"
        }
      },
    }
  }
}

pub(super) struct LoopbackConfig {
  format: AudioFormat,
  periods: SharedModePeriodInfo,
}

#[derive(Clone, Copy, Debug)]
struct SharedModePeriodInfo {
  default_period_frames: u32,
  fundamental_period_frames: u32,
  min_period_frames: u32,
  max_period_frames: u32,
}

struct ComGuard;

impl ComGuard {
  fn init_mta() -> Result<Self> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
      .context("failed to initialize COM for WASAPI loopback")?;
    Ok(Self)
  }
}

impl Drop for ComGuard {
  fn drop(&mut self) {
    unsafe { CoUninitialize() };
  }
}

#[derive(Clone, Copy)]
enum Role {
  Console,
}

impl From<Role> for windows::Win32::Media::Audio::ERole {
  fn from(role: Role) -> Self {
    match role {
      Role::Console => eConsole,
    }
  }
}

struct EventHandle(HANDLE);

impl EventHandle {
  fn create() -> Result<Self> {
    let handle = unsafe { CreateEventW(None, false, false, None) }
      .context("failed to create WASAPI event")?;
    Ok(Self(handle))
  }

  fn wait(&self, timeout_ms: u32) -> Result<EventWait> {
    let result = unsafe { WaitForSingleObject(self.0, timeout_ms) };
    match result {
      WAIT_OBJECT_0 => Ok(EventWait::Signaled),
      WAIT_TIMEOUT => Ok(EventWait::Timeout),
      WAIT_FAILED => Err(anyhow!("WAIT_FAILED while waiting for WASAPI event")),
      other => Err(anyhow!("unexpected wait result: {}", other.0)),
    }
  }

  fn handle(&self) -> HANDLE {
    self.0
  }
}

impl Drop for EventHandle {
  fn drop(&mut self) {
    if !self.0.is_invalid() {
      unsafe {
        let _ = CloseHandle(self.0);
      }
    }
  }
}

enum EventWait {
  Signaled,
  Timeout,
}

/// Get the default playback device for a specific role.
fn get_default_render_device(role: Role) -> Result<IMMDevice> {
  let enumerator: IMMDeviceEnumerator =
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
      .context("failed to create MMDeviceEnumerator")?;
  unsafe {
    enumerator.GetDefaultAudioEndpoint(
      windows::Win32::Media::Audio::eRender,
      role.into(),
    )
  }
  .context("failed to get default audio endpoint")
}

fn query_mix_format(client: &IAudioClient3) -> Result<AudioFormat> {
  unsafe {
    let ptr = client.GetMixFormat().context("GetMixFormat failed")?;
    let format_result =
      AudioFormat::from_mix_format(ptr).context("failed to parse mix format");
    CoTaskMemFree(Some(ptr as *const c_void));
    Ok(format_result?)
  }
}

fn query_shared_mode_engine_period(
  client: &IAudioClient3,
  format: &AudioFormat,
) -> Result<SharedModePeriodInfo> {
  let mut default_period = 0u32;
  let mut fundamental_period = 0u32;
  let mut min_period = 0u32;
  let mut max_period = 0u32;
  unsafe {
    client.GetSharedModeEnginePeriod(
      format.as_waveformatex_ptr(),
      &mut default_period,
      &mut fundamental_period,
      &mut min_period,
      &mut max_period,
    )
  }
  .context("GetSharedModeEnginePeriod failed")?;
  Ok(SharedModePeriodInfo {
    default_period_frames: default_period,
    fundamental_period_frames: fundamental_period,
    min_period_frames: min_period,
    max_period_frames: max_period,
  })
}

fn frames_to_100ns(frames: u32, sample_rate: u32) -> i64 {
  if sample_rate == 0 {
    return 0;
  }
  let frames = frames.max(1) as u64;
  let sample_rate = sample_rate as u64;
  let ticks = frames
    .saturating_mul(10_000_000)
    .saturating_add(sample_rate / 2)
    / sample_rate;
  ticks.max(1) as i64
}

pub(super) fn prepare_loopback() -> Result<(Meta, LoopbackConfig)> {
  let _com = ComGuard::init_mta()?;
  let device = get_default_render_device(Role::Console)
    .context("no default render device for loopback")?;
  let audio_client: IAudioClient3 =
    unsafe { device.Activate::<IAudioClient3>(CLSCTX_ALL, None) }
      .context("failed to activate IAudioClient3 for loopback")?;

  let format = query_mix_format(&audio_client)
    .context("failed to query mix format for loopback")?;

  if !format.is_float() {
    bail!("loopback mix format is not 32-bit float");
  }

  let sample_rate = format.sample_rate();
  let channels = format.channels();

  let periods = query_shared_mode_engine_period(&audio_client, &format)
    .context("failed to query shared-mode engine period for loopback")?;

  let meta = Meta {
    channels: channels.min(255) as u8,
    sample_rate: SampleRate(sample_rate),
    sample_format: SampleFormat::F32,
  };

  Ok((meta, LoopbackConfig { format, periods }))
}

pub(super) fn spawn_loopback_capture(
  config: LoopbackConfig,
  process_chunk: ProcessChunk,
) -> Result<()> {
  let channels = config.format.channels();
  let sample_rate = config.format.sample_rate();
  let buffer_duration_hns =
    frames_to_100ns(config.periods.min_period_frames, sample_rate);

  eprintln!("Channels: {channels}");
  eprintln!("Sample Rate: {sample_rate}");
  eprintln!("Sample Bits: {}", config.format.bits_per_sample());
  eprintln!("Sample Format: {}", config.format.subformat_label());
  eprintln!("Block Align: {}", config.format.block_align());
  eprintln!(
    "Engine Periods (frames): default={}, fundamental={}, min={}, max={}",
    config.periods.default_period_frames,
    config.periods.fundamental_period_frames,
    config.periods.min_period_frames,
    config.periods.max_period_frames
  );
  eprintln!("Selected buffer duration (100ns units): {buffer_duration_hns}");
  eprintln!(
    "Selected buffer duration (ms): {:.3}",
    buffer_duration_hns as f64 / 10_000.0
  );

  thread::Builder::new()
    .name("wasapi-loopback".to_string())
    .spawn(move || {
      crate::boost_current_thread_priority();
      let mut chunker = process_chunk;
      if let Err(err) = run_loopback_capture(config, &mut chunker) {
        eprintln!("WASAPI loopback capture error: {err:?}");
      }
    })
    .context("failed to spawn WASAPI loopback thread")?;
  Ok(())
}

fn run_loopback_capture(
  config: LoopbackConfig,
  process_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
  let _com = ComGuard::init_mta()?;

  let device = get_default_render_device(Role::Console)
    .context("no default render device for loopback")?;
  let audio_client: IAudioClient3 =
    unsafe { device.Activate::<IAudioClient3>(CLSCTX_ALL, None) }
      .context("failed to activate IAudioClient3 for loopback")?;
  let sample_rate = config.format.sample_rate();
  let buffer_duration_hns =
    frames_to_100ns(config.periods.min_period_frames, sample_rate);

  let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK
    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

  unsafe {
    audio_client.Initialize(
      AUDCLNT_SHAREMODE_SHARED,
      stream_flags,
      buffer_duration_hns,
      0,
      config.format.as_waveformatex_ptr(),
      None,
    )
  }
  .context("failed to initialize WASAPI loopback client")?;

  let event = EventHandle::create()?;
  unsafe { audio_client.SetEventHandle(event.handle()) }
    .context("failed to set loopback event handle")?;

  let capture_client: IAudioCaptureClient =
    unsafe { audio_client.GetService() }
      .context("failed to get AudioCaptureClient for loopback")?;

  unsafe { audio_client.Start() }
    .context("failed to start WASAPI loopback stream")?;

  let frame_bytes = config.format.block_align() as usize;
  assert!(PAYLOAD_ALIGNMENT % frame_bytes == 0);
  assert!(MAX_PAYLOAD % frame_bytes == 0);

  let run_result: Result<(), anyhow::Error> = loop {
    if let Err(err) =
      drain_packets(&capture_client, MAX_PAYLOAD, frame_bytes, process_chunk)
    {
      break Err(err);
    }

    match event.wait(2000)? {
      EventWait::Signaled => {}
      EventWait::Timeout => continue,
    }
  };

  let stop_result = unsafe { audio_client.Stop() }
    .context("failed to stop WASAPI loopback stream");

  if let Err(run_err) = run_result {
    if let Err(stop_err) = stop_result {
      eprintln!("failed to stop WASAPI loopback stream: {stop_err:?}");
    }
    Err(run_err)
  } else {
    stop_result?;
    Ok(())
  }
}

fn drain_packets(
  capture_client: &IAudioCaptureClient,
  chunk_stride: usize,
  frame_bytes: usize,
  process_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
  assert!(chunk_stride % frame_bytes == 0);
  assert!(chunk_stride >= frame_bytes);

  loop {
    let packet_frames = unsafe { capture_client.GetNextPacketSize() }
      .context("failed to query next packet size")?;
    if packet_frames == 0 {
      break;
    }

    let mut buffer_ptr = std::ptr::null_mut();
    let mut frames_returned = 0u32;
    let mut flags = 0u32;
    unsafe {
      capture_client.GetBuffer(
        &mut buffer_ptr,
        &mut frames_returned,
        &mut flags,
        None,
        None,
      )
    }
    .context("failed to read loopback packet")?;

    if frames_returned == 0 {
      unsafe { capture_client.ReleaseBuffer(frames_returned) }
        .context("failed to release empty loopback packet")?;
      continue;
    }

    let used = frames_returned as usize * frame_bytes;
    let mut buffer = vec![0u8; used];

    if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) == 0 {
      unsafe {
        std::ptr::copy_nonoverlapping(buffer_ptr, buffer.as_mut_ptr(), used);
      }
    }

    unsafe { capture_client.ReleaseBuffer(frames_returned) }
      .context("failed to release loopback packet")?;

    process_chunk(&buffer)?;
  }

  Ok(())
}
