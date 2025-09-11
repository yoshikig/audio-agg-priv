use std::collections::VecDeque;
use std::env;
use std::io::{self, Read, Write};
use std::net::UdpSocket;
use std::time::{Duration, Instant};

// 送信するデータのチャンクサイズ (バイト単位)
const CHUNK_SIZE: usize = 1024 + 256; // 1280 bytes

fn main() -> io::Result<()> {
    // 1. コマンドライン引数を解析する
    let args: Vec<String> = env::args().collect();

    if args.len() != 2 {
        // 引数が不正な場合は、使い方を表示してエラー終了
        eprintln!("使用法: {} <サーバーアドレス:ポート>", args[0]);
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "引数が不正です。",
        ));
    }
    let server_addr = &args[1];

    // 2. UDPソケットを作成する
    // "0.0.0.0:0" を指定すると、OSが利用可能なローカルポートを自動的に割り当てる
    let socket = UdpSocket::bind("0.0.0.0:0")?;

    println!("標準入力を受け付けます。Ctrl+D (Unix系) または Ctrl+Z (Windows) で終了します。");
    println!("送信先: {}", server_addr);
    println!("チャンクサイズ: {} バイト", CHUNK_SIZE);

    // 3. 標準入力から一定のバイト数ずつ読み込み、送信する
    let mut buffer = [0u8; CHUNK_SIZE];
    let mut stdin = io::stdin().lock();
    let mut total_bytes_sent: u64 = 0;
    let mut sequence_number: u64 = 0;

    // 統計情報用の変数を初期化
    let mut history: VecDeque<(Instant, usize)> = VecDeque::new();
    let mut last_update_time = Instant::now();
    const UPDATE_INTERVAL: Duration = Duration::from_millis(200); // 統計情報の更新間隔 (0.2秒)
    const HISTORY_DURATION: Duration = Duration::from_secs(10); // 平均を計算する期間 (10秒)

    loop {
        let bytes_read = stdin.read(&mut buffer)?;

        if bytes_read == 0 {
            // EOF (End of File) に到達した
            break;
        }

        // ヘッダー（シーケンス番号）とペイロードを結合して送信
        let mut send_buf = Vec::with_capacity(8 + bytes_read);
        send_buf.extend_from_slice(&sequence_number.to_be_bytes());
        send_buf.extend_from_slice(&buffer[..bytes_read]);

        socket.send_to(&send_buf, server_addr)?;

        // 統計情報を記録
        let now = Instant::now();
        let sent_packet_size = send_buf.len();
        total_bytes_sent += sent_packet_size as u64;
        // 平均レート計算のため、履歴にはヘッダーを含めたパケットサイズを記録
        history.push_back((now, sent_packet_size));

        // 一定間隔で統計情報を更新・表示
        if now.duration_since(last_update_time) >= UPDATE_INTERVAL {
            // 1. 履歴から古いデータを削除 (10秒以上前のもの)
            while let Some((timestamp, _)) = history.front() {
                if now.duration_since(*timestamp) > HISTORY_DURATION {
                    history.pop_front();
                } else {
                    break;
                }
            }

            // 2. 過去10秒間の合計バイト数を計算し、平均送信レート (Bytes/sec) を算出
            let recent_bytes: usize = history.iter().map(|&(_, bytes)| bytes).sum();
            let average_rate_bps = recent_bytes as f64 / HISTORY_DURATION.as_secs_f64();

            // 3. 統計情報を一行にまとめて表示 (\rでカーソルを先頭に戻して上書き)
            print!(
                "\r合計: {:>7.2} MB | 過去10秒平均: {:>7.2} KB/s   ",
                total_bytes_sent as f64 / (1024.0 * 1024.0),
                average_rate_bps / 1024.0
            );
            io::stdout().flush()?; // 表示を即時反映させる

            last_update_time = now;
        }

        // 次のパケットのためにシーケンス番号をインクリメント
        sequence_number += 1;
    }

    // 最終的な統計情報を表示
    println!(); // 統計表示行から改行するため
    println!("\n入力が終了しました。プログラムを終了します。");
    println!(
        "最終的な合計送信バイト数: {} バイト (約 {:.2} MB)",
        total_bytes_sent,
        total_bytes_sent as f64 / (1024.0 * 1024.0)
    );

    Ok(())
}
