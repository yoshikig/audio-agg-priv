use std::io::{self, Read};

use anyhow::Result;
use sound_send::packet::{Meta, SampleFormat, SampleRate};

use super::{InputOptions, InputSource, ProcessChunk};
use crate::MAX_PAYLOAD;

pub struct StdinInput;

impl InputSource for StdinInput {
  fn validate_options(&self, _opts: &InputOptions) -> Result<()> {
    Ok(())
  }

  fn prepare_meta(&mut self, opts: &InputOptions) -> Result<Meta> {
    Ok(Meta {
      channels: opts.channels.unwrap_or(2),
      sample_rate: SampleRate(opts.sample_rate.unwrap_or(48_000)),
      sample_format: opts.format.unwrap_or(SampleFormat::U32),
    })
  }

  fn start(
    &mut self,
    _meta: &Meta,
    process_chunk: ProcessChunk,
  ) -> Result<()> {
    println!("Input: stdin (reading raw bytes)");
    std::thread::spawn(move || {
      crate::boost_current_thread_priority();
      let mut chunker = process_chunk;
      let mut stdin = io::stdin().lock();
      let mut buf = vec![0u8; MAX_PAYLOAD];
      loop {
        match stdin.read(&mut buf) {
          Ok(0) => break,
          Ok(n) => {
            if chunker(&buf[..n]).is_err() {
              break;
            }
          }
          Err(_) => break,
        }
      }
    });
    Ok(())
  }
}
