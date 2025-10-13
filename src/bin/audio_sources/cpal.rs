use anyhow::{Context, Result, bail};
use sound_send::packet::{Meta, SampleFormat, SampleRate};

use super::{InputOptions, InputSource, ProcessChunk};
use crate::MAX_PAYLOAD;

pub struct CpalInput {
  device: cpal::Device,
  supported_config: Option<cpal::SupportedStreamConfig>,
  stream: Option<cpal::Stream>,
}

impl CpalInput {
  pub fn new(device: cpal::Device) -> Self {
  use cpal::traits::DeviceTrait;

    let supported_config = device
      .default_input_config().ok();

    Self { device, supported_config: supported_config, stream: None }
  }
}

impl InputSource for CpalInput {
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
    generate_cpal_meta(&self.device, self.supported_config.as_ref().ok_or(
      anyhow::anyhow!("no default input device or supported config found"),
    )?)
  }

  fn start(
    &mut self,
    meta: &Meta,
    process_chunk: ProcessChunk,
  ) -> Result<()> {
    self.stream =
      Some(generate_cpal_stream(&self.device, &self.supported_config.as_ref().context("no default input device or supported config found")?.config(), meta.sample_format, process_chunk)?);
    Ok(())
  }
}

fn generate_cpal_stream(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  sample_format: SampleFormat,
  process_chunk: ProcessChunk,
) -> Result<cpal::Stream> {
  use cpal::traits::{DeviceTrait, StreamTrait};

  let stream: cpal::Stream = match sample_format {
    SampleFormat::F32 => {
      build_cpal_input_stream::<f32>(device, &config, process_chunk)?
    }
    SampleFormat::I16 => {
      build_cpal_input_stream::<i16>(device, &config, process_chunk)?
    }
    SampleFormat::U16 => {
      build_cpal_input_stream::<u16>(device, &config, process_chunk)?
    }
    other => bail!("unsupported sample format: {:?}", other),
  };
  stream.play().context("failed to start input stream")?;

  Ok(stream)
}

fn generate_cpal_meta(device: &cpal::Device, supported_config: &cpal::SupportedStreamConfig) -> Result<Meta> {
  use cpal::traits::DeviceTrait;

  // Metadata to include in each packet
  let mut packet_meta = Meta {
    channels: 0,
    sample_rate: SampleRate(0),
    sample_format: SampleFormat::F32,
  };
  let config = supported_config.config();

  println!("Input: CPAL (default audio input)");
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

fn build_cpal_input_stream<T>(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  process_chunk: ProcessChunk,
) -> Result<cpal::Stream>
where
  T: cpal::Sample + cpal::SizedSample + bytemuck::Pod + bytemuck::Zeroable,
{
  use cpal::traits::DeviceTrait;

  // Cast &[T] -> &[u8] safely via bytemuck
  let err_fn = |err| eprintln!("input stream error: {err}");

  let mut chunker = process_chunk;
  let stream = device.build_input_stream(
    config,
    move |data: &[T], _| {
      chunker(bytemuck::cast_slice(data));
    },
    err_fn,
    None,
  )?;
  Ok(stream)
}
