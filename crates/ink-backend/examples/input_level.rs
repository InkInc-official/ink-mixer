//! 確認用: 入力デバイスを開き、入力レベルを 100ms ごとに表示する（Issue #36）。
//!
//! ```text
//! cargo run -p ink-backend --example input_level -- [--device <ID>] [--rate <Hz>] [--channels <N>] [--seconds <N>]
//! ```
//!
//! - `--device`: `ink-mixer devices` が表示する入力デバイスの id。省くと既定の入力
//! - `--rate`: サンプルレート。省くと 48000（対応していなければ一覧の最初）
//! - `--channels`: 開くチャンネル数。省くとデバイスの最大
//! - `--seconds`: 表示する秒数。既定は 10
//!
//! 終了時に、1秒あたりのコールバックの回数とフレーム数を表示する。フレーム数がサンプルレートより
//! 少なければ、データが足りていない。
//!
//! レベルは stdout、エラーは stderr に出す。ストリームのエラー、または 1 秒間コールバックが
//! 来ない（入力が止まった）ときは、終了コード 1 で終わる。
//!
//! `ink-mixer run`（#38）ができるまでの一時的な確認用で、配布物には入らない。

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ink_backend::{AudioBackend, CpalBackend, DeviceId, DeviceInfo, StreamConfig};
use ink_core::graph::{PeakMeter, SILENCE_DB, block_peak, gain_to_db};

/// レベルを表示する間隔。
const INTERVAL: Duration = Duration::from_millis(100);
/// この時間コールバックが来なければ、入力が止まったとみなす。
const STALL: Duration = Duration::from_secs(1);
/// レベルのバーの長さ（文字数）。
const BAR_WIDTH: usize = 40;

struct Args {
    device: Option<String>,
    rate: Option<u32>,
    channels: Option<u16>,
    seconds: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        device: None,
        rate: None,
        channels: None,
        seconds: 10,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--device" => args.device = Some(value()?),
            "--rate" => {
                let v = value()?;
                args.rate = Some(v.parse().map_err(|_| format!("invalid --rate: {v}"))?);
            }
            "--channels" => {
                let v = value()?;
                args.channels = Some(v.parse().map_err(|_| format!("invalid --channels: {v}"))?);
            }
            "--seconds" => {
                let v = value()?;
                args.seconds = v.parse().map_err(|_| format!("invalid --seconds: {v}"))?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    Ok(args)
}

/// 開くデバイスを決める。`--device` があれば入力デバイスの一覧から探し、無ければ既定の入力。
fn find_device(backend: &CpalBackend, id: Option<&str>) -> Result<DeviceInfo, String> {
    match id {
        Some(id) => backend
            .enumerate_input_devices()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|d| d.id.as_str() == id)
            .ok_or(format!(
                "input device not found: {id} (see `cargo run -p ink-mixer -- devices`)"
            )),
        None => backend
            .default_input()
            .map_err(|e| e.to_string())?
            .ok_or("no default input device".to_string()),
    }
}

/// 48000 Hz に対応していれば 48000、無ければ一覧の最初。
fn choose_rate(info: &DeviceInfo) -> Option<u32> {
    if info.sample_rates.contains(&48_000) {
        Some(48_000)
    } else {
        info.sample_rates.first().copied()
    }
}

/// レベル（倍率）を「-23.4 dBFS |#####     |」の形にする。
fn level_line(peak: f32) -> String {
    let db = gain_to_db(peak);
    let filled = if db <= SILENCE_DB {
        0
    } else {
        let ratio = ((db - SILENCE_DB) / -SILENCE_DB).clamp(0.0, 1.0);
        (ratio * BAR_WIDTH as f32).round() as usize
    };
    let bar = format!("{}{}", "#".repeat(filled), " ".repeat(BAR_WIDTH - filled));
    if db.is_finite() {
        format!("{db:6.1} dBFS |{bar}|")
    } else {
        format!("  -inf dBFS |{bar}|")
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!(
                "usage: input_level [--device <ID>] [--rate <Hz>] [--channels <N>] [--seconds <N>]"
            );
            return ExitCode::FAILURE;
        }
    };

    let backend = CpalBackend::new();
    let info = match find_device(&backend, args.device.as_deref()) {
        Ok(info) => info,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(sample_rate) = args.rate.or_else(|| choose_rate(&info)) else {
        eprintln!("error: {} reports no sample rate", info.id);
        return ExitCode::FAILURE;
    };
    let config = StreamConfig {
        sample_rate,
        channels: args.channels.unwrap_or(info.channels),
    };

    let meter = Arc::new(PeakMeter::new());
    let frames = Arc::new(AtomicU64::new(0));
    let writer = Arc::clone(&meter);
    let frame_counter = Arc::clone(&frames);
    let channels = u64::from(config.channels.max(1));
    let callback = Box::new(move |samples: &[f32]| {
        // 音声スレッド: 確保・ロック・ログ出力をしない。
        // interleaved のまま全体の最大を取ると、チャンネルごとの最大のうち一番大きい値になる。
        writer.record(block_peak(samples));
        frame_counter.fetch_add(samples.len() as u64 / channels, Ordering::Relaxed);
    });

    let id = DeviceId::new(info.id.as_str());
    let stream = match backend.open_input(&id, config, callback) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("device:   {} ({})", info.name, info.id);
    println!(
        "config:   {} Hz, {} ch, {}",
        config.sample_rate,
        config.channels,
        stream.sample_format()
    );

    let start = Instant::now();
    let end = start + Duration::from_secs(args.seconds);
    let mut last_callbacks = 0;
    let mut last_progress = Instant::now();
    let mut frames_shown = false;
    while Instant::now() < end {
        thread::sleep(INTERVAL);

        if let Some(e) = stream.error() {
            eprintln!("error: input stream stopped: {e}");
            return ExitCode::FAILURE;
        }
        let callbacks = stream.callbacks();
        if callbacks != last_callbacks {
            last_callbacks = callbacks;
            last_progress = Instant::now();
        } else if last_progress.elapsed() >= STALL {
            eprintln!(
                "error: input stopped: no audio for {} s (device unplugged?)",
                STALL.as_secs()
            );
            return ExitCode::FAILURE;
        }
        if !frames_shown && callbacks > 0 {
            println!("callback: {} frames", stream.callback_frames());
            frames_shown = true;
        }

        println!("{}", level_line(meter.take()));
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "rate:     {:.1} callbacks/s, {:.0} frames/s (sample rate {} Hz)",
        stream.callbacks() as f64 / elapsed,
        frames.load(Ordering::Relaxed) as f64 / elapsed,
        config.sample_rate
    );
    println!(
        "done:     {} callbacks, {} xruns{}",
        stream.callbacks(),
        stream.xruns(),
        if stream.realtime_denied() {
            ", realtime scheduling denied"
        } else {
            ""
        }
    );
    ExitCode::SUCCESS
}
