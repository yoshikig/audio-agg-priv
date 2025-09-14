use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::env;
use std::io::{self, Read};
use std::net::UdpSocket;
use std::sync::mpsc;
use sound_send::rate::RollingRate;
use sound_send::volume::VolumeMeter;
use std::sync::{Arc, Mutex};
use sound_send::packet::{encode_packet, Meta};
use std::any::TypeId;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sound_send::packet::{decode_message, encode_sync, respond_to_ping, Message, SyncMessage};
use sound_send::send_stats::SendStats;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
    Cpal,
    Stdin,
}

fn main() -> Result<()> {
    // --- 1. Parse args and set up socket ---
    let mut args = env::args().skip(1); // skip program name
    let mut input_mode = InputMode::Cpal;
    let mut server_addr: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            "-i" | "--input" => {
                let val = args
                    .next()
                    .ok_or_else(|| {
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
                if server_addr.is_none() { server_addr = Some(s.to_string()); }
                else { bail!("unexpected argument: {}", s); }
            }
        }
    }
    let server_addr = server_addr.ok_or_else(|| {
        anyhow::anyhow!(
            "missing destination. Usage: udp_sender <addr:port> [--input cpal|stdin]"
        )
    })?;

    // Create UDP socket (OS picks an ephemeral local port)
    let socket = UdpSocket::bind("0.0.0.0:0").context("failed to bind UDP socket")?;
    println!("Destination: {}", server_addr);

    // Channel used to pass input chunks to the main thread
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let meter = Arc::new(Mutex::new(VolumeMeter::new(VOLUME_WINDOW)));

    // --- 2. Configure input source ---
    let _maybe_stream; // keep stream alive when in CPAL mode
    // Metadata to include in each packet
    let mut packet_meta = Meta {
        channels: 0,
        sample_rate: cpal::SampleRate(0),
        sample_format: cpal::SampleFormat::F32,
    };
    match input_mode {
        InputMode::Cpal => {
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
            packet_meta.sample_rate = config.sample_rate;
            packet_meta.sample_format = supported_config.sample_format();

            let stream: cpal::Stream = match supported_config.sample_format() {
                cpal::SampleFormat::F32 => {
                    build_input_stream::<f32>(
                        &device,
                        &config,
                        tx.clone(),
                        Some(meter.clone()),
                    )?
                }
                cpal::SampleFormat::I16 => {
                    build_input_stream::<i16>(
                        &device,
                        &config,
                        tx.clone(),
                        Some(meter.clone()),
                    )?
                }
                cpal::SampleFormat::U16 => {
                    build_input_stream::<u16>(
                        &device,
                        &config,
                        tx.clone(),
                        Some(meter.clone()),
                    )?
                }
                other => bail!("unsupported sample format: {:?}", other),
            };
            stream.play().context("failed to start input stream")?;
            _maybe_stream = Some(stream);
        }
        InputMode::Stdin => {
            println!("Input: stdin (reading raw bytes)");
            std::thread::spawn(move || {
                let mut stdin = io::stdin().lock();
                // Use a buffer that fits roughly within a UDP payload
                // once the ~24B header is added
                const MAX_PAYLOAD: usize = 1200;
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

    std::thread::spawn(move || {
        let mut total_bytes_sent: u64 = 0;
        let mut sequence_number: u64 = 0;
        let mut last_update_time = Instant::now();
        let mut byte_rate = RollingRate::new(WINDOW);

        for audio_chunk in rx {
            let now_ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|_| Duration::from_millis(0));
            let ts_ms = now_ts.as_millis() as u64;
            let send_buf = encode_packet(sequence_number, &audio_chunk, packet_meta, ts_ms);

            if send_sock
                .send_to(&send_buf, &server_addr_cloned)
                .is_err()
            {
                // Ignore send errors and continue
            }

            let now = Instant::now();
            let sent_packet_size = send_buf.len();
            total_bytes_sent += sent_packet_size as u64;
            byte_rate.record(now, sent_packet_size as u64);

            if now.duration_since(last_update_time) >= UPDATE_INTERVAL {
                let average_rate_bps = byte_rate.rate_per_sec(now);
                let _ = stats_tx.send(SendStats { total_bytes_sent, average_rate_bps });
                last_update_time = now;
            }

            sequence_number = sequence_number.wrapping_add(1);
        }
        // Drop the stats channel to signal completion
        drop(stats_tx);
    });

    #[cfg(target_os = "macos")]
    {
        println!("Sending started.");
        sound_send::status_icon_mac::show_status_icon(stats_rx);
    }

    #[cfg(not(target_os = "macos"))]
    {
        use std::io::Write;

        println!("Sending started. Press Ctrl+C to stop.");

        // Main thread: receive stats and render
        while let Ok(stats) = stats_rx.recv() {
            let now = Instant::now();
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

fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<Vec<u8>>,
    meter: Option<Arc<Mutex<VolumeMeter>>>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample + bytemuck::Pod + bytemuck::Zeroable,
{
    // Cast &[T] -> &[u8] safely via bytemuck
    let err_fn = |err| eprintln!("input stream error: {err}");

    let channels = config.channels as usize;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _| {
            // Data is interleaved. Send in reasonably small chunks.
            // For now, split the current callback buffer into UDP-sized chunks.
            if let Some(m) = &meter {
                let mut guard = m.lock().unwrap();
                let now = Instant::now();
                if TypeId::of::<T>() == TypeId::of::<f32>() {
                    let f: &[f32] = unsafe { &*(data as *const [T] as *const [f32]) };
                    guard.add_samples_f32(now, f);
                } else if TypeId::of::<T>() == TypeId::of::<i16>() {
                    let f: &[i16] = unsafe { &*(data as *const [T] as *const [i16]) };
                    guard.add_samples_i16(now, f);
                } else if TypeId::of::<T>() == TypeId::of::<u16>() {
                    let f: &[u16] = unsafe { &*(data as *const [T] as *const [u16]) };
                    guard.add_samples_u16(now, f);
                }
            }
            let bytes: &[u8] = bytemuck::cast_slice(data);
            // Split to avoid exceeding typical MTU when adding our ~24-byte header
            const MAX_PAYLOAD: usize = 1024 + 256; // payload only (excludes our header)
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + MAX_PAYLOAD).min(bytes.len());
                let chunk = &bytes[offset..end];
                if tx.send(chunk.to_vec()).is_err() {
                    break;
                }
                offset = end;
            }
            let _ = channels; // keep to show we considered channel count
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

fn print_usage() {
    eprintln!(
        "Usage: udp_sender <server_addr:port> [--input <cpal|stdin>]\n\
         -i, --input    Input source (default: cpal)\n\
         -h, --help     Show this help"
    );
}

fn wait_for_pong_handshake(socket: &UdpSocket, server_addr: &str) -> Result<()> {
    // Temporarily set a read timeout for handshake retries
    let original_timeout = socket
        .read_timeout()
        .unwrap_or(None);
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
                if let Ok(Message::Sync(SyncMessage::Pong { t0_ms, .. })) = decode_message(&buf[..n]) {
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
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
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
                if let Ok(Message::Sync(SyncMessage::Ping { t0_ms })) = decode_message(&buf[..n]) {
                    respond_to_ping(&ts_sock, addr, t0_ms);
                }
            }
            Err(_) => break,
        }
    });
}
