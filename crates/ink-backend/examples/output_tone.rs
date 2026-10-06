//! 確認用: 出力デバイスを開き、サイン波を鳴らす（Issue #37）。
//!
//! ```text
//! cargo run -p ink-backend --example output_tone -- [--device <ID>] [--rate <Hz>] [--channels <N>] [--seconds <N>] [--freq <Hz>] [--level <dBFS>]
//! ```
//!
//! - `--device`: `ink-mixer devices` が表示する出力デバイスの id。省くと既定の出力
//! - `--rate`: サンプルレート。省くと 48000（対応していなければ一覧の最初）
//! - `--channels`: 開くチャンネル数。省くと 2（デバイスの最大が 1 なら 1）。全チャンネルに同じ音を書く
//! - `--seconds`: 鳴らす秒数。既定は 3
//! - `--freq`: 周波数。既定は 440 Hz
//! - `--level`: 音量。既定は -20 dBFS。耳とスピーカーを守るため、0 dBFS より大きい値はエラーにする
//!
//! 鳴り始めと終わりに 50ms のフェードをかける。ストリームのエラー、または 1 秒間コールバックが
//! 来ない（出力が止まった）ときは、終了コード 1 で終わる。終了時に、1秒あたりのコールバックの
//! 回数とフレーム数、xrun の回数を表示する。
//!
//! `ink-mixer run`（#38）ができるまでの一時的な確認用で、配布物には入らない。

use std::f64::consts::TAU;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ink_backend::{AudioBackend, CpalBackend, DeviceId, DeviceInfo, StreamConfig};
use ink_core::graph::db_to_gain;

/// 状態を確かめる間隔。
const INTERVAL: Duration = Duration::from_millis(100);
/// この時間コールバックが来なければ、出力が止まったとみなす。
const STALL: Duration = Duration::from_secs(1);
/// 鳴り始めと終わりのフェードの長さ。
const FADE: Duration = Duration::from_millis(50);

struct Args {
    device: Option<String>,
    rate: Option<u32>,
    channels: Option<u16>,
    seconds: f64,
    freq: f64,
    level_db: f32,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        device: None,
        rate: None,
        channels: None,
        seconds: 3.0,
        freq: 440.0,
        level_db: -20.0,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--device" => args.device = Some(value()?),
            "--rate" => args.rate = Some(parse(&arg, &value()?)?),
            "--channels" => args.channels = Some(parse(&arg, &value()?)?),
            "--seconds" => args.seconds = parse(&arg, &value()?)?,
            "--freq" => args.freq = parse(&arg, &value()?)?,
            "--level" => args.level_db = parse(&arg, &value()?)?,
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if !(args.seconds > 0.0 && args.seconds.is_finite()) {
        return Err("--seconds must be greater than 0".to_string());
    }
    if !(args.freq > 0.0 && args.freq.is_finite()) {
        return Err("--freq must be greater than 0".to_string());
    }
    if args.level_db.is_nan() || args.level_db > 0.0 {
        return Err("--level must be 0 dBFS or below".to_string());
    }
    Ok(args)
}

fn parse<T: std::str::FromStr>(name: &str, value: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {name}: {value}"))
}

/// 開くデバイスを決める。`--device` があれば出力デバイスの一覧から探し、無ければ既定の出力。
fn find_device(backend: &CpalBackend, id: Option<&str>) -> Result<DeviceInfo, String> {
    match id {
        Some(id) => backend
            .enumerate_output_devices()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|d| d.id.as_str() == id)
            .ok_or(format!(
                "output device not found: {id} (see `cargo run -p ink-mixer -- devices`)"
            )),
        None => backend
            .default_output()
            .map_err(|e| e.to_string())?
            .ok_or("no default output device".to_string()),
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

/// サイン波を作る。音声スレッドで使うので、確保・ロック・ログ出力をしない。
struct Tone {
    channels: usize,
    gain: f64,
    step: f64,
    phase: f64,
    frame: u64,
    total_frames: u64,
    fade_frames: u64,
}

impl Tone {
    /// フェードの倍率（0.0〜1.0）。鳴らし終えた後は 0。
    fn envelope(&self) -> f64 {
        let i = self.frame;
        if i >= self.total_frames {
            0.0
        } else if i < self.fade_frames {
            i as f64 / self.fade_frames as f64
        } else if self.total_frames - i <= self.fade_frames {
            (self.total_frames - i) as f64 / self.fade_frames as f64
        } else {
            1.0
        }
    }

    /// interleaved のバッファを埋める。全チャンネルに同じ値を書く。
    fn fill(&mut self, buffer: &mut [f32]) {
        for frame in buffer.chunks_mut(self.channels) {
            let value = (self.gain * self.envelope() * self.phase.sin()) as f32;
            frame.fill(value);
            self.phase = (self.phase + self.step) % TAU;
            self.frame += 1;
        }
    }

    fn finished(&self) -> bool {
        self.frame >= self.total_frames
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!(
                "usage: output_tone [--device <ID>] [--rate <Hz>] [--channels <N>] \
                 [--seconds <N>] [--freq <Hz>] [--level <dBFS>]"
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
        channels: args.channels.unwrap_or(info.channels.min(2)),
    };

    let rate = f64::from(sample_rate);
    let mut tone = Tone {
        channels: usize::from(config.channels.max(1)),
        gain: f64::from(db_to_gain(args.level_db)),
        step: TAU * args.freq / rate,
        phase: 0.0,
        frame: 0,
        total_frames: (args.seconds * rate).round() as u64,
        fade_frames: (FADE.as_secs_f64() * rate).round().max(1.0) as u64,
    };
    let frames = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let frame_counter = Arc::clone(&frames);
    let finished_flag = Arc::clone(&finished);
    let channels = u64::from(config.channels.max(1));
    let callback = Box::new(move |buffer: &mut [f32]| {
        // 音声スレッド: 確保・ロック・ログ出力をしない。
        tone.fill(buffer);
        frame_counter.fetch_add(buffer.len() as u64 / channels, Ordering::Relaxed);
        if tone.finished() {
            finished_flag.store(true, Ordering::Relaxed);
        }
    });

    let id = DeviceId::new(info.id.as_str());
    let stream = match backend.open_output(&id, config, callback) {
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
    println!(
        "tone:     {} Hz, {} dBFS, {} s",
        args.freq, args.level_db, args.seconds
    );

    let start = Instant::now();
    let mut last_callbacks = 0;
    let mut last_progress = Instant::now();
    let mut frames_shown = false;
    while !finished.load(Ordering::Relaxed) {
        thread::sleep(INTERVAL);

        if let Some(e) = stream.error() {
            eprintln!("error: output stream stopped: {e}");
            return ExitCode::FAILURE;
        }
        let callbacks = stream.callbacks();
        if callbacks != last_callbacks {
            last_callbacks = callbacks;
            last_progress = Instant::now();
        } else if last_progress.elapsed() >= STALL {
            eprintln!(
                "error: output stopped: no callback for {} s (device unplugged?)",
                STALL.as_secs()
            );
            return ExitCode::FAILURE;
        }
        if !frames_shown && callbacks > 0 {
            println!("callback: {} frames", stream.callback_frames());
            frames_shown = true;
        }
    }
    // フェードアウトの後の無音がデバイスに届くまで待ってから止める。
    thread::sleep(INTERVAL);

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
