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
//! With `--bgm FILE` it plays a song through the BGM channel (#63), read a
//! little at a time by a worker thread: the same, to measure the CPU load,
//! the xruns and the underruns of the BGM. `--bgm-loop` starts the song
//! again when it ends, and `--bgm-cycle` pauses and stops it again and again
//! to check those on a real device. Without `--bgm` nothing is printed or
//! done for the BGM.
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

use crate::bgm_worker::{Bgm, BgmFailure};
use crate::cli::RunArgs;
use crate::session::{AudioSession, SessionOptions, choose_devices};

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
/// How often the line of the BGM is printed.
const BGM_STATUS: Duration = Duration::from_secs(1);
/// How long `--bgm-cycle` keeps the song paused before it plays again.
const CYCLE_PAUSE: Duration = Duration::from_secs(1);
/// How long to wait after the last pause, so that its fade-out (10 ms) is
/// played before the streams stop.
const BGM_EXIT_WAIT: Duration = Duration::from_millis(30);

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
    let bgm_info = match args.bgm.as_deref().map(check_bgm_file) {
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
    if args.bgm.is_some() {
        control.bgm.set_gain_db(args.bgm_volume);
    }

    let options = SessionOptions {
        bgm: args.bgm.is_some(),
    };
    let mut session = match AudioSession::start_with(backend, input, output, &control, options) {
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

    let mut bgm_watch = None;
    if let (Some(path), Some(info)) = (args.bgm.as_deref(), bgm_info.as_ref()) {
        let cycle = args.bgm_cycle.map(duration_from_seconds);
        let Some(bgm) = session.bgm() else {
            drop(session);
            return fail("the BGM was asked for, but the session has none");
        };
        // The song is opened by the worker when the flush of `load` is done;
        // a failure comes through `take_error` in the monitor. `play` has to
        // come after `load`, because the flush pauses.
        bgm.load(path);
        bgm.controller().set_looping(args.bgm_loop);
        bgm.controller().play();
        let loaded = LoadedBgm {
            name: display_name(path),
            duration: info.duration(),
            file_rate: info.sample_rate(),
            output_rate: session.sample_rate(),
        };
        println!(
            "{}",
            format_bgm_file_line(
                &loaded,
                control.bgm.gain_db(),
                args.bgm_loop,
                args.bgm_cycle
            )
        );
        bgm_watch = Some(BgmWatch::new(cycle.map(Cycle::new)));
    }

    let result = monitor(
        &mut session,
        &control,
        Duration::from_secs(args.seconds),
        se,
        bgm_watch.as_mut(),
    );
    if let (Some(watch), Some(bgm)) = (bgm_watch.as_mut(), session.bgm()) {
        watch.finish(bgm, result.is_ok());
    }
    print_summary(&session, se.is_some(), bgm_watch.as_ref());
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
/// and then every `se_every`. With `bgm`, the song is watched: its failures,
/// its end, the `--bgm-cycle` steps, and a line of its state every second.
fn monitor(
    session: &mut AudioSession,
    control: &MixerControl,
    duration: Duration,
    se_every: Option<Duration>,
    mut bgm: Option<&mut BgmWatch>,
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
        if let (Some(watch), Some(song)) = (bgm.as_deref_mut(), session.bgm()) {
            watch.poll(song, session.sample_rate(), start.elapsed());
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
                    bgm.as_ref().map(|_| control.bgm.meter().take()),
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

/// Formats the line of levels printed while running. Without `bgm` and `se`
/// it is the line of Phase 1.
fn format_levels_line(
    mic: f32,
    bgm: Option<f32>,
    se: Option<f32>,
    master: f32,
    buffer_ms: f64,
    underruns: u64,
    overruns: u64,
) -> String {
    let bgm = bgm.map_or(String::new(), |peak| {
        format!(" | BGM {} dBFS", format_level(peak))
    });
    let se = se.map_or(String::new(), |peak| {
        format!(" | SE {} dBFS", format_level(peak))
    });
    format!(
        "MIC {} dBFS{bgm}{se} | MASTER {} dBFS | buffer {buffer_ms:5.1} ms | under {underruns} over {overruns}",
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

/// What `--bgm` is about to play, from what the file says.
struct LoadedBgm {
    name: String,
    /// `None` when the file does not say how long it is.
    duration: Option<Duration>,
    file_rate: u32,
    output_rate: u32,
}

/// Formats the `bgm:` line printed before the levels start.
fn format_bgm_file_line(
    loaded: &LoadedBgm,
    volume_db: f32,
    looping: bool,
    cycle: Option<f64>,
) -> String {
    let length = loaded.duration.map_or("length unknown".to_string(), |d| {
        format!("{:.2} s", d.as_secs_f64())
    });
    let rate = if loaded.file_rate == loaded.output_rate {
        format!("{} Hz", loaded.output_rate)
    } else {
        format!("{} Hz -> {} Hz", loaded.file_rate, loaded.output_rate)
    };
    let looping = if looping { "loop on" } else { "loop off" };
    let cycle = cycle.map_or(String::new(), |seconds| format!(", cycle {seconds} s"));
    format!(
        "bgm:      {}: {length}, {rate}, volume {volume_db:.1} dB, {looping}{cycle}",
        loaded.name
    )
}

/// Reads what the file says about its audio, before anything is opened. A
/// file that cannot be read or is not WAV or MP3 is an error. There is no
/// limit of length: a song is read a little at a time.
fn check_bgm_file(path: &Path) -> Result<SoundInfo, String> {
    probe(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// What `--bgm-cycle` asks the player to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleAction {
    Pause,
    Stop,
    Play,
}

impl CycleAction {
    fn name(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Stop => "stop",
            Self::Play => "play",
        }
    }
}

/// The steps of `--bgm-cycle`: every `every`, pause (the first time, the
/// third, ...) or stop (the second, the fourth, ...), and play again
/// [`CYCLE_PAUSE`] later. `every` is more than that (the command line makes
/// sure), so a step ends before the next one starts.
#[derive(Debug)]
struct Cycle {
    every: Duration,
    /// How many steps were started.
    steps: u64,
    /// When the song is to play again, while a step is in progress.
    resume_at: Option<Duration>,
}

impl Cycle {
    fn new(every: Duration) -> Self {
        Self {
            every,
            steps: 0,
            resume_at: None,
        }
    }

    /// Returns the next action that is due at `elapsed` (the time since the
    /// start), or `None`. Call it until it gives `None`, because a late call
    /// can have more than one due.
    fn due(&mut self, elapsed: Duration) -> Option<CycleAction> {
        if let Some(at) = self.resume_at
            && elapsed >= at
        {
            self.resume_at = None;
            return Some(CycleAction::Play);
        }
        if self.resume_at.is_some() {
            return None;
        }
        let number = self.steps.saturating_add(1);
        let start = duration_from_seconds(self.every.as_secs_f64() * number as f64);
        if elapsed < start {
            return None;
        }
        self.steps = number;
        self.resume_at = Some(start.saturating_add(CYCLE_PAUSE));
        Some(if number % 2 == 1 {
            CycleAction::Pause
        } else {
            CycleAction::Stop
        })
    }
}

/// Formats the line printed for a step of `--bgm-cycle`.
fn format_cycle_line(action: CycleAction, elapsed: Duration) -> String {
    format!(
        "cycle:    {} at {:.1} s",
        action.name(),
        elapsed.as_secs_f64()
    )
}

/// Whether the end of the song is announced now: it has finished, a flush
/// (Stop) is not in progress (the flag of the last song is still set until
/// the audio thread has thrown it away), the song did not fail, and the end
/// was not announced already.
fn should_announce_end(finished: bool, flushed: bool, failed: bool, announced: bool) -> bool {
    finished && flushed && !failed && !announced
}

/// Formats the line printed when a song fails.
fn format_bgm_failure(failure: &BgmFailure) -> String {
    format!("BGM stopped: {failure}")
}

/// What the player is doing, for the line of its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgmState {
    Playing,
    Paused,
    Finished,
}

impl BgmState {
    fn new(finished: bool, wants_play: bool) -> Self {
        if finished {
            Self::Finished
        } else if wants_play {
            Self::Playing
        } else {
            Self::Paused
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Finished => "finished",
        }
    }
}

/// The values of the line of the state of the BGM.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BgmStatus {
    state: BgmState,
    position_frames: u64,
    /// `None` when the length of the song is not known.
    total_frames: Option<u64>,
    fill_frames: u32,
    min_fill_frames: u32,
    underruns: u64,
    loops: u64,
    skipped_packets: u64,
    sample_rate: u32,
}

/// Formats the line of the state of the BGM, printed every second.
fn format_bgm_status_line(status: &BgmStatus) -> String {
    let rate = f64::from(status.sample_rate);
    let position = status.position_frames as f64 / rate;
    let total = status
        .total_frames
        .map_or("?".to_string(), |t| format!("{:.1}", t as f64 / rate));
    format!(
        "bgm:      {} | pos {position:.1} / {total} s | buffer {:.1} ms (min {:.1} ms) | under {} | loops {} | skipped {}",
        status.state.name(),
        frames_to_ms(f64::from(status.fill_frames), status.sample_rate),
        frames_to_ms(f64::from(status.min_fill_frames), status.sample_rate),
        status.underruns,
        status.loops,
        status.skipped_packets,
    )
}

/// The values of the line of the BGM in the summary.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BgmSummary {
    underruns: u64,
    underrun_frames: u64,
    min_fill_frames: u32,
    played_frames: u64,
    loops: u64,
    skipped_packets: u64,
    finished: bool,
    failures: u64,
    sample_rate: u32,
}

/// Formats the line of the BGM in the summary.
fn format_bgm_summary_line(summary: &BgmSummary) -> String {
    let rate = summary.sample_rate;
    format!(
        "  BGM:             underruns {} ({} frames), min buffer {} frames ({:.1} ms), played {} frames ({:.1} s), loops {}, skipped packets {}, finished {}, failures {}",
        summary.underruns,
        summary.underrun_frames,
        summary.min_fill_frames,
        frames_to_ms(f64::from(summary.min_fill_frames), rate),
        summary.played_frames,
        frames_to_ms(summary.played_frames as f64, rate) / 1000.0,
        summary.loops,
        summary.skipped_packets,
        if summary.finished { "yes" } else { "no" },
        summary.failures,
    )
}

/// What `run` keeps while it watches the song.
struct BgmWatch {
    cycle: Option<Cycle>,
    /// The least that the ring buffer held, in frames, over the whole run.
    min_fill_frames: u32,
    /// How many failures were taken from the player and shown. A failure
    /// that the worker dropped because an older one had not been taken yet
    /// is not counted.
    failures: u64,
    /// A failure was shown since the song was last started.
    failed: bool,
    end_announced: bool,
    /// When the line of the state is printed next.
    next_status: Duration,
}

impl BgmWatch {
    fn new(cycle: Option<Cycle>) -> Self {
        Self {
            cycle,
            min_fill_frames: u32::MAX,
            failures: 0,
            failed: false,
            end_announced: false,
            next_status: BGM_STATUS,
        }
    }

    /// One look at the song: the steps of `--bgm-cycle` that are due, a
    /// failure, the end of the song, and (every second) the line of its state.
    fn poll(&mut self, bgm: &Bgm, sample_rate: u32, elapsed: Duration) {
        let controller = bgm.controller();
        while let Some(action) = self.cycle.as_mut().and_then(|cycle| cycle.due(elapsed)) {
            match action {
                CycleAction::Pause => controller.pause(),
                CycleAction::Stop => {
                    bgm.stop();
                    // The song starts again: a new end, a new failure.
                    self.failed = false;
                    self.end_announced = false;
                }
                CycleAction::Play => controller.play(),
            }
            println!("{}", format_cycle_line(action, elapsed));
        }

        self.show_failure(bgm);

        let announce = should_announce_end(
            controller.is_finished(),
            controller.is_flushed(),
            self.failed,
            self.end_announced,
        );
        if announce {
            self.end_announced = true;
            println!(
                "bgm:      the song ended (played {:.1} s)",
                frames_to_ms(controller.stats().played_frames as f64, sample_rate) / 1000.0
            );
        }

        if elapsed >= self.next_status {
            self.next_status = self.next_status.saturating_add(BGM_STATUS);
            let stats = controller.stats();
            let min_fill = controller.take_min_fill_frames();
            self.min_fill_frames = self.min_fill_frames.min(min_fill);
            let status = BgmStatus {
                state: BgmState::new(stats.finished, controller.wants_play()),
                position_frames: controller.position_frames(),
                total_frames: controller.total_frames(),
                fill_frames: stats.fill_frames,
                min_fill_frames: min_fill,
                underruns: stats.underruns,
                loops: stats.loops,
                skipped_packets: stats.skipped_packets,
                sample_rate,
            };
            println!("{}", format_bgm_status_line(&status));
        }
    }

    /// Prints a failure of the song, if there is one, to stderr (once: it is
    /// taken from the player).
    fn show_failure(&mut self, bgm: &Bgm) {
        if let Some(failure) = bgm.take_error() {
            eprintln!("{}", format_bgm_failure(&failure));
            self.failures += 1;
            self.failed = true;
        }
    }

    /// The last look, after the monitor: a failure of the last moments, the
    /// least buffer, and (if the streams are fine) a pause that is played to
    /// its end, so that the streams do not stop in the middle of a sound.
    fn finish(&mut self, bgm: &Bgm, streams_fine: bool) {
        self.show_failure(bgm);
        let min_fill = bgm.controller().take_min_fill_frames();
        self.min_fill_frames = self.min_fill_frames.min(min_fill);
        if streams_fine {
            bgm.controller().pause();
            thread::sleep(BGM_EXIT_WAIT);
        }
    }

    fn summary(&self, bgm: &Bgm, sample_rate: u32) -> BgmSummary {
        let stats = bgm.controller().stats();
        BgmSummary {
            underruns: stats.underruns,
            underrun_frames: stats.underrun_frames,
            min_fill_frames: self.min_fill_frames,
            played_frames: stats.played_frames,
            loops: stats.loops,
            skipped_packets: stats.skipped_packets,
            finished: stats.finished,
            failures: self.failures,
            sample_rate,
        }
    }
}

/// Prints the values that ADR-0007 asks to record.
fn print_summary(session: &AudioSession, with_se: bool, bgm: Option<&BgmWatch>) {
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
    if let (Some(watch), Some(song)) = (bgm, session.bgm()) {
        let summary = watch.summary(song, rate);
        println!("{}", format_bgm_summary_line(&summary));
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
            format_levels_line(0.5, None, None, 1.0, 21.333, 3, 4),
            "MIC   -6.0 dBFS | MASTER    0.0 dBFS | buffer  21.3 ms | under 3 over 4"
        );
    }

    #[test]
    fn the_levels_line_with_se_adds_it_between_mic_and_master() {
        assert_eq!(
            format_levels_line(0.5, None, Some(0.25), 1.0, 21.333, 0, 0),
            "MIC   -6.0 dBFS | SE  -12.0 dBFS | MASTER    0.0 dBFS | buffer  21.3 ms | under 0 over 0"
        );
        assert!(
            format_levels_line(0.0, None, Some(0.0), 0.0, 0.0, 0, 0).contains("SE   -inf dBFS")
        );
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

    #[test]
    fn the_levels_line_with_bgm_puts_it_between_mic_and_se() {
        assert_eq!(
            format_levels_line(0.5, Some(0.25), None, 1.0, 21.333, 0, 0),
            "MIC   -6.0 dBFS | BGM  -12.0 dBFS | MASTER    0.0 dBFS | buffer  21.3 ms | under 0 over 0"
        );
        assert_eq!(
            format_levels_line(0.5, Some(0.25), Some(0.125), 1.0, 21.333, 0, 0),
            "MIC   -6.0 dBFS | BGM  -12.0 dBFS | SE  -18.1 dBFS | MASTER    0.0 dBFS | buffer  21.3 ms | under 0 over 0"
        );
        assert!(
            format_levels_line(0.0, Some(0.0), None, 0.0, 0.0, 0, 0).contains("BGM   -inf dBFS")
        );
    }

    fn loaded_bgm() -> LoadedBgm {
        LoadedBgm {
            name: "song.mp3".to_string(),
            duration: Some(Duration::from_millis(192_400)),
            file_rate: 44_100,
            output_rate: 48_000,
        }
    }

    #[test]
    fn the_bgm_file_line_shows_the_file_the_rates_and_the_options() {
        assert_eq!(
            format_bgm_file_line(&loaded_bgm(), -6.0, true, Some(10.0)),
            "bgm:      song.mp3: 192.40 s, 44100 Hz -> 48000 Hz, volume -6.0 dB, loop on, cycle 10 s"
        );
        let same_rate = LoadedBgm {
            file_rate: 48_000,
            duration: None,
            ..loaded_bgm()
        };
        assert_eq!(
            format_bgm_file_line(&same_rate, 0.0, false, None),
            "bgm:      song.mp3: length unknown, 48000 Hz, volume 0.0 dB, loop off"
        );
    }

    fn status() -> BgmStatus {
        BgmStatus {
            state: BgmState::Playing,
            position_frames: 590_400,
            total_frames: Some(9_235_200),
            fill_frames: 47_386,
            min_fill_frames: 30_720,
            underruns: 0,
            loops: 2,
            skipped_packets: 1,
            sample_rate: 48_000,
        }
    }

    #[test]
    fn the_bgm_status_line_shows_the_state_the_position_and_the_counters() {
        assert_eq!(
            format_bgm_status_line(&status()),
            "bgm:      playing | pos 12.3 / 192.4 s | buffer 987.2 ms (min 640.0 ms) | under 0 | loops 2 | skipped 1"
        );
        let paused = BgmStatus {
            state: BgmState::Paused,
            total_frames: None,
            ..status()
        };
        let line = format_bgm_status_line(&paused);
        assert!(
            line.starts_with("bgm:      paused | pos 12.3 / ? s |"),
            "{line}"
        );
        let finished = BgmStatus {
            state: BgmState::Finished,
            ..status()
        };
        assert!(format_bgm_status_line(&finished).contains("finished"));
    }

    #[test]
    fn the_state_of_the_bgm_is_finished_before_playing_before_paused() {
        assert_eq!(BgmState::new(true, true), BgmState::Finished);
        assert_eq!(BgmState::new(true, false), BgmState::Finished);
        assert_eq!(BgmState::new(false, true), BgmState::Playing);
        assert_eq!(BgmState::new(false, false), BgmState::Paused);
    }

    #[test]
    fn the_bgm_summary_line_lists_the_counters() {
        let summary = BgmSummary {
            underruns: 1,
            underrun_frames: 480,
            min_fill_frames: 38_400,
            played_frames: 9_216_000,
            loops: 3,
            skipped_packets: 0,
            finished: false,
            failures: 2,
            sample_rate: 48_000,
        };
        assert_eq!(
            format_bgm_summary_line(&summary),
            "  BGM:             underruns 1 (480 frames), min buffer 38400 frames (800.0 ms), played 9216000 frames (192.0 s), loops 3, skipped packets 0, finished no, failures 2"
        );
        let finished = BgmSummary {
            finished: true,
            ..summary
        };
        assert!(format_bgm_summary_line(&finished).contains("finished yes"));
    }

    #[test]
    fn a_failure_is_shown_as_one_line() {
        let failure = BgmFailure {
            path: Some(std::path::PathBuf::from("music/a.mp3")),
            message: "the audio file has no sound".to_string(),
        };
        assert_eq!(
            format_bgm_failure(&failure),
            "BGM stopped: music/a.mp3: the audio file has no sound"
        );
    }

    #[test]
    fn the_end_of_the_song_is_announced_once_and_not_after_a_failure_or_during_a_stop() {
        assert!(should_announce_end(true, true, false, false));
        assert!(
            !should_announce_end(false, true, false, false),
            "not finished"
        );
        assert!(
            !should_announce_end(true, false, false, false),
            "a stop is in progress"
        );
        assert!(!should_announce_end(true, true, true, false), "it failed");
        assert!(
            !should_announce_end(true, true, false, true),
            "already announced"
        );
    }

    fn secs(seconds: f64) -> Duration {
        Duration::from_secs_f64(seconds)
    }

    /// All the actions that are due at `elapsed`.
    fn due_all(cycle: &mut Cycle, elapsed: Duration) -> Vec<CycleAction> {
        let mut actions = Vec::new();
        while let Some(action) = cycle.due(elapsed) {
            actions.push(action);
            assert!(actions.len() < 100, "never stops");
        }
        actions
    }

    #[test]
    fn the_cycle_pauses_then_stops_and_plays_again_after_a_second() {
        use CycleAction::{Pause, Play, Stop};
        let mut cycle = Cycle::new(secs(3.0));
        assert_eq!(due_all(&mut cycle, secs(0.0)), vec![]);
        assert_eq!(due_all(&mut cycle, secs(2.9)), vec![]);
        assert_eq!(due_all(&mut cycle, secs(3.0)), vec![Pause]);
        assert_eq!(due_all(&mut cycle, secs(3.9)), vec![]);
        assert_eq!(due_all(&mut cycle, secs(4.0)), vec![Play]);
        assert_eq!(due_all(&mut cycle, secs(5.9)), vec![]);
        assert_eq!(due_all(&mut cycle, secs(6.0)), vec![Stop]);
        assert_eq!(due_all(&mut cycle, secs(7.0)), vec![Play]);
        assert_eq!(due_all(&mut cycle, secs(9.0)), vec![Pause]);
        assert_eq!(due_all(&mut cycle, secs(10.0)), vec![Play]);
        assert_eq!(due_all(&mut cycle, secs(12.0)), vec![Stop]);
        assert_eq!(due_all(&mut cycle, secs(13.0)), vec![Play]);
    }

    #[test]
    fn a_stop_is_always_followed_by_a_play() {
        use CycleAction::{Play, Stop};
        let mut cycle = Cycle::new(secs(2.5));
        let mut actions = Vec::new();
        for tenth in 0..1_000 {
            actions.extend(due_all(&mut cycle, secs(f64::from(tenth) / 10.0)));
        }
        assert!(actions.contains(&Stop));
        for (n, action) in actions.iter().enumerate() {
            if *action == Stop {
                // The last action may be a Stop whose Play is not due yet.
                assert!(
                    actions.get(n + 1).is_none_or(|next| *next == Play),
                    "a stop was not followed by a play"
                );
            }
        }
    }

    #[test]
    fn a_late_check_gives_the_actions_in_order_without_repeating_them() {
        use CycleAction::{Pause, Play, Stop};
        let mut cycle = Cycle::new(secs(3.0));
        // Nothing was checked for 10 s: the first step and its play are due,
        // and so is the second step (at 6 s), but its play is not (7 s).
        assert_eq!(due_all(&mut cycle, secs(6.5)), vec![Pause, Play, Stop]);
        assert_eq!(due_all(&mut cycle, secs(6.5)), vec![]);
        assert_eq!(due_all(&mut cycle, secs(7.0)), vec![Play]);
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
    fn a_song_can_be_checked_and_has_no_limit_of_length() {
        let song = TempWav::new("bgm-ok", 8_000, 8_000);
        let info = check_bgm_file(&song.0).unwrap();
        assert_eq!(info.sample_rate(), 8_000);
        assert_eq!(info.duration(), Some(Duration::from_secs(1)));

        // 61 s: too long for a sound effect, fine for a song.
        let long = TempWav::new("bgm-long", 8_000, 8_000 * 61);
        assert!(check_se_file(&long.0).is_err());
        assert!(check_bgm_file(&long.0).is_ok());
    }

    #[test]
    fn a_song_that_cannot_be_played_fails_the_check_and_names_the_file() {
        let missing = std::env::temp_dir().join("ink-mixer-bgm-does-not-exist.wav");
        let error = check_bgm_file(&missing).unwrap_err();
        assert!(
            error.contains("ink-mixer-bgm-does-not-exist.wav"),
            "{error}"
        );

        let text =
            std::env::temp_dir().join(format!("ink-mixer-{}-bgm-text.wav", std::process::id()));
        std::fs::write(&text, b"this is not audio").unwrap();
        let result = check_bgm_file(&text);
        let _ = std::fs::remove_file(&text);
        assert!(result.is_err());

        // The header says 0 Hz.
        let zero = TempWav::new("bgm-zero", 0, 100);
        let error = check_bgm_file(&zero.0).unwrap_err();
        assert!(
            error.contains("does not say a valid sample rate"),
            "{error}"
        );
        assert!(error.contains("bgm-zero"), "{error}");
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
