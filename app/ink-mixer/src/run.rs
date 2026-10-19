//! `ink-mixer run`: play MIC -> Gain -> MASTER -> output (#38, ADR-0007).
//!
//! Opening the streams is shared with the window (`session.rs`). This module
//! chooses the levels from the arguments, prints the levels while running,
//! and prints the values ADR-0007 asks to record at the end.
//!
//! With `--se FILE` it also plays a sound effect again and again through the
//! SE channel (#62), to measure the CPU load and the xruns without the
//! window. Without `--se` nothing is printed or done for the SE.
//!
//! stdout gets only the levels and the results; logs (warnings, errors) go
//! to stderr (AGENTS.md §10).

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ink_backend::{AudioBackend, DeviceInfo};
use ink_core::Config;
use ink_core::graph::{MixerControl, SILENCE_DB, SePolicy, SoundData, gain_to_db};
use ink_media::{SoundInfo, load_sound, probe};
use tracing::{error, warn};

use crate::cli::RunArgs;
use crate::session::{AudioSession, choose_devices};

/// How often the streams are checked.
const POLL: Duration = Duration::from_millis(100);
/// How often the levels are printed.
const DISPLAY: Duration = Duration::from_millis(500);
/// The longest sound effect that `--se` loads (Issue 62).
const SE_MAX_DURATION: Duration = Duration::from_secs(60);
/// The slot of the SE player that holds the sound.
const SE_SLOT: usize = 0;
/// The most plays sent in one check. If the loop was late for longer, the
/// plays that are further behind are skipped, not sent in a burst.
const MAX_PLAYS_PER_CHECK: u64 = 50;
/// The shortest wait between two checks while a sound effect plays.
const MIN_POLL: Duration = Duration::from_millis(2);

/// Runs `ink-mixer run`. Exits with 0 when it finishes, or 1 when the
/// streams cannot be opened or stop while running.
pub fn run(backend: &dyn AudioBackend, config: &Config, args: &RunArgs) -> ExitCode {
    warn!(
        "the microphone is played through the output as is; \
         use headphones or a headset (speakers cause feedback)"
    );

    // Look at the file before the streams are opened, so that a file that
    // cannot be played does not make the MIC play for a moment first.
    let se_info = match args.se.as_deref().map(check_se_file) {
        Some(Ok(info)) => Some(info),
        Some(Err(e)) => return fail(&e),
        None => None,
    };

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
    if args.se.is_some() {
        control.se.set_gain_db(args.se_volume);
    }

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

    let every = duration_from_seconds(args.se_every);
    if let (Some(path), Some(info)) = (args.se.as_deref(), se_info.as_ref()) {
        let line = match install_se(&mut session, path, info) {
            Ok(loaded) => format_se_line(&loaded, every, control.se.gain_db()),
            Err(e) => {
                drop(session);
                return fail(&e);
            }
        };
        println!("{line}");
    }
    let se = args.se.is_some().then_some(every);

    let result = monitor(
        &mut session,
        &control,
        Duration::from_secs(args.seconds),
        se,
    );
    print_summary(&session, se.is_some());
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
///
/// With `se_every`, the sound effect in the SE slot is played at the start
/// and then every `se_every`.
fn monitor(
    session: &mut AudioSession,
    control: &MixerControl,
    duration: Duration,
    se_every: Option<Duration>,
) -> Result<(), String> {
    let start = Instant::now();
    let mut last_display = start;
    let mut plays_made = 0_u64;
    // Check more often than the interval of the plays, so they are on time.
    let poll = se_every.map_or(POLL, |every| (every / 4).clamp(MIN_POLL, POLL));

    while start.elapsed() < duration {
        if let Some(every) = se_every {
            let due = plays_due(start.elapsed(), every, plays_made);
            for _ in 0..due.min(MAX_PLAYS_PER_CHECK) {
                // A refusal (Busy, QueueFull) is counted in the SE stats
                // and shown in the summary.
                let _ = session.se_mut().play(SE_SLOT, 1.0, SePolicy::Overlap);
            }
            plays_made = plays_made.saturating_add(due);
        }
        thread::sleep(poll);
        session.check(Instant::now())?;

        if last_display.elapsed() >= DISPLAY {
            last_display = Instant::now();
            let stats = session.stats();
            println!(
                "{}",
                format_levels_line(
                    control.mic.meter().take(),
                    se_every.map(|_| control.se.meter().take()),
                    control.master.meter().take(),
                    frames_to_ms(f64::from(stats.fill_frames()), session.sample_rate()),
                    stats.underruns(),
                    stats.overruns()
                )
            );
        }
    }
    Ok(())
}

/// Converts seconds to a `Duration`; a value too large for it becomes the
/// longest one.
fn duration_from_seconds(seconds: f64) -> Duration {
    Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX)
}

/// Returns how many plays are due at `elapsed` and not made yet. Plays start
/// at 0 and then every `every`: at `elapsed` there should have been
/// `floor(elapsed / every) + 1`.
fn plays_due(elapsed: Duration, every: Duration, made: u64) -> u64 {
    let due = (elapsed.as_secs_f64() / every.as_secs_f64()).floor();
    // `as` saturates, and turns NaN (0 / 0) into 0.
    (due as u64).saturating_add(1).saturating_sub(made)
}

/// Formats the line of levels printed while running. Without `se` it is the
/// line of Phase 1.
fn format_levels_line(
    mic: f32,
    se: Option<f32>,
    master: f32,
    buffer_ms: f64,
    underruns: u64,
    overruns: u64,
) -> String {
    let se = se.map_or(String::new(), |peak| {
        format!(" | SE {} dBFS", format_level(peak))
    });
    format!(
        "MIC {} dBFS{se} | MASTER {} dBFS | buffer {buffer_ms:5.1} ms | under {underruns} over {overruns}",
        format_level(mic),
        format_level(master),
    )
}

/// What `--se` loaded.
struct LoadedSe {
    name: String,
    duration: Duration,
    file_rate: u32,
    output_rate: u32,
    load_time: Duration,
}

/// Formats the `se:` line printed before the levels start.
fn format_se_line(loaded: &LoadedSe, every: Duration, volume_db: f32) -> String {
    let rate = if loaded.file_rate == loaded.output_rate {
        format!("{} Hz", loaded.output_rate)
    } else {
        format!("{} Hz -> {} Hz", loaded.file_rate, loaded.output_rate)
    };
    format!(
        "se:       {}: {:.2} s, {rate} (loaded in {} ms), every {} s, volume {volume_db:.1} dB",
        loaded.name,
        loaded.duration.as_secs_f64(),
        loaded.load_time.as_millis(),
        every.as_secs_f64()
    )
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Reads what the file says about its audio, before anything is opened. A
/// file that cannot be read, is not WAV or MP3, or says it is longer than
/// [`SE_MAX_DURATION`] is an error.
fn check_se_file(path: &Path) -> Result<SoundInfo, String> {
    let info = probe(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if info.duration().is_some_and(|d| d > SE_MAX_DURATION) {
        return Err(format!(
            "{}: the audio is longer than the limit of {} s",
            path.display(),
            SE_MAX_DURATION.as_secs()
        ));
    }
    Ok(info)
}

/// Decodes the file and resamples it to `rate`. Returns the sound, how long
/// it took, and how many packets were skipped.
fn load_se_sound(path: &Path, rate: u32) -> Result<(SoundData, Duration, u64), String> {
    let started = Instant::now();
    let decoded = load_sound(path, rate, Some(SE_MAX_DURATION))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((decoded.sound, started.elapsed(), decoded.skipped_packets))
}

/// Loads the sound effect for the rate of the session and puts it in the SE
/// slot. The file is read here, on the control thread, never on the audio
/// thread. `ink-media` does not log, so the warning about packets that could
/// not be decoded is given here, once.
fn install_se(
    session: &mut AudioSession,
    path: &Path,
    info: &SoundInfo,
) -> Result<LoadedSe, String> {
    let output_rate = session.sample_rate();
    let (sound, load_time, skipped) = load_se_sound(path, output_rate)?;
    if skipped >= 1 {
        warn!(
            "{}: {skipped} packet(s) could not be decoded and were skipped",
            path.display()
        );
    }
    let duration = sound.duration();
    session
        .se_mut()
        .set_sound(SE_SLOT, Arc::new(sound))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(LoadedSe {
        name: display_name(path),
        duration,
        file_rate: info.sample_rate(),
        output_rate,
        load_time,
    })
}

/// Prints the values that ADR-0007 asks to record.
fn print_summary(session: &AudioSession, with_se: bool) {
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
    if with_se {
        let se = session.se().stats();
        println!(
            "  SE:              started {}, ignored {}, dropped {}, commands dropped {}, busy {}, leaked {}",
            se.plays_started,
            se.plays_ignored,
            se.plays_dropped,
            se.commands_dropped,
            se.plays_busy,
            se.leaked
        );
    }
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
    fn plays_start_at_0_and_then_every_interval() {
        let every = Duration::from_millis(500);
        assert_eq!(plays_due(Duration::ZERO, every, 0), 1);
        assert_eq!(plays_due(Duration::from_millis(499), every, 1), 0);
        assert_eq!(plays_due(Duration::from_millis(500), every, 1), 1);
        assert_eq!(plays_due(Duration::from_millis(1100), every, 1), 2);
        assert_eq!(plays_due(Duration::from_secs(10), every, 21), 0);
        // 30 s at one play per 2 s: 16 plays (at 0, 2, ..., 30).
        assert_eq!(
            plays_due(Duration::from_secs(30), Duration::from_secs(2), 0),
            16
        );
    }

    #[test]
    fn a_tiny_interval_does_not_overflow() {
        let tiny = Duration::from_nanos(1);
        assert!(plays_due(Duration::from_secs(60), tiny, 0) > 1_000_000);
        // An interval of 0 (below what a `Duration` can hold) is "always
        // due"; the number of plays per check is capped by the caller.
        assert_eq!(
            plays_due(Duration::from_secs(1), Duration::ZERO, 0),
            u64::MAX
        );
        assert_eq!(plays_due(Duration::ZERO, Duration::ZERO, 0), 1);
        assert_eq!(plays_due(Duration::MAX, tiny, 0), u64::MAX);
    }

    #[test]
    fn seconds_become_durations_and_huge_values_are_capped() {
        assert_eq!(duration_from_seconds(0.5), Duration::from_millis(500));
        assert_eq!(duration_from_seconds(1e300), Duration::MAX);
    }

    #[test]
    fn the_levels_line_without_se_is_the_phase_1_line() {
        assert_eq!(
            format_levels_line(0.5, None, 1.0, 21.333, 3, 4),
            "MIC   -6.0 dBFS | MASTER    0.0 dBFS | buffer  21.3 ms | under 3 over 4"
        );
    }

    #[test]
    fn the_levels_line_with_se_adds_it_between_mic_and_master() {
        assert_eq!(
            format_levels_line(0.5, Some(0.25), 1.0, 21.333, 0, 0),
            "MIC   -6.0 dBFS | SE  -12.0 dBFS | MASTER    0.0 dBFS | buffer  21.3 ms | under 0 over 0"
        );
        assert!(format_levels_line(0.0, Some(0.0), 0.0, 0.0, 0, 0).contains("SE   -inf dBFS"));
    }

    #[test]
    fn the_se_line_shows_the_file_the_rates_and_the_options() {
        let loaded = LoadedSe {
            name: "clap.wav".to_string(),
            duration: Duration::from_millis(2000),
            file_rate: 48_000,
            output_rate: 48_000,
            load_time: Duration::from_millis(5),
        };
        assert_eq!(
            format_se_line(&loaded, Duration::from_secs(2), 0.0),
            "se:       clap.wav: 2.00 s, 48000 Hz (loaded in 5 ms), every 2 s, volume 0.0 dB"
        );
        let resampled = LoadedSe {
            file_rate: 44_100,
            ..loaded
        };
        assert_eq!(
            format_se_line(&resampled, Duration::from_millis(500), -6.0),
            "se:       clap.wav: 2.00 s, 44100 Hz -> 48000 Hz (loaded in 5 ms), every 0.5 s, volume -6.0 dB"
        );
    }

    /// A WAV file of mono 16-bit silence, removed when dropped.
    struct TempWav(std::path::PathBuf);

    impl TempWav {
        fn new(name: &str, rate: u32, frames: usize) -> Self {
            let path =
                std::env::temp_dir().join(format!("ink-mixer-{}-{name}.wav", std::process::id()));
            let data_len = (frames * 2) as u32;
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"RIFF");
            bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
            bytes.extend_from_slice(b"WAVEfmt ");
            bytes.extend_from_slice(&16_u32.to_le_bytes());
            bytes.extend_from_slice(&1_u16.to_le_bytes()); // PCM
            bytes.extend_from_slice(&1_u16.to_le_bytes()); // mono
            bytes.extend_from_slice(&rate.to_le_bytes());
            bytes.extend_from_slice(&(rate * 2).to_le_bytes());
            bytes.extend_from_slice(&2_u16.to_le_bytes());
            bytes.extend_from_slice(&16_u16.to_le_bytes());
            bytes.extend_from_slice(b"data");
            bytes.extend_from_slice(&data_len.to_le_bytes());
            bytes.resize(bytes.len() + frames * 2, 0);
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TempWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_file_that_can_be_played_passes_the_check() {
        let wav = TempWav::new("ok", 8_000, 8_000);
        let info = check_se_file(&wav.0).unwrap();
        assert_eq!(info.sample_rate(), 8_000);
        assert_eq!(info.duration(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn a_missing_file_fails_the_check_and_names_the_file() {
        let path = std::env::temp_dir().join("ink-mixer-does-not-exist.wav");
        let error = check_se_file(&path).unwrap_err();
        assert!(error.contains("ink-mixer-does-not-exist.wav"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_audio_fails_the_check() {
        let path = std::env::temp_dir().join(format!("ink-mixer-{}-text.wav", std::process::id()));
        std::fs::write(&path, b"this is not audio").unwrap();
        let result = check_se_file(&path);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn a_sound_longer_than_60_seconds_fails_the_check() {
        let long = TempWav::new("long", 8_000, 8_000 * 61);
        let error = check_se_file(&long.0).unwrap_err();
        assert!(error.contains("longer than the limit of 60 s"), "{error}");
        let limit = TempWav::new("limit", 8_000, 8_000 * 60);
        assert!(check_se_file(&limit.0).is_ok());
    }

    #[test]
    fn a_sound_is_loaded_at_the_output_rate() {
        let wav = TempWav::new("rate", 8_000, 8_000);
        let (sound, _time, skipped) = load_se_sound(&wav.0, 48_000).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(sound.sample_rate(), 48_000);
        let seconds = sound.duration().as_secs_f64();
        assert!((seconds - 1.0).abs() < 0.01, "{seconds}");
    }

    #[test]
    fn loading_fails_for_a_sound_over_the_limit() {
        let long = TempWav::new("loadlong", 8_000, 8_000 * 61);
        let error = load_se_sound(&long.0, 48_000).unwrap_err();
        assert!(error.contains("longer than the limit"), "{error}");
    }

    #[test]
    fn levels_below_silence_show_as_minus_infinity() {
        assert_eq!(format_level(1.0), "   0.0");
        assert_eq!(format_level(0.5), "  -6.0");
        assert_eq!(format_level(0.0), "  -inf");
        assert_eq!(format_level(0.0005), "  -inf");
    }
}
