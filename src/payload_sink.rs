use crate::packet::Meta;
use std::io::{self, Write};
use std::process::{Child, Command, Stdio};

pub struct BinarySink {
    use_pipewire: bool,
    child: Option<Child>,
    pw_stdin: Option<std::process::ChildStdin>,
    last_meta: Option<Meta>,
}

impl BinarySink {
    pub fn new(use_pipewire: bool) -> Self {
        Self { use_pipewire, child: None, pw_stdin: None, last_meta: None }
    }

    fn spawn_pw(&mut self, meta: &Meta) -> io::Result<()> {
        let fmt = match meta.sample_format {
            crate::packet::SampleFormat::F32 => "f32",
            crate::packet::SampleFormat::I16 => "s16",
            crate::packet::SampleFormat::U16 => "u16",
            _ => "f32",
        };
        let rate = meta.sample_rate.0.to_string();
        let ch = meta.channels.to_string();
        let mut child = Command::new("pw-cat")
            .arg("--playback")
            .arg("--rate").arg(rate)
            .arg("--channels").arg(ch)
            .arg("--format").arg(fmt)
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()?;
        self.pw_stdin = child.stdin.take();
        self.child = Some(child);
        self.last_meta = Some(*meta);
        Ok(())
    }

    pub fn process(&mut self, meta: &Meta, payload: &[u8]) -> io::Result<()> {
        if self.use_pipewire {
            if self.pw_stdin.is_none() || self.meta_changed(meta) {
                // If format changed, restart pw-cat with new params
                let _ = self.teardown_child();
                self.spawn_pw(meta)?;
            }
            match self.pw_stdin.as_mut().unwrap().write_all(payload) {
                Ok(()) => {}
                Err(e) => {
                    // Try one restart on write failure (e.g., broken pipe), then retry once
                    let _ = self.teardown_child();
                    self.spawn_pw(meta)?;
                    self.pw_stdin.as_mut().unwrap().write_all(payload).map_err(|e2| {
                        // If retry also fails, return original error context
                        io::Error::new(e2.kind(), format!("pipewire write failed after restart: {e}"))
                    })?;
                }
            }
        } else {
            io::stdout().write_all(payload)?;
        }
        Ok(())
    }

    fn meta_changed(&self, meta: &Meta) -> bool {
        match self.last_meta {
            Some(m) => m.channels != meta.channels || m.sample_rate.0 != meta.sample_rate.0 || m.sample_format as u8 != meta.sample_format as u8,
            None => true,
        }
    }

    fn teardown_child(&mut self) -> io::Result<()> {
        if let Some(mut child) = self.child.take() {
            // Close stdin so pw-cat can terminate gracefully
            self.pw_stdin.take();
            // Attempt to wait; if it errors, ignore (process may have already exited)
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }
}

impl Drop for BinarySink {
    fn drop(&mut self) {
        let _ = self.teardown_child();
    }
}
