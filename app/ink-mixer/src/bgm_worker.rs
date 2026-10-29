//! The worker thread of the background music (BGM) player (ADR-0009,
//! Issue 63).
//!
//! The audio thread only reads the ring buffer. This thread fills it: it
//! reads the song with [`SoundStream`] (decode, change of the sample rate,
//! loop), and writes the frames with the [`BgmFeeder`], one chunk of
//! `BGM_WRITE_CHUNK_FRAMES` at a time. Everything that is not allowed on the
//! audio thread (files, decoding, allocation, sleeping, logging) happens
//! here.
//!
//! # Songs and flushes
//!
//! Stop and a change of song both throw away what is in the ring buffer.
//! That is a *flush*: the control side asks for it
//! ([`BgmController::request_flush`]), the worker stops writing and answers
//! (`poll_flush` gives `Waiting`), the audio thread fades out and empties the
//! ring buffer, and then the worker starts the song again from its start, or
//! opens the other song (`Done`). [`Bgm::load`] sends the path to the worker
//! **before** it asks for the flush, so the song is known when the flush is
//! done. The worker keeps nothing of the old position: the chunk it held and
//! its write position are thrown away as soon as the flush is seen.
//!
//! # Errors
//!
//! A song that cannot be opened, or fails while it plays, ends: the worker
//! logs it, tells the player that the song is empty and over (so that it
//! finishes instead of waiting for frames), and records a [`BgmFailure`] for
//! the user. The worker itself goes on. The song is tried again at the next
//! Stop, so a file that was missing for a moment can be played later.
//!
//! # The end of the worker
//!
//! The worker ends when its [`BgmWorker`] is dropped, and only then. Its
//! longest wait is 5 ms, so that takes a few milliseconds.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ink_core::graph::{BgmController, BgmFeeder, FlushPoll};
use ink_media::{MediaError, SoundStream};
use tracing::{debug, warn};

/// How long the worker waits when it has nothing to do (no song, a song that
/// has ended, a flush that is not done).
const IDLE_NAP: Duration = Duration::from_millis(2);
/// How long the worker waits when the ring buffer is full.
const FULL_NAP: Duration = Duration::from_millis(5);

/// What kind of thing went wrong with a song, so that the window can say it
/// in Japanese. The text of the error ([`BgmFailure::message`]) stays English,
/// for logs and bug reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The file is not there (it was deleted or renamed).
    NotFound,
    /// The file is there but cannot be read (no permission, a disk error).
    CannotRead,
    /// The file is not a sound that can be played: not WAV or MP3, no audio
    /// track, or no sound in it.
    NotASound,
    /// The sound broke while it was decoded.
    Broken,
    /// Anything else (a sample rate that cannot be used, a resampling error).
    Other,
}

/// Why a song stopped, for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgmFailure {
    /// The file of the song, if there was one.
    pub path: Option<PathBuf>,
    /// What kind of failure it was.
    pub kind: FailureKind,
    /// What went wrong (the text of the error).
    pub message: String,
}

impl BgmFailure {
    /// One line in Japanese for the window: the song `name` (the display name,
    /// never a path) and why it cannot be played. The English
    /// [`message`](Self::message) is shown beside it as the detail.
    pub fn message_ja(&self, name: &str) -> String {
        let reason = match self.kind {
            FailureKind::NotFound => "ファイルが見つかりません",
            FailureKind::CannotRead => "ファイルを読めません",
            FailureKind::NotASound => "音声として読めません（WAV / MP3 か確認してください）",
            FailureKind::Broken => "音声が壊れていて、続けられません",
            FailureKind::Other => "原因は詳細を見てください",
        };
        format!("「{name}」を再生できません：{reason}")
    }
}

/// The kind of a failure, from the error of the media code.
fn failure_kind(error: &MediaError) -> FailureKind {
    match error {
        MediaError::Io(e) if e.kind() == io::ErrorKind::NotFound => FailureKind::NotFound,
        MediaError::Io(_) => FailureKind::CannotRead,
        MediaError::UnsupportedFormat | MediaError::NoAudioTrack | MediaError::Empty => {
            FailureKind::NotASound
        }
        MediaError::Decode(_) => FailureKind::Broken,
        // `MediaError` may get more kinds; they are "other" until named here.
        _ => FailureKind::Other,
    }
}

impl fmt::Display for BgmFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}", path.display(), self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// The BGM player as the control side uses it: the controller and the worker
/// that fills the ring buffer.
///
/// The fields are in the order they are dropped: the worker is joined first
/// (so the writing end of the ring buffer is gone), then the controller. The
/// reading end of the ring buffer is in the controller after the audio
/// thread has dropped the player, so the memory is freed on the control side.
/// Keep a `Bgm` after the output stream in a struct that owns both.
pub struct Bgm {
    worker: BgmWorker,
    controller: BgmController,
}

impl Bgm {
    /// Starts the worker thread (`ink-bgm`). `controller` and `feeder` come
    /// from [`bgm_channel`](ink_core::graph::bgm_channel); its player is in
    /// the graph of the output.
    ///
    /// # Errors
    ///
    /// Returns an error if the thread cannot be started.
    pub fn start(controller: BgmController, feeder: BgmFeeder) -> io::Result<Self> {
        let worker = BgmWorker::spawn(feeder)?;
        Ok(Self { worker, controller })
    }

    /// Returns the controller: `play`, `pause`, `set_looping`, the counters.
    pub fn controller(&self) -> &BgmController {
        &self.controller
    }

    /// Makes `path` the song: sends the path to the worker and asks for a
    /// flush. It does not wait: the worker opens the file when the flush is
    /// done, and a failure comes through [`take_error`](Self::take_error).
    /// The song is not played until [`BgmController::play`], which has to be
    /// called **after** this (the flush pauses).
    pub fn load(&self, path: &Path) {
        load_song(&self.worker.commands, &self.controller, path, &mut || {});
    }

    /// Stops the song: it goes back to its start (paused).
    pub fn stop(&self) {
        self.controller.request_flush();
    }

    /// Takes the failure of a song, if there is one. The first failure stays
    /// until it is taken: a later one is only logged, not stored over it.
    pub fn take_error(&self) -> Option<BgmFailure> {
        take_failure(&self.worker.failure)
    }
}

/// Sends `path` to the worker and then asks for a flush. The order matters:
/// if the flush came first, the worker could finish it before the path
/// arrives, and start the old song again with the new path left unused.
/// `between` runs between the two (for tests).
fn load_song(
    commands: &Sender<PathBuf>,
    controller: &BgmController,
    path: &Path,
    between: &mut dyn FnMut(),
) {
    // A closed channel means the worker is gone; there is nothing to tell.
    let _ = commands.send(path.to_path_buf());
    between();
    controller.request_flush();
}

fn take_failure(failure: &Mutex<Option<BgmFailure>>) -> Option<BgmFailure> {
    failure
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

/// The handle of the worker thread. Dropping it ends the thread.
struct BgmWorker {
    stop: Arc<AtomicBool>,
    commands: Sender<PathBuf>,
    failure: Arc<Mutex<Option<BgmFailure>>>,
    thread: Option<JoinHandle<()>>,
}

impl BgmWorker {
    fn spawn(feeder: BgmFeeder) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let (commands, receiver) = mpsc::channel();
        let mut worker = Worker::new(feeder, receiver, Arc::clone(&stop), Arc::clone(&failure));
        let thread = thread::Builder::new()
            .name("ink-bgm".to_string())
            .spawn(move || worker.run())?;
        Ok(Self {
            stop,
            commands,
            failure,
            thread: Some(thread),
        })
    }
}

impl Drop for BgmWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("the BGM worker thread panicked");
        }
    }
}

/// The state of the worker thread.
struct Worker {
    feeder: BgmFeeder,
    commands: Receiver<PathBuf>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<BgmFailure>>>,

    /// The song. It stays after an error (dead), to be rewound at the next
    /// Stop.
    stream: Option<SoundStream>,
    /// The file of the song, also when it could not be opened.
    path: Option<PathBuf>,
    /// The song that the next flush starts.
    pending: Option<PathBuf>,
    /// The song failed; nothing is written until the next flush.
    failed: bool,
    /// The last chunk of the song was written and the end was marked.
    eof_sent: bool,

    /// The chunk in hand: `left` and `right`, written up to `at`.
    left: Vec<f32>,
    right: Vec<f32>,
    at: usize,
    have_chunk: bool,
    chunk_looped: bool,
    chunk_ended: bool,

    /// The real length of the song was told to the player.
    length_told: bool,
    /// The skipped packets of this song were logged.
    skipped_logged: bool,
}

impl Worker {
    fn new(
        feeder: BgmFeeder,
        commands: Receiver<PathBuf>,
        stop: Arc<AtomicBool>,
        failure: Arc<Mutex<Option<BgmFailure>>>,
    ) -> Self {
        Self {
            feeder,
            commands,
            stop,
            failure,
            stream: None,
            path: None,
            pending: None,
            failed: false,
            eof_sent: false,
            left: Vec::new(),
            right: Vec::new(),
            at: 0,
            have_chunk: false,
            chunk_looped: false,
            chunk_ended: false,
            length_told: false,
            skipped_logged: false,
        }
    }

    /// Runs until the stop flag is set. Every wait is short, so the flag is
    /// seen within `FULL_NAP` plus the time of one step.
    fn run(&mut self) {
        while !self.stop.load(Ordering::Acquire) {
            if let Some(nap) = self.step() {
                thread::sleep(nap);
            }
        }
    }

    /// One turn of the loop. Returns how long to wait before the next one,
    /// or `None` to go on at once.
    fn step(&mut self) -> Option<Duration> {
        while let Ok(path) = self.commands.try_recv() {
            self.pending = Some(path);
        }

        match self.feeder.poll_flush() {
            FlushPoll::Waiting => {
                // The audio thread is throwing the old sound away. What is in
                // hand belongs to that sound. The discard at `Done` is the one
                // that counts; this one is a safeguard.
                self.discard_chunk();
                return Some(IDLE_NAP);
            }
            FlushPoll::Done => {
                self.discard_chunk();
                self.restart();
                return None;
            }
            FlushPoll::Idle => {}
        }

        if self.stream.is_none() || self.failed || self.eof_sent {
            return Some(IDLE_NAP);
        }

        if !self.have_chunk && !self.fetch_chunk() {
            return None;
        }

        if self.at < self.left.len() {
            self.at += self
                .feeder
                .write(&self.left[self.at..], &self.right[self.at..]);
            if self.at < self.left.len() {
                return Some(FULL_NAP);
            }
        }

        // The chunk is in the ring buffer.
        if self.chunk_looped {
            self.feeder.count_loop();
        }
        if self.chunk_ended {
            self.feeder.mark_eof();
            self.eof_sent = true;
        }
        self.discard_chunk();
        None
    }

    /// Takes the next chunk of the song. Returns `false` if the song failed.
    fn fetch_chunk(&mut self) -> bool {
        let Some(stream) = self.stream.as_mut() else {
            return false;
        };
        self.left.clear();
        self.right.clear();
        self.at = 0;
        let looping = self.feeder.is_looping();
        match stream.next_chunk(looping, &mut self.left, &mut self.right) {
            Ok(chunk) => {
                self.have_chunk = true;
                self.chunk_looped = chunk.looped;
                self.chunk_ended = chunk.ended;
            }
            Err(e) => {
                let path = self.path.clone();
                self.fail(path, &e);
                return false;
            }
        }

        let skipped = stream.take_skipped_packets();
        if skipped > 0 {
            self.feeder.add_skipped_packets(skipped);
            if !self.skipped_logged {
                self.skipped_logged = true;
                warn!("BGM: {skipped} packet(s) of the song could not be decoded and were skipped");
            }
        }
        if !self.length_told
            && let Some(measured) = stream.measured_frames()
        {
            // A song that did not say how long it is (an MP3 without a
            // header), or said it wrong, is known now.
            self.feeder.begin_stream(Some(measured));
            self.length_told = true;
        }
        true
    }

    /// Throws away the chunk in hand and the write position.
    fn discard_chunk(&mut self) {
        self.left.clear();
        self.right.clear();
        self.at = 0;
        self.have_chunk = false;
        self.chunk_looped = false;
        self.chunk_ended = false;
    }

    /// Starts the song after a flush: the pending song, or the same one from
    /// its start.
    fn restart(&mut self) {
        self.failed = false;
        self.eof_sent = false;
        self.length_told = false;
        self.skipped_logged = false;

        if self.pending.is_some() || self.stream.is_none() {
            if let Some(path) = self.pending.take() {
                self.path = Some(path);
                self.stream = None;
            }
            // Nothing was ever loaded: nothing to start.
            let Some(path) = self.path.clone() else {
                return;
            };
            match open_stream(&path, self.feeder.sample_rate()) {
                Ok(stream) => {
                    debug!("BGM: opened {}", path.display());
                    self.stream = Some(stream);
                }
                Err(e) => {
                    self.fail(Some(path), &e);
                    return;
                }
            }
        } else if let Some(stream) = self.stream.as_mut()
            && let Err(e) = stream.rewind()
        {
            let path = self.path.clone();
            self.fail(path, &e);
            return;
        }

        if let Some(stream) = &self.stream {
            let measured = stream.measured_frames();
            self.feeder
                .begin_stream(measured.or(stream.output_frames()));
            self.length_told = measured.is_some();
        }
    }

    /// The song cannot go on. It is over for the player (empty, and its end
    /// marked, so that it finishes), the failure is recorded, and the worker
    /// waits for the next flush.
    ///
    /// The line that the user sees is printed by whoever takes the failure
    /// with [`Bgm::take_error`]; a caller that does not read it sees the
    /// failure only with `RUST_LOG=debug`.
    fn fail(&mut self, path: Option<PathBuf>, error: &MediaError) {
        let message = error.to_string();
        match &path {
            Some(path) => debug!("BGM stopped: {}: {message}", path.display()),
            None => debug!("BGM stopped: {message}"),
        }
        self.feeder.begin_stream(Some(0));
        self.feeder.mark_eof();
        self.failed = true;
        self.eof_sent = true;
        self.discard_chunk();

        // The failure is recorded last: when it can be taken, the end is
        // already marked.
        let mut slot = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(BgmFailure {
                path,
                kind: failure_kind(error),
                message,
            });
        }
    }
}

/// Opens the song for the rate of the output, and checks that it is
/// prepared for that rate.
fn open_stream(path: &Path, rate: u32) -> Result<SoundStream, MediaError> {
    let stream = SoundStream::open(path, rate)?;
    check_rate(stream.output_rate(), rate)?;
    Ok(stream)
}

/// Checks that the frames of the song are at the rate of the ring buffer. A
/// different rate would play the song at the wrong pitch.
fn check_rate(stream_rate: u32, feeder_rate: u32) -> Result<(), MediaError> {
    if stream_rate == feeder_rate {
        Ok(())
    } else {
        Err(MediaError::Resample(format!(
            "the song is prepared for {stream_rate} Hz, but the output runs at {feeder_rate} Hz"
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use ink_core::graph::{BgmPlayer, bgm_channel};

    use super::*;

    /// A small rate keeps the tests short: a fade is 80 frames, priming 800
    /// frames and the ring buffer 8,000 frames (one second).
    const RATE: u32 = 8_000;
    const BLOCK: usize = 64;
    /// Long enough for anything that works; a test that waits this long has
    /// failed.
    const TIMEOUT: Duration = Duration::from_secs(10);

    // -- files --------------------------------------------------------------

    static FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A WAV file in the temporary directory, removed when dropped.
    struct TempWav(PathBuf);

    impl TempWav {
        fn path(name: &str) -> PathBuf {
            let n = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!(
                "ink-mixer-bgm-{}-{n}-{name}.wav",
                std::process::id()
            ))
        }

        /// A mono 16-bit file with these samples.
        fn new(name: &str, rate: u32, samples: &[i16]) -> Self {
            Self::with_claim(name, rate, samples, samples.len())
        }

        /// A file whose header says `claimed` frames, with `samples` in it.
        fn with_claim(name: &str, rate: u32, samples: &[i16], claimed: usize) -> Self {
            let path = Self::path(name);
            let data_len = (claimed * 2) as u32;
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
            for sample in samples {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TempWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// 0.25 and 0.5 of full scale, as they come back from a 16-bit file.
    const QUARTER: i16 = 8_192;
    const HALF: i16 = 16_384;

    fn constant(value: i16, frames: usize) -> Vec<i16> {
        vec![value; frames]
    }

    /// `first` for the first `first_frames` frames, then `second` up to
    /// `total` frames.
    fn step_wave(first: i16, first_frames: usize, second: i16, total: usize) -> Vec<i16> {
        let mut samples = constant(first, first_frames);
        samples.extend(constant(second, total - first_frames));
        samples
    }

    fn missing_path(name: &str) -> PathBuf {
        TempWav::path(name)
    }

    // -- the worker and the audio thread, turn by turn ----------------------

    /// What the audio thread heard: the peak of every block.
    fn peak(player: &mut BgmPlayer) -> f32 {
        let block = player.render(BLOCK);
        block.channel(0).iter().fold(0.0_f32, |p, x| p.max(x.abs()))
    }

    /// The worker and the player on the test thread: one turn of the worker,
    /// then one block of the player, and so on. Nothing waits, and the
    /// order of events is exactly what the test says.
    struct Rig {
        worker: Worker,
        player: BgmPlayer,
        /// The peak of every block rendered so far.
        levels: Vec<f32>,
    }

    impl Rig {
        fn pump(&mut self, rounds: usize) {
            for _ in 0..rounds {
                self.worker.step();
                let level = peak(&mut self.player);
                self.levels.push(level);
            }
        }

        fn steps(&mut self, rounds: usize) {
            for _ in 0..rounds {
                self.worker.step();
            }
        }
    }

    struct Manual {
        commands: Sender<PathBuf>,
        controller: BgmController,
        failure: Arc<Mutex<Option<BgmFailure>>>,
        rig: Rig,
    }

    impl Manual {
        fn new() -> Self {
            let (controller, feeder, player) = bgm_channel(RATE, BLOCK);
            let (commands, receiver) = mpsc::channel();
            let failure = Arc::new(Mutex::new(None));
            let worker = Worker::new(
                feeder,
                receiver,
                Arc::new(AtomicBool::new(false)),
                Arc::clone(&failure),
            );
            Self {
                commands,
                controller,
                failure,
                rig: Rig {
                    worker,
                    player,
                    levels: Vec::new(),
                },
            }
        }

        fn load(&self, path: &Path) {
            load_song(&self.commands, &self.controller, path, &mut || {});
        }

        fn pump(&mut self, rounds: usize) {
            self.rig.pump(rounds);
        }

        /// Pumps until `condition` is true (at most 30,000 rounds).
        fn pump_until(&mut self, what: &str, condition: impl Fn(&Manual) -> bool) {
            for _ in 0..30_000 {
                if condition(self) {
                    return;
                }
                self.rig.pump(1);
            }
            panic!("gave up waiting for: {what}");
        }

        fn levels(&self) -> &[f32] {
            &self.rig.levels
        }

        /// The index of the first block at or after `from` whose peak is at
        /// least `level`.
        fn first_at_least(&self, from: usize, level: f32) -> Option<usize> {
            self.levels()[from..]
                .iter()
                .position(|&p| p >= level)
                .map(|i| from + i)
        }

        fn pump_until_heard(&mut self, from: usize, level: f32) -> usize {
            self.pump_until("a level to be heard", |m| {
                m.first_at_least(from, level).is_some()
            });
            self.first_at_least(from, level).unwrap()
        }

        fn has_failure(&self) -> bool {
            self.failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some()
        }
    }

    #[test]
    fn a_song_is_played_to_its_end_and_the_player_is_told_its_length() {
        let song = TempWav::new("end", RATE, &constant(QUARTER, 4_000));
        let mut m = Manual::new();
        m.load(&song.0);
        m.controller.play();
        m.pump_until("the song to finish", |m| m.controller.is_finished());

        let stats = m.controller.stats();
        assert_eq!(stats.played_frames, 4_000);
        assert_eq!(m.controller.total_frames(), Some(4_000));
        assert_eq!(stats.underruns, 0);
        assert!(m.first_at_least(0, 0.24).is_some(), "the song was heard");
        assert!(!m.has_failure());
        assert!(m.rig.worker.eof_sent);
    }

    #[test]
    fn the_length_is_told_to_the_player_as_soon_as_the_song_is_opened() {
        // The song is longer than the ring buffer, so the end of the file
        // has not been read yet: the length is the one the file says.
        let song = TempWav::new("length", RATE, &constant(QUARTER, 40_000));
        let mut m = Manual::new();
        assert_eq!(m.controller.total_frames(), None);
        m.load(&song.0);
        m.controller.play();
        m.pump_until_heard(0, 0.24);
        assert_eq!(m.controller.total_frames(), Some(40_000));
    }

    #[test]
    fn a_song_of_another_rate_is_played_at_the_rate_of_the_output() {
        // 1 s at 4,000 Hz is 1 s (8,000 frames) at the rate of the output.
        let song = TempWav::new("rate", RATE / 2, &constant(QUARTER, 4_000));
        let mut m = Manual::new();
        m.load(&song.0);
        m.controller.play();
        m.pump_until("the song to finish", |m| m.controller.is_finished());
        assert_eq!(m.controller.total_frames(), Some(8_000));
        assert_eq!(m.controller.stats().played_frames, 8_000);
    }

    #[test]
    fn a_looping_song_goes_round_without_a_gap() {
        let song = TempWav::new("loop", RATE, &constant(QUARTER, 4_000));
        let mut m = Manual::new();
        m.controller.set_looping(true);
        m.load(&song.0);
        m.controller.play();
        m.pump_until("three loops", |m| m.controller.stats().loops >= 3);
        m.pump(100);

        let stats = m.controller.stats();
        assert!(stats.played_frames > 8_000, "{}", stats.played_frames);
        assert_eq!(stats.underruns, 0);
        assert!(!m.controller.is_finished());
    }

    #[test]
    fn a_stop_starts_the_song_again_without_what_was_left_of_the_old_pass() {
        // 0.25 for the first 2,000 frames, then 0.5. The ring buffer is full
        // and the worker holds a part of a chunk when the stop comes.
        let song = TempWav::new("stop", RATE, &step_wave(QUARTER, 2_000, HALF, 16_000));
        let mut m = Manual::new();
        m.load(&song.0);
        m.controller.play();
        let heard = m.pump_until_heard(0, 0.49);
        m.pump(30);
        assert!(
            m.rig.worker.have_chunk && m.rig.worker.at > 0,
            "the worker should hold a written part of a chunk"
        );
        assert!(m.rig.worker.at < m.rig.worker.left.len());
        assert!(heard > 0);

        m.controller.request_flush();
        m.pump_until("the flush to be done", |m| m.controller.is_flushed());
        m.pump(10);
        m.controller.play();
        let mark = m.levels().len();
        let first = m.pump_until_heard(mark, 0.24);
        assert!(
            m.levels()[first] < 0.3,
            "the song did not start from its beginning: a block of {}",
            m.levels()[first]
        );
    }

    #[test]
    fn another_song_replaces_the_old_one_completely() {
        let a = TempWav::new("a", RATE, &constant(QUARTER, 40_000));
        let b = TempWav::new("b", RATE, &constant(HALF, 40_000));
        let mut m = Manual::new();
        m.load(&a.0);
        m.controller.play();
        m.pump_until_heard(0, 0.24);
        m.pump(50);

        m.load(&b.0);
        m.controller.play();
        let from = m.levels().len();
        let first_b = m.pump_until_heard(from, 0.49);
        m.pump(200);
        assert!(
            m.levels()[first_b..].iter().all(|&p| p >= 0.45),
            "a block of the old song after the new one began"
        );
        assert!(!m.controller.is_finished());
    }

    #[test]
    fn the_path_is_sent_before_the_flush_is_asked_for() {
        // The worker and the audio thread run to the end of the flush in the
        // gap between the two calls of `load_song`. If the flush came first,
        // the flush would end with the old song and the path would arrive
        // too late.
        let a = TempWav::new("order-a", RATE, &constant(QUARTER, 40_000));
        let b = TempWav::new("order-b", RATE, &constant(HALF, 40_000));
        let mut m = Manual::new();
        m.load(&a.0);
        m.controller.play();
        m.pump_until_heard(0, 0.24);

        let Manual {
            commands,
            controller,
            rig,
            ..
        } = &mut m;
        load_song(commands, controller, &b.0, &mut || rig.pump(200));
        controller.play();
        let from = rig.levels.len();
        m.pump_until_heard(from, 0.49);
    }

    #[test]
    fn a_play_right_after_a_load_is_enough() {
        let song = TempWav::new("direct", RATE, &constant(QUARTER, 4_000));
        let mut m = Manual::new();
        m.load(&song.0);
        m.controller.play();
        m.pump_until_heard(0, 0.24);
    }

    // -- failures -----------------------------------------------------------

    #[test]
    fn a_song_that_cannot_be_opened_is_over_for_the_player_and_the_user_is_told() {
        let mut m = Manual::new();
        let missing = missing_path("missing");
        m.load(&missing);
        m.pump_until("the failure", |m| m.has_failure());
        // The end was marked before the failure could be taken, so a play
        // after it finishes.
        m.controller.play();
        m.pump_until("the player to finish", |m| m.controller.is_finished());
        assert_eq!(m.controller.total_frames(), Some(0));

        let failure = m.failure.lock().unwrap().take().unwrap();
        assert_eq!(failure.path.as_deref(), Some(missing.as_path()));
        assert!(failure.to_string().contains("-missing.wav"), "{failure}");
    }

    #[test]
    fn a_play_before_the_failure_finishes_too() {
        let mut m = Manual::new();
        m.load(&missing_path("missing-early"));
        m.controller.play();
        m.pump_until("the player to finish", |m| m.controller.is_finished());
        assert!(m.has_failure());
    }

    #[test]
    fn a_song_that_failed_is_tried_again_at_the_next_stop() {
        let mut m = Manual::new();
        let path = missing_path("later");
        m.load(&path);
        m.pump_until("the failure", |m| m.has_failure());
        let _ = take_failure(&m.failure);

        // The file appears.
        let song = TempWav(path.clone());
        std::fs::write(
            &path,
            std::fs::read(
                TempWav::new("source", RATE, &constant(QUARTER, 4_000))
                    .0
                    .clone(),
            )
            .unwrap(),
        )
        .unwrap();
        m.controller.request_flush();
        m.controller.play();
        m.pump_until_heard(0, 0.24);
        assert!(!m.has_failure());
        drop(song);
    }

    #[test]
    fn a_loop_of_a_song_with_no_frame_fails_instead_of_turning_for_ever() {
        // The header says 1,000 frames and there are none.
        let empty = TempWav::with_claim("hollow", RATE, &[], 1_000);
        let good = TempWav::new("good", RATE, &constant(QUARTER, 4_000));
        let mut m = Manual::new();
        m.controller.set_looping(true);
        m.load(&empty.0);
        m.controller.play();
        m.pump_until("the failure", |m| m.has_failure());
        m.pump_until("the player to finish", |m| m.controller.is_finished());
        assert!(
            take_failure(&m.failure)
                .unwrap()
                .message
                .contains("no sound")
        );

        // The worker is alive: the next song plays.
        m.controller.set_looping(false);
        m.load(&good.0);
        m.controller.play();
        m.pump_until_heard(0, 0.24);
    }

    #[test]
    fn the_first_failure_stays_until_it_is_taken() {
        let mut m = Manual::new();
        let first = missing_path("first");
        let second = missing_path("second");
        m.load(&first);
        m.controller.play();
        m.pump_until("the player to finish", |m| m.controller.is_finished());
        m.load(&second);
        m.controller.play();
        m.pump_until("the second song to be flushed in", |m| {
            m.rig.worker.path.as_deref() == Some(second.as_path()) && m.rig.worker.failed
        });

        let failure = take_failure(&m.failure).unwrap();
        assert_eq!(failure.path.as_deref(), Some(first.as_path()));
        assert_eq!(take_failure(&m.failure), None, "the second was not stored");
    }

    // -- flushes in a row and the end mark ----------------------------------

    #[test]
    fn the_end_mark_of_an_old_song_does_not_end_the_new_one() {
        // The worker has written all of A and marked its end, but A is still
        // in the ring buffer when three flushes come in a row.
        let a = TempWav::new("eof-a", RATE, &constant(QUARTER, 3_000));
        let b = TempWav::new("eof-b", RATE, &constant(HALF, 80_000));
        let mut m = Manual::new();
        m.load(&a.0);
        m.controller.play();
        m.pump(2);
        m.rig.steps(10);
        assert!(
            m.rig.worker.eof_sent,
            "all of A should be in the ring buffer"
        );
        assert!(!m.controller.is_finished());

        m.controller.request_flush();
        m.controller.request_flush();
        m.load(&b.0);
        m.controller.play();
        let from = m.levels().len();
        let first_b = m.pump_until_heard(from, 0.49);
        m.pump(300);
        assert!(!m.controller.is_finished(), "the old end mark ended B");
        assert!(m.levels()[first_b..].iter().all(|&p| p >= 0.45));
        assert!(!m.rig.worker.eof_sent);
    }

    // -- checks and texts ---------------------------------------------------

    #[test]
    fn a_song_for_another_rate_is_refused() {
        assert!(check_rate(48_000, 48_000).is_ok());
        let error = check_rate(44_100, 48_000).unwrap_err();
        assert!(matches!(error, MediaError::Resample(_)));
        let text = error.to_string();
        assert!(text.contains("44100") && text.contains("48000"), "{text}");
    }

    #[test]
    fn a_failure_is_shown_with_its_file() {
        let with = BgmFailure {
            path: Some(PathBuf::from("music/a.mp3")),
            kind: FailureKind::NotASound,
            message: "the audio file has no sound".to_string(),
        };
        assert_eq!(with.to_string(), "music/a.mp3: the audio file has no sound");
        let without = BgmFailure {
            path: None,
            kind: FailureKind::Other,
            message: "oops".to_string(),
        };
        assert_eq!(without.to_string(), "oops");
    }

    #[test]
    fn a_failure_kind_comes_from_the_error() {
        let io = |kind| MediaError::Io(io::Error::from(kind));
        let cases = [
            (io(io::ErrorKind::NotFound), FailureKind::NotFound),
            (io(io::ErrorKind::PermissionDenied), FailureKind::CannotRead),
            (io(io::ErrorKind::UnexpectedEof), FailureKind::CannotRead),
            (MediaError::UnsupportedFormat, FailureKind::NotASound),
            (MediaError::NoAudioTrack, FailureKind::NotASound),
            (MediaError::Empty, FailureKind::NotASound),
            (MediaError::Decode("x".to_string()), FailureKind::Broken),
            (MediaError::InvalidSampleRate(0), FailureKind::Other),
            (MediaError::Resample("x".to_string()), FailureKind::Other),
            (
                MediaError::TooLong {
                    limit: Duration::from_secs(1),
                },
                FailureKind::Other,
            ),
        ];
        for (error, kind) in cases {
            assert_eq!(failure_kind(&error), kind, "{error}");
        }
    }

    #[test]
    fn a_failure_says_in_japanese_which_song_and_why() {
        let failure = |kind| BgmFailure {
            path: Some(PathBuf::from("music").join("a.mp3")),
            kind,
            message: "english detail".to_string(),
        };
        let cases = [
            (
                FailureKind::NotFound,
                "「曲 A」を再生できません：ファイルが見つかりません",
            ),
            (
                FailureKind::CannotRead,
                "「曲 A」を再生できません：ファイルを読めません",
            ),
            (
                FailureKind::NotASound,
                "「曲 A」を再生できません：音声として読めません（WAV / MP3 か確認してください）",
            ),
            (
                FailureKind::Broken,
                "「曲 A」を再生できません：音声が壊れていて、続けられません",
            ),
            (
                FailureKind::Other,
                "「曲 A」を再生できません：原因は詳細を見てください",
            ),
        ];
        for (kind, expected) in cases {
            let text = failure(kind).message_ja("曲 A");
            assert_eq!(text, expected);
            // The song is named by its display name, never by its path.
            assert!(!text.contains("music") && !text.contains("a.mp3"), "{text}");
        }
    }

    // -- the real thread ----------------------------------------------------

    /// The audio thread, played by a thread that renders a block every 2 ms.
    struct FakeAudio {
        stop: Arc<AtomicBool>,
        levels: Arc<Mutex<Vec<f32>>>,
        thread: Option<JoinHandle<()>>,
    }

    impl FakeAudio {
        fn start(mut player: BgmPlayer) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let levels = Arc::new(Mutex::new(Vec::new()));
            let thread = {
                let stop = Arc::clone(&stop);
                let levels = Arc::clone(&levels);
                thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let level = peak(&mut player);
                        levels.lock().unwrap().push(level);
                        thread::sleep(Duration::from_millis(2));
                    }
                })
            };
            Self {
                stop,
                levels,
                thread: Some(thread),
            }
        }

        fn heard(&self, level: f32) -> bool {
            self.levels.lock().unwrap().iter().any(|&p| p >= level)
        }
    }

    impl Drop for FakeAudio {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(start.elapsed() < TIMEOUT, "gave up waiting for: {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    // The audio side is dropped before the `Bgm`, as in a session.
    struct Threaded {
        audio: FakeAudio,
        bgm: Bgm,
    }

    fn threaded() -> Threaded {
        let (controller, feeder, player) = bgm_channel(RATE, BLOCK);
        let bgm = Bgm::start(controller, feeder).unwrap();
        Threaded {
            audio: FakeAudio::start(player),
            bgm,
        }
    }

    #[test]
    fn the_thread_plays_a_song_to_its_end_and_a_loop_goes_round() {
        let t = threaded();
        let song = TempWav::new("thread", RATE, &constant(QUARTER, 2_000));
        t.bgm.load(&song.0);
        t.bgm.controller().play();
        wait_until("the song to finish", || t.bgm.controller().is_finished());
        assert_eq!(t.bgm.controller().stats().played_frames, 2_000);
        assert!(t.audio.heard(0.24));

        t.bgm.controller().set_looping(true);
        t.bgm.stop();
        wait_until("the flush", || t.bgm.controller().is_flushed());
        t.bgm.controller().play();
        wait_until("two loops", || t.bgm.controller().stats().loops >= 2);
        assert!(t.bgm.take_error().is_none());
    }

    #[test]
    fn the_thread_reports_a_song_that_cannot_be_opened_and_goes_on() {
        let t = threaded();
        t.bgm.load(&missing_path("thread-missing"));
        t.bgm.controller().play();
        wait_until("the player to finish", || t.bgm.controller().is_finished());
        let failure = t.bgm.take_error().unwrap();
        assert!(failure.to_string().contains("thread-missing"), "{failure}");
        assert_eq!(failure.kind, FailureKind::NotFound);

        let song = TempWav::new("thread-good", RATE, &constant(QUARTER, 2_000));
        t.bgm.load(&song.0);
        t.bgm.controller().play();
        wait_until("the song to be heard", || t.audio.heard(0.24));
    }

    /// How long dropping a `Bgm` takes, with the player dropped first (as
    /// the output stream does in a session).
    fn drop_time(bgm: Bgm, player: BgmPlayer) -> Duration {
        drop(player);
        let start = Instant::now();
        drop(bgm);
        start.elapsed()
    }

    #[test]
    fn the_worker_ends_within_a_second_whatever_it_is_waiting_for() {
        let limit = Duration::from_secs(1);

        // Waiting for a flush that the audio thread never finishes.
        let (controller, feeder, player) = bgm_channel(RATE, BLOCK);
        let bgm = Bgm::start(controller, feeder).unwrap();
        let song = TempWav::new("drop-a", RATE, &constant(QUARTER, 16_000));
        bgm.load(&song.0);
        thread::sleep(Duration::from_millis(50));
        let waiting = drop_time(bgm, player);

        // Waiting for room in a ring buffer that nothing reads.
        let (controller, feeder, player) = bgm_channel(RATE, BLOCK);
        let bgm = Bgm::start(controller, feeder).unwrap();
        bgm.stop();
        bgm.load(&song.0);
        // The flush is not answered by the audio thread, so drive it here.
        let mut player = player;
        let start = Instant::now();
        while !bgm.controller().is_flushed() {
            peak(&mut player);
            assert!(start.elapsed() < TIMEOUT, "the flush did not end");
            thread::sleep(Duration::from_millis(1));
        }
        bgm.controller().play();
        thread::sleep(Duration::from_millis(100));
        let full = drop_time(bgm, player);

        // Nothing to do at all.
        let (controller, feeder, player) = bgm_channel(RATE, BLOCK);
        let bgm = Bgm::start(controller, feeder).unwrap();
        thread::sleep(Duration::from_millis(20));
        let idle = drop_time(bgm, player);

        eprintln!("drop times: waiting {waiting:?}, full {full:?}, idle {idle:?}");
        assert!(waiting < limit, "{waiting:?}");
        assert!(full < limit, "{full:?}");
        assert!(idle < limit, "{idle:?}");
    }
}
