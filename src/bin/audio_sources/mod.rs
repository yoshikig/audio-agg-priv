use anyhow::Result;
use sound_send::packet::{Meta, SampleFormat};

pub type ProcessChunk = Box<dyn FnMut(&[u8]) -> Result<()> + Send + 'static>;

pub struct InputOptions {
  pub channels: Option<u8>,
  pub sample_rate: Option<u32>,
  pub format: Option<SampleFormat>,
}

pub trait InputSource {
  fn validate_options(&self, opts: &InputOptions) -> Result<()>;
  fn prepare_meta(&mut self, opts: &InputOptions) -> Result<Meta>;
  fn start(&mut self, meta: &Meta, process_chunk: ProcessChunk) -> Result<()>;
}

#[cfg(feature = "cpal")]
pub mod cpal;
pub mod stdin;
#[cfg(target_os = "windows")]
pub mod wasapi;

#[cfg(feature = "cpal")]
pub use cpal::CpalInput;
pub use stdin::StdinInput;
#[cfg(target_os = "windows")]
pub use wasapi::WasapiInput;
