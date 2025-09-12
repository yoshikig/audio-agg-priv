use std::env;
use std::io::{self, Write};
use std::net::UdpSocket;
use std::time::{Duration, Instant};
use sound_send::rate::RollingRate;
use sound_send::packet::decode_packet;

fn main() -> io::Result<()> {
    // 1. Parse listening address from command-line args
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <listen_addr:port>", args[0]);
        eprintln!("Example: {} 127.0.0.1:12345", args[0]);
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid arguments",
        ));
    }
    let listen_addr = &args[1];

    // 2. Bind UDP socket and start listening
    let socket = UdpSocket::bind(listen_addr)?;
    eprintln!("Listening on {} ...", socket.local_addr()?);

    // 3. Prepare receive buffer and statistics
    // UDP max payload is 65507 bytes, but typical MTU is ~1500
    // Use a buffer larger than the client's chunk size to be safe
    let mut buf = [0; 2048];
    let mut total_bytes_received: u64 = 0;
    let mut total_packets_received: u64 = 0;
    let mut expected_sequence: u64 = 0;
    let mut lost_packets: u64 = 0;
    let mut out_of_order_packets: u64 = 0;
    let mut last_update_time = Instant::now();
    const UPDATE_INTERVAL: Duration = Duration::from_millis(200); // stats update interval (0.2s)
    const WINDOW: Duration = Duration::from_secs(10);

    // Rolling rates over the last WINDOW
    let mut pkt_rate = RollingRate::new(WINDOW);
    let mut byte_rate = RollingRate::new(WINDOW);

    // Lock stdout for efficient writing
    let mut stdout = io::stdout().lock();

    // 4. Receive loop
    loop {
        // Receive data; get byte count and source address
        let (bytes_received, src_addr) = socket.recv_from(&mut buf)?;

        // Decode packet (magic, length, meta, sequence, payload)
        let decoded = match decode_packet(&buf[..bytes_received]) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let received_sequence = decoded.seq;
        let payload = decoded.payload;

        total_bytes_received += bytes_received as u64;
        total_packets_received += 1;

        // Update rolling rates
        let now_inst = Instant::now();
        pkt_rate.record(now_inst, 1);
        byte_rate.record(now_inst, payload.len() as u64);

        // Check packet loss/order; write payload only for in-order packets
        if received_sequence == expected_sequence {
            // In-order packet: write payload
            stdout.write_all(payload)?;
            expected_sequence += 1;
        } else if received_sequence > expected_sequence {
            // Some packets were lost.
            // This packet is in-order relative to its sequence; write payload
            stdout.write_all(payload)?;
            let lost_count = received_sequence - expected_sequence;
            lost_packets += lost_count;
            expected_sequence = received_sequence + 1;
        } else { // received_sequence < expected_sequence
            // Late/out-of-order packet: count it but do not write payload
            out_of_order_packets += 1;
        }

        // Update and print stats periodically
        let now = Instant::now();
        if now.duration_since(last_update_time) >= UPDATE_INTERVAL {
            // Rolling averages over the last WINDOW seconds
            let pkt_per_sec = pkt_rate.rate_per_sec(now);
            let bytes_per_sec = byte_rate.rate_per_sec(now);
            let average_rate_kbs = bytes_per_sec / 1024.0;

            // Print stats in a single line (carriage return to overwrite)
            let total_expected_packets = expected_sequence;
            let loss_percentage = if total_expected_packets > 0 {
                (lost_packets as f64 / total_expected_packets as f64) * 100.0
            } else {
                0.0
            };

            eprint!(
                "\rRecv: {} | Lost: {} ({:.2}%) | Late: {} | Total: {:.2} MB | \
                 Avg10s: {:.2} KB/s | Pkts10s: {:.2}/s from {}   ",
                total_packets_received,
                lost_packets,
                loss_percentage,
                out_of_order_packets,
                total_bytes_received as f64 / (1024.0 * 1024.0),
                average_rate_kbs,
                pkt_per_sec,
                src_addr
            );
            // Flush to stderr immediately
            io::stderr().flush()?;

            last_update_time = now;
        }
    }
    // This loop is typically interrupted with Ctrl+C
}
