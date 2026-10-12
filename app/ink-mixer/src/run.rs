//! `ink-mixer run`: play MIC -> Gain -> MASTER -> output (#38, ADR-0007).
//!
//! Opening the streams is shared with the window (`session.rs`). This module
//! chooses the levels from the arguments, prints the levels while running,
//! and prints the values ADR-0007 asks to record at the end.
//!
//! stdout gets only the levels and the results; logs (warnings, errors) go
//! to stderr (AGENTS.md §10).

use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ink_backend::{AudioBackend, DeviceInfo};
use ink_core::Config;
use ink_core::graph::{MixerControl, SILENCE_DB, gain_to_db};
use tracing::{error, warn};

use crate::cli::RunArgs;
use crate::session::{AudioSession, choose_devices};

/// How often the streams are checked.
const POLL: Duration = Duration::from_millis(100);
/// How often the levels are printed.
const DISPLAY: Duration = Duration::from_millis(500);

/// Runs `ink-mixer run`. Exits with 0 when it finishes, or 1 when the
/// streams cannot be opened or stop while running.
pub fn run(backend: &dyn AudioBackend, config: &Config, args: &RunArgs) -> ExitCode {
    warn!(
        "the microphone is played through the output as is; \
         use headphones or a headset (speakers cause feedback)"
    );

    let (input, output) = match choose_devices(
        backend,
        config,
        args.input.as_deref(),
        args.output.as_deref(),
    ) {
        Ok(devices) => devices,
        Err(e) => return fail(&e),
    };

    let control = Arc::new(MixerControl::new());
    control.mic.set_gain_db(args.gain);
    control.master.set_gain_db(args.master);

    let mut session = match AudioSession::start(backend, input, output, &control) {
        Ok(session) => session,
        Err(e) => return fail(&e),
    };
    print_opened(session.input_device(), session.input(), "input");
    print_opened(session.output_device(), session.output(), "output");
    println!(
        "levels:   gain {:.1} dB, master {:.1} dB, {} s",
        control.mic.gain_db(),
        control.master.gain_db(),
        args.seconds
    );

    let result = monitor(&mut session, &control, Duration::from_secs(args.seconds));
    print_summary(&session);
    // Dropping the session stops the input first, then the output.
    drop(session);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(message: &str) -> ExitCode {
    error!("{message}");
    ExitCode::FAILURE
}

/// Converts frames to milliseconds.
fn frames_to_ms(frames: f64, sample_rate: u32) -> f64 {
    frames * 1000.0 / f64::from(sample_rate)
}

/// Estimated latency in milliseconds: input block + average buffer + output
/// block (ADR-0007).
fn estimated_latency_ms(in_block: u32, average_fill: f64, out_block: u32, sample_rate: u32) -> f64 {
    frames_to_ms(
        f64::from(in_block) + average_fill + f64::from(out_block),
        sample_rate,
    )
}

/// Formats a level (linear) in dBFS; -60 dB and below show as `-inf`.
fn format_level(peak: f32) -> String {
    let db = gain_to_db(peak);
    if db > SILENCE_DB {
        format!("{db:6.1}")
    } else {
        "  -inf".to_string()
    }
}

fn print_opened<D>(device: &DeviceInfo, stream: &ink_backend::AudioStream<D>, side: &str) {
    let config = stream.config();
    println!(
        "{side:<9} {} ({}): {} Hz, {} ch, {}",
        device.name,
        device.id,
        config.sample_rate,
        config.channels,
        stream.sample_format()
    );
}

/// Watches the streams and prints the levels for `duration`. Returns an
/// error when either stream stops.
fn monitor(
    session: &mut AudioSession,
    control: &MixerControl,
    duration: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    let mut last_display = start;

    while start.elapsed() < duration {
        thread::sleep(POLL);
        session.check(Instant::now())?;

        if last_display.elapsed() >= DISPLAY {
            last_display = Instant::now();
            let stats = session.stats();
            println!(
                "MIC {} dBFS | MASTER {} dBFS | buffer {:5.1} ms | under {} over {}",
                format_level(control.mic.meter().take()),
                format_level(control.master.meter().take()),
                frames_to_ms(f64::from(stats.fill_frames()), session.sample_rate()),
                stats.underruns(),
                stats.overruns()
            );
        }
    }
    Ok(())
}

/// Prints the values that ADR-0007 asks to record.
fn print_summary(session: &AudioSession) {
    let rate = session.sample_rate();
    let stats = session.stats();
    let in_block = session.input().callback_frames();
    let out_block = session.output().callback_frames();
    let target = stats.target_frames();
    let average = stats.average_fill_frames();
    println!("summary:");
    println!("  sample rate:     {rate} Hz");
    println!(
        "  callback:        input {in_block} frames ({:.1} ms), output {out_block} frames ({:.1} ms)",
        frames_to_ms(f64::from(in_block), rate),
        frames_to_ms(f64::from(out_block), rate)
    );
    println!(
        "  target:          {target} frames ({:.1} ms)",
        frames_to_ms(f64::from(target), rate)
    );
    println!(
        "  average buffer:  {average:.0} frames ({:.1} ms)",
        frames_to_ms(average, rate)
    );
    println!(
        "  latency (est.):  {:.1} ms (input block + average buffer + output block)",
        estimated_latency_ms(in_block, average, out_block, rate)
    );
    println!(
        "  underruns:       {}\n  overruns:        {}",
        stats.underruns(),
        stats.overruns()
    );
    println!(
        "  backend xruns:   input {}, output {}",
        session.input().xruns(),
        session.output().xruns()
    );
    for (side, denied) in [
        ("input", session.input().realtime_denied()),
        ("output", session.output().realtime_denied()),
    ] {
        if denied {
            warn!("realtime scheduling was denied for the {side} stream");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_adds_blocks_and_average_buffer() {
        assert_eq!(frames_to_ms(480.0, 48_000), 10.0);
        // 1024 + 512 + 1024 frames at 48 kHz.
        let ms = estimated_latency_ms(1024, 512.0, 1024, 48_000);
        assert!((ms - 53.333).abs() < 0.01, "{ms}");
    }

    #[test]
    fn levels_below_silence_show_as_minus_infinity() {
        assert_eq!(format_level(1.0), "   0.0");
        assert_eq!(format_level(0.5), "  -6.0");
        assert_eq!(format_level(0.0), "  -inf");
        assert_eq!(format_level(0.0005), "  -inf");
    }
}
