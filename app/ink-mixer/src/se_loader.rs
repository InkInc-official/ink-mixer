//! The loader of the sound effects (SE): a thread that reads the files for the
//! Sound Pad (#64, ADR-0009).
//!
//! Reading an SE means decoding the file and changing its sample rate to the
//! rate of the output ([`load_sound`]). It takes a few milliseconds, up to
//! about 0.15 s for the longest SE (60 s). It is done here, on the thread
//! `ink-se-loader`, so that the window does not stop while the pads of a new
//! session are read.
//!
//! # One at a time
//!
//! The jobs are done **one at a time, in the order they were given**
//! (ADR-0009: the memory peak of reading is about 3.3 times the size of the
//! sound, so several at once could take much more than the total limit).
//!
//! # Who puts the sound in its place
//!
//! The loader only reads. The window takes the results with
//! [`try_recv`](SeLoader::try_recv) and puts the sound into the SE player
//! (`SeController::set_sound`) on the UI thread, because the controller
//! belongs to the UI. The `Arc` of a sound that is not wanted any more (the
//! pad was changed, or the session was made again) is dropped there too,
//! never on the audio thread.
//!
//! # Waking the window
//!
//! The window repaints only when something happens. After each result the
//! loader calls `wake`, which asks the window for a repaint, so that the
//! result is seen without a timer that polls.
//!
//! # The end
//!
//! Dropping the [`SeLoader`] ends the thread: the job that is being done is
//! finished (at most about 0.2 s), and the jobs that are waiting are not
//! done.

// The window uses all of this in the later steps of #64; remove this there.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "used by the window in later steps of #64")
)]

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ink_core::SE_MAX_DURATION;
use ink_core::graph::SoundData;
use ink_media::{Decoded, MediaError, load_sound};
use tracing::{debug, warn};

/// One sound to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Told back in the [`Done`]. The window gives a new number to every job
    /// and keeps the newest one of each pad, so that the result of an older
    /// job is thrown away.
    pub ticket: u64,
    /// The pad (the slot of the SE player) the sound is for.
    pub slot: usize,
    /// The file.
    pub path: PathBuf,
    /// The sample rate of the output.
    pub rate: u32,
}

/// The result of a [`Job`].
#[derive(Debug)]
pub struct Done {
    pub ticket: u64,
    pub slot: usize,
    pub result: Result<Loaded, LoadError>,
}

/// A sound that was read.
#[derive(Debug)]
pub struct Loaded {
    /// At the sample rate of the job.
    pub sound: Arc<SoundData>,
    /// How many packets could not be decoded and were skipped.
    pub skipped_packets: u64,
    /// How long reading took.
    pub took: Duration,
}

/// What kind of thing went wrong while a sound was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadErrorKind {
    /// Longer than [`SE_MAX_DURATION`].
    TooLong,
    /// The file is not there.
    NotFound,
    /// The file is there but cannot be read.
    CannotRead,
    /// The file is not a sound that can be played, or it broke while it was
    /// decoded.
    NotASound,
    /// Anything else.
    Other,
}

/// Why a sound could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub kind: LoadErrorKind,
    /// The text of the error, in English, for the log and for a bug report.
    pub detail: String,
}

impl LoadError {
    /// Classifies the error of the media code.
    fn from_media(error: &MediaError) -> Self {
        let kind = match error {
            MediaError::TooLong { .. } => LoadErrorKind::TooLong,
            MediaError::Io(e) if e.kind() == io::ErrorKind::NotFound => LoadErrorKind::NotFound,
            MediaError::Io(_) => LoadErrorKind::CannotRead,
            MediaError::UnsupportedFormat
            | MediaError::NoAudioTrack
            | MediaError::Empty
            | MediaError::Decode(_) => LoadErrorKind::NotASound,
            // `MediaError` may get more kinds; they are "other" until named here.
            _ => LoadErrorKind::Other,
        };
        Self {
            kind,
            detail: error.to_string(),
        }
    }

    /// One line in Japanese for the pad: the sound `name` (the display name,
    /// never a path) and why it cannot be used. The English
    /// [`detail`](Self::detail) is for the hover.
    pub fn message_ja(&self, name: &str) -> String {
        match self.kind {
            LoadErrorKind::TooLong => format!(
                "「{name}」は {} 秒より長いため、SE にできません。BGM として登録してください",
                SE_MAX_DURATION.as_secs()
            ),
            LoadErrorKind::NotFound => format!("「{name}」のファイルが見つかりません"),
            LoadErrorKind::CannotRead => format!("「{name}」を読めませんでした"),
            LoadErrorKind::NotASound => {
                format!("「{name}」は音声として読めませんでした（WAV / MP3 か確認してください）")
            }
            LoadErrorKind::Other => {
                format!("「{name}」を読み込めませんでした（詳細を確認してください）")
            }
        }
    }
}

/// The function that reads a sound: the file and the sample rate of the
/// output. The loader uses [`load_sound`]; the tests use their own.
type LoadFn = Box<dyn Fn(&Path, u32) -> Result<Decoded, MediaError> + Send>;

/// The loader thread, as the window holds it. Dropping it ends the thread.
pub struct SeLoader {
    /// `None` after it is closed.
    jobs: Option<Sender<Job>>,
    results: Receiver<Done>,
    closing: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SeLoader {
    /// Starts the thread (`ink-se-loader`). `wake` is called after every
    /// result, from the loader thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the thread cannot be started.
    pub fn start(wake: Box<dyn Fn() + Send>) -> io::Result<Self> {
        Self::start_with(
            wake,
            Box::new(|path, rate| load_sound(path, rate, Some(SE_MAX_DURATION))),
        )
    }

    fn start_with(wake: Box<dyn Fn() + Send>, load: LoadFn) -> io::Result<Self> {
        let (jobs, job_receiver) = mpsc::channel::<Job>();
        let (result_sender, results) = mpsc::channel::<Done>();
        let closing = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("ink-se-loader".to_string())
            .spawn({
                let closing = Arc::clone(&closing);
                move || run(&job_receiver, &result_sender, &closing, &*wake, &*load)
            })?;
        Ok(Self {
            jobs: Some(jobs),
            results,
            closing,
            thread: Some(thread),
        })
    }

    /// Queues `job`. It is done after the jobs that are already waiting.
    /// Returns `false` if the thread is gone (the job will never be done).
    pub fn submit(&self, job: Job) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(job).is_ok())
    }

    /// Takes the next result, if there is one. Never waits.
    pub fn try_recv(&self) -> Option<Done> {
        self.results.try_recv().ok()
    }
}

impl Drop for SeLoader {
    fn drop(&mut self) {
        // The job that is running is finished; the ones that wait are not done.
        self.closing.store(true, Ordering::Release);
        self.jobs = None;
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("the SE loader thread panicked");
        }
    }
}

/// The loop of the thread: one job at a time, until the loader is dropped.
fn run(
    jobs: &Receiver<Job>,
    results: &Sender<Done>,
    closing: &AtomicBool,
    wake: &dyn Fn(),
    load: &dyn Fn(&Path, u32) -> Result<Decoded, MediaError>,
) {
    // `recv` ends when the loader drops its sender.
    while let Ok(job) = jobs.recv() {
        if closing.load(Ordering::Acquire) {
            break;
        }
        let done = process(job, load);
        // A closed channel means the loader is gone; there is nobody to tell.
        let _ = results.send(done);
        wake();
    }
}

/// Reads the sound of `job`. A panic of the media code (a broken file that
/// the decoder cannot take) is an error of this job, not the end of the loader.
fn process(job: Job, load: &dyn Fn(&Path, u32) -> Result<Decoded, MediaError>) -> Done {
    let Job {
        ticket,
        slot,
        path,
        rate,
    } = job;
    let started = Instant::now();
    let result = match panic::catch_unwind(AssertUnwindSafe(|| load(&path, rate))) {
        Ok(Ok(decoded)) => {
            if decoded.skipped_packets >= 1 {
                warn!(
                    "{}: {} packet(s) could not be decoded and were skipped",
                    path.display(),
                    decoded.skipped_packets
                );
            }
            Ok(Loaded {
                sound: Arc::new(decoded.sound),
                skipped_packets: decoded.skipped_packets,
                took: started.elapsed(),
            })
        }
        Ok(Err(error)) => {
            debug!("SE not loaded: {}: {error}", path.display());
            Err(LoadError::from_media(&error))
        }
        Err(_) => {
            warn!("the media code panicked while it read {}", path.display());
            Err(LoadError {
                kind: LoadErrorKind::Other,
                detail: "the loader panicked while it read the file".to_string(),
            })
        }
    };
    Done {
        ticket,
        slot,
        result,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Long enough for anything that works; a test that waits this long has
    /// failed.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// A fresh directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ink-mixer-se-loader-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A mono 16-bit WAV file whose header says it has `claimed` frames,
    /// with `samples` in it.
    fn write_wav(path: &Path, rate: u32, samples: &[i16], claimed: usize) {
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
        fs::write(path, bytes).unwrap();
    }

    /// 0.25 of full scale, as it comes back from a 16-bit file.
    const QUARTER: i16 = 8_192;

    fn job(ticket: u64, slot: usize, path: &Path, rate: u32) -> Job {
        Job {
            ticket,
            slot,
            path: path.to_path_buf(),
            rate,
        }
    }

    /// A wake that counts.
    fn counting_wake() -> (Arc<AtomicUsize>, Box<dyn Fn() + Send>) {
        let count = Arc::new(AtomicUsize::new(0));
        let wake = {
            let count = Arc::clone(&count);
            Box::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            })
        };
        (count, wake)
    }

    fn no_wake() -> Box<dyn Fn() + Send> {
        Box::new(|| {})
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(start.elapsed() < TIMEOUT, "waited too long for {what}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Waits for the next result.
    fn next_done(loader: &SeLoader) -> Done {
        let start = Instant::now();
        loop {
            if let Some(done) = loader.try_recv() {
                return done;
            }
            assert!(start.elapsed() < TIMEOUT, "waited too long for a result");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A sound of `frames` frames, as a loader that does not read files gives.
    fn decoded(rate: u32, frames: usize) -> Decoded {
        Decoded {
            sound: SoundData::new(rate, vec![0.1; frames], vec![0.1; frames]),
            skipped_packets: 0,
        }
    }

    // -- real files ----------------------------------------------------------------

    #[test]
    fn a_real_wav_is_read_at_the_rate_of_the_output() {
        let dir = test_dir("real");
        let path = dir.join("効果音 テスト.wav");
        write_wav(&path, 8_000, &vec![QUARTER; 4_000], 4_000);
        let loader = SeLoader::start(no_wake()).unwrap();

        assert!(loader.submit(job(7, 3, &path, 8_000)));
        let done = next_done(&loader);
        assert_eq!((done.ticket, done.slot), (7, 3));
        let loaded = done.result.unwrap();
        assert_eq!(loaded.sound.sample_rate(), 8_000);
        assert_eq!(loaded.sound.frames(), 4_000);
        assert!((loaded.sound.left()[100] - 0.25).abs() < 1e-3);
        assert_eq!(loaded.skipped_packets, 0);
        assert!(loaded.took < TIMEOUT, "{:?}", loaded.took);
        assert!(loader.try_recv().is_none());
    }

    #[test]
    fn a_wav_is_changed_to_the_rate_of_the_output() {
        let dir = test_dir("resample");
        let path = dir.join("rate change.wav");
        write_wav(&path, 8_000, &vec![QUARTER; 4_000], 4_000);
        let loader = SeLoader::start(no_wake()).unwrap();

        loader.submit(job(1, 0, &path, 16_000));
        let loaded = next_done(&loader).result.unwrap();
        assert_eq!(loaded.sound.sample_rate(), 16_000);
        let frames = loaded.sound.frames();
        assert!((7_900..=8_100).contains(&frames), "{frames}");
    }

    #[test]
    fn a_sound_longer_than_the_limit_is_refused_from_its_header() {
        let dir = test_dir("long");
        let path = dir.join("long header.wav");
        // The header says 61 s; there is no sound in the file, so the answer
        // can only come from the header.
        write_wav(&path, 8_000, &[], 61 * 8_000);
        let loader = SeLoader::start(no_wake()).unwrap();

        loader.submit(job(1, 0, &path, 8_000));
        let error = next_done(&loader).result.unwrap_err();
        assert_eq!(error.kind, LoadErrorKind::TooLong, "{error:?}");
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn a_sound_of_exactly_the_limit_is_read() {
        let dir = test_dir("exact");
        let path = dir.join("sixty.wav");
        let frames = 60 * 8_000;
        write_wav(&path, 8_000, &vec![QUARTER; frames], frames);
        let loader = SeLoader::start(no_wake()).unwrap();

        loader.submit(job(1, 0, &path, 8_000));
        let loaded = next_done(&loader).result.unwrap();
        assert_eq!(loaded.sound.frames(), frames);
    }

    #[test]
    fn a_missing_file_and_a_file_that_is_not_a_sound_are_told_apart() {
        let dir = test_dir("errors");
        let missing = dir.join("not there.wav");
        let text = dir.join("text.wav");
        fs::write(&text, b"this is not a sound at all, only some text").unwrap();
        let loader = SeLoader::start(no_wake()).unwrap();

        loader.submit(job(1, 0, &missing, 48_000));
        loader.submit(job(2, 1, &text, 48_000));
        assert_eq!(
            next_done(&loader).result.unwrap_err().kind,
            LoadErrorKind::NotFound
        );
        assert_eq!(
            next_done(&loader).result.unwrap_err().kind,
            LoadErrorKind::NotASound
        );
    }

    // -- the errors -------------------------------------------------------------------

    #[test]
    fn an_error_of_the_media_code_gets_a_kind() {
        let io = |kind| MediaError::Io(io::Error::from(kind));
        let cases = [
            (
                MediaError::TooLong {
                    limit: Duration::from_secs(60),
                },
                LoadErrorKind::TooLong,
            ),
            (io(io::ErrorKind::NotFound), LoadErrorKind::NotFound),
            (
                io(io::ErrorKind::PermissionDenied),
                LoadErrorKind::CannotRead,
            ),
            (MediaError::UnsupportedFormat, LoadErrorKind::NotASound),
            (MediaError::NoAudioTrack, LoadErrorKind::NotASound),
            (MediaError::Empty, LoadErrorKind::NotASound),
            (
                MediaError::Decode("x".to_string()),
                LoadErrorKind::NotASound,
            ),
            (MediaError::InvalidSampleRate(0), LoadErrorKind::Other),
            (MediaError::Resample("x".to_string()), LoadErrorKind::Other),
        ];
        for (error, kind) in cases {
            let classified = LoadError::from_media(&error);
            assert_eq!(classified.kind, kind, "{error}");
            assert_eq!(classified.detail, error.to_string());
        }
    }

    #[test]
    fn the_japanese_line_names_the_sound_and_never_a_path() {
        let name = "拍手 テスト";
        let path_text = "folder";
        for kind in [
            LoadErrorKind::TooLong,
            LoadErrorKind::NotFound,
            LoadErrorKind::CannotRead,
            LoadErrorKind::NotASound,
            LoadErrorKind::Other,
        ] {
            let error = LoadError {
                kind,
                detail: format!("{path_text}/secret.wav: english detail"),
            };
            let text = error.message_ja(name);
            assert!(text.contains(&format!("「{name}」")), "{text}");
            assert!(
                !text.contains(path_text) && !text.contains("secret") && !text.contains("english"),
                "{text}"
            );
        }
        let too_long = LoadError {
            kind: LoadErrorKind::TooLong,
            detail: String::new(),
        }
        .message_ja(name);
        assert!(too_long.contains("60 秒"), "{too_long}");
        assert!(too_long.contains("BGM"), "{too_long}");
    }

    // -- the thread --------------------------------------------------------------------

    #[test]
    fn jobs_are_done_one_at_a_time_in_the_order_they_were_given() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(Mutex::new(Vec::new()));
        let load: LoadFn = {
            let (in_flight, most, order) = (
                Arc::clone(&in_flight),
                Arc::clone(&most),
                Arc::clone(&order),
            );
            Box::new(move |path, rate| {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                order.lock().unwrap().push(path.to_path_buf());
                thread::sleep(Duration::from_millis(15));
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(decoded(rate, 10))
            })
        };
        let loader = SeLoader::start_with(no_wake(), load).unwrap();

        let dir = test_dir("order");
        let paths: Vec<PathBuf> = (0..6).map(|n| dir.join(format!("{n}.wav"))).collect();
        for (n, path) in paths.iter().enumerate() {
            assert!(loader.submit(job(n as u64 + 1, n, path, 48_000)));
        }
        let tickets: Vec<u64> = (0..6).map(|_| next_done(&loader).ticket).collect();

        assert_eq!(tickets, [1, 2, 3, 4, 5, 6]);
        assert_eq!(*order.lock().unwrap(), paths);
        assert_eq!(most.load(Ordering::SeqCst), 1, "never two at once");
    }

    #[test]
    fn the_window_is_woken_after_every_result() {
        let (count, wake) = counting_wake();
        let loader = SeLoader::start_with(wake, Box::new(|_, rate| Ok(decoded(rate, 10)))).unwrap();

        let dir = test_dir("wake");
        for n in 0..3_u64 {
            loader.submit(job(n, 0, &dir.join("a.wav"), 48_000));
        }
        wait_until("three wakes", || count.load(Ordering::SeqCst) == 3);

        // Every result is there to be taken when the window wakes.
        let mut tickets = Vec::new();
        while let Some(done) = loader.try_recv() {
            tickets.push(done.ticket);
        }
        assert_eq!(tickets, [0, 1, 2]);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_failure_wakes_the_window_too() {
        let (count, wake) = counting_wake();
        let loader =
            SeLoader::start_with(wake, Box::new(|_, _| Err(MediaError::UnsupportedFormat)))
                .unwrap();
        loader.submit(job(1, 0, &test_dir("wake-fail").join("x.wav"), 48_000));
        wait_until("a wake", || count.load(Ordering::SeqCst) == 1);
        assert!(loader.try_recv().unwrap().result.is_err());
    }

    #[test]
    fn dropping_returns_within_a_second_while_a_sound_is_being_read_and_skips_the_rest() {
        let started = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let load: LoadFn = {
            let (started, calls) = (Arc::clone(&started), Arc::clone(&calls));
            Box::new(move |_, rate| {
                calls.fetch_add(1, Ordering::SeqCst);
                started.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(300));
                Ok(decoded(rate, 10))
            })
        };
        let loader = SeLoader::start_with(no_wake(), load).unwrap();
        let dir = test_dir("drop");
        for n in 0..4_u64 {
            loader.submit(job(n, 0, &dir.join("a.wav"), 48_000));
        }
        wait_until("the first read to start", || started.load(Ordering::SeqCst));

        let start = Instant::now();
        drop(loader);
        let took = start.elapsed();
        assert!(took < Duration::from_secs(1), "{took:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the jobs that waited were not done"
        );
    }

    #[test]
    fn an_idle_loader_is_dropped_at_once() {
        let loader = SeLoader::start(no_wake()).unwrap();
        let start = Instant::now();
        drop(loader);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_panic_of_the_media_code_is_the_error_of_that_job_and_the_loader_goes_on() {
        let load: LoadFn = Box::new(|path, rate| {
            if path.ends_with("bad.wav") {
                panic!("the decoder cannot take this file");
            }
            Ok(decoded(rate, 10))
        });
        let loader = SeLoader::start_with(no_wake(), load).unwrap();
        let dir = test_dir("panic");

        loader.submit(job(1, 0, &dir.join("bad.wav"), 48_000));
        loader.submit(job(2, 1, &dir.join("good.wav"), 48_000));
        let first = next_done(&loader);
        assert_eq!(first.ticket, 1);
        assert_eq!(first.result.unwrap_err().kind, LoadErrorKind::Other);
        let second = next_done(&loader);
        assert_eq!(second.ticket, 2);
        assert_eq!(second.result.unwrap().sound.frames(), 10);
    }

    #[test]
    fn a_result_keeps_the_ticket_and_the_slot_of_its_job() {
        let loader =
            SeLoader::start_with(no_wake(), Box::new(|_, rate| Ok(decoded(rate, 10)))).unwrap();
        let dir = test_dir("ticket");
        loader.submit(job(41, 5, &dir.join("a.wav"), 44_100));
        let done = next_done(&loader);
        assert_eq!((done.ticket, done.slot), (41, 5));
        assert_eq!(done.result.unwrap().sound.sample_rate(), 44_100);
    }
}
