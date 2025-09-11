use std::env;
use std::io::{self, Write};
use std::net::UdpSocket;
use std::convert::TryInto;
use std::time::{Duration, Instant};

fn main() -> io::Result<()> {
    // 1. コマンドライン引数から待受アドレスを取得
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("使用法: {} <待受アドレス:ポート>", args[0]);
        eprintln!("例: {} 127.0.0.1:12345", args[0]);
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "引数が不正です。",
        ));
    }
    let listen_addr = &args[1];

    // 2. UDPソケットをバインドして待受開始
    let socket = UdpSocket::bind(listen_addr)?;
    eprintln!("{} で待受を開始しました...", socket.local_addr()?);

    // 3. データ受信用バッファと統計用変数を準備
    // UDPの最大ペイロードサイズは65507バイトだが、通常はMTU(約1500)以下に収まる
    // クライアントのCHUNK_SIZEより大きいサイズを確保しておくと安全
    let mut buf = [0; 2048];
    let mut total_bytes_received: u64 = 0;
    let mut total_packets_received: u64 = 0;
    let mut expected_sequence: u64 = 0;
    let mut lost_packets: u64 = 0;
    let mut out_of_order_packets: u64 = 0;
    let start_time = Instant::now();
    let mut last_update_time = Instant::now();
    const UPDATE_INTERVAL: Duration = Duration::from_millis(200); // 統計情報の更新間隔 (0.2秒)

    // 標準出力をロックして効率的に書き込めるようにする
    let mut stdout = io::stdout().lock();

    // 4. データ受信ループ
    loop {
        // データを受信し、受信したバイト数と送信元アドレスを取得
        let (bytes_received, src_addr) = socket.recv_from(&mut buf)?;

        // ヘッダーサイズ(u64 = 8 bytes)より小さいパケットは無視
        if bytes_received < 8 {
            continue;
        }

        total_bytes_received += bytes_received as u64;
        total_packets_received += 1;

        // ヘッダーからシーケンス番号を抽出
        let received_sequence = u64::from_be_bytes(buf[0..8].try_into().unwrap());

        // パケットロスと順序をチェックし、順序が正しいものだけを標準出力に書き出す
        if received_sequence == expected_sequence {
            // 期待通りのパケット。ペイロードを書き出す
            stdout.write_all(&buf[8..bytes_received])?;
            expected_sequence += 1;
        } else if received_sequence > expected_sequence {
            // パケットがいくつか飛んだ (ロス)。
            // このパケット自体は順序が正しいので、ペイロードを書き出す
            stdout.write_all(&buf[8..bytes_received])?;
            let lost_count = received_sequence - expected_sequence;
            lost_packets += lost_count;
            expected_sequence = received_sequence + 1;
        } else { // received_sequence < expected_sequence
            // 遅延して到着したパケット (順序が違う)。
            // 統計には加えるが、標準出力には書き出さない
            out_of_order_packets += 1;
        }

        // 一定間隔で統計情報を更新・表示
        let now = Instant::now();
        if now.duration_since(last_update_time) >= UPDATE_INTERVAL {
            // 経過時間と平均受信レートを計算
            let elapsed_time = start_time.elapsed().as_secs_f64();
            let average_rate_kbs = if elapsed_time > 0.0 {
                (total_bytes_received as f64 / 1024.0) / elapsed_time
            } else {
                0.0
            };

            // 統計情報を一行にまとめて表示 (\rでカーソルを先頭に戻して上書き)
            let total_expected_packets = expected_sequence;
            let loss_percentage = if total_expected_packets > 0 {
                (lost_packets as f64 / total_expected_packets as f64) * 100.0
            } else {
                0.0
            };

            eprint!(
                "\r受信: {} | ロス: {} ({:.2}%) | 遅延: {} | 合計: {:.2} MB | 平均: {:.2} KB/s from {}   ",
                total_packets_received, lost_packets, loss_percentage, out_of_order_packets,
                total_bytes_received as f64 / (1024.0 * 1024.0),
                average_rate_kbs,
                src_addr
            );
            // 標準エラー出力に即時反映させる
            io::stderr().flush()?;

            last_update_time = now;
        }
    }
    // このループは通常Ctrl+Cで中断されるため、ループ後のコードは実行されません
}
