//! Backend-independent stream types.
//!
//! No `cpal` type appears here: a backend keeps its own stream object inside
//! [`AudioStream`] as an opaque box.

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

/// The format of a stream: sample rate and number of channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
}

/// The largest number of frames passed to an [`InputCallback`] or an
/// [`OutputCallback`] in one call.
///
/// A larger block from the device is split into calls of at most this many
/// frames, so the callback can work with buffers allocated in advance.
pub const MAX_FRAMES_PER_CALLBACK: usize = 4096;

/// Receives captured audio as interleaved f32 samples with
/// [`StreamConfig::channels`] channels, at most [`MAX_FRAMES_PER_CALLBACK`]
/// frames per call.
///
/// Runs on the audio thread: it must not allocate, block, or log
/// (AGENTS.md §5).
pub type InputCallback = Box<dyn FnMut(&[f32]) + Send + 'static>;

/// Fills a buffer of interleaved f32 samples with [`StreamConfig::channels`]
/// channels to be played, at most [`MAX_FRAMES_PER_CALLBACK`] frames per
/// call.
///
/// The buffer is silent (all 0.0) when passed in, so samples the callback
/// does not write stay silent. Before reaching the device, NaN and infinity
/// become 0 and other values are limited to -1.0..=1.0.
///
/// Runs on the audio thread: it must not allocate, block, or log
/// (AGENTS.md §5).
pub type OutputCallback = Box<dyn FnMut(&mut [f32]) + Send + 'static>;

/// A fatal error of a running stream. After it, the stream delivers no more
/// data and should be dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamError {
    /// The device went away (for example, it was unplugged).
    DeviceUnavailable,
    /// The OS denied access to the device (for example, a privacy setting).
    PermissionDenied,
    /// Any other fatal failure reported by the backend.
    Other,
}

impl StreamError {
    fn code(self) -> u8 {
        match self {
            Self::DeviceUnavailable => 1,
            Self::PermissionDenied => 2,
            Self::Other => 3,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::DeviceUnavailable),
            2 => Some(Self::PermissionDenied),
            3 => Some(Self::Other),
            _ => None,
        }
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceUnavailable => write!(f, "the audio device is no longer available"),
            Self::PermissionDenied => write!(f, "access to the audio device was denied"),
            Self::Other => write!(f, "the audio stream failed"),
        }
    }
}

impl std::error::Error for StreamError {}

/// What the backend reported when a stream failed, kept next to the coarse
/// [`StreamError`] so a log line can say more than "the audio stream failed".
///
/// It names the kind of the backend's own error (for example cpal's
/// `ErrorKind`) without exposing the backend's type. For diagnostics only:
/// do not branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorDetail {
    /// The device is busy, for example used by another application.
    DeviceBusy,
    /// The device is not available.
    DeviceNotAvailable,
    /// The audio host (server or subsystem) is not available.
    HostUnavailable,
    /// An argument was invalid.
    InvalidInput,
    /// Access to the device was denied.
    PermissionDenied,
    /// An OS resource limit was reached.
    ResourceExhausted,
    /// The stream configuration is no longer valid.
    StreamInvalidated,
    /// The stream configuration is not supported.
    UnsupportedConfig,
    /// The operation is not supported.
    UnsupportedOperation,
    /// The platform's audio API returned an error the backend cannot map to
    /// a more specific kind.
    BackendError,
    /// The backend reported an error that fits no other kind.
    Other,
    /// A kind this version does not know (a newer backend library may add
    /// kinds), or a kind that is never fatal.
    Unknown,
}

impl ErrorDetail {
    /// Every kind, in code order.
    pub const ALL: [Self; 12] = [
        Self::DeviceBusy,
        Self::DeviceNotAvailable,
        Self::HostUnavailable,
        Self::InvalidInput,
        Self::PermissionDenied,
        Self::ResourceExhausted,
        Self::StreamInvalidated,
        Self::UnsupportedConfig,
        Self::UnsupportedOperation,
        Self::BackendError,
        Self::Other,
        Self::Unknown,
    ];

    /// A short stable name for logs, such as `"backend-error"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeviceBusy => "device-busy",
            Self::DeviceNotAvailable => "device-not-available",
            Self::HostUnavailable => "host-unavailable",
            Self::InvalidInput => "invalid-input",
            Self::PermissionDenied => "permission-denied",
            Self::ResourceExhausted => "resource-exhausted",
            Self::StreamInvalidated => "stream-invalidated",
            Self::UnsupportedConfig => "unsupported-config",
            Self::UnsupportedOperation => "unsupported-operation",
            Self::BackendError => "backend-error",
            Self::Other => "other",
            Self::Unknown => "unknown",
        }
    }

    /// 1 for the first kind of [`ALL`](Self::ALL); 0 is kept for "none".
    fn code(self) -> u8 {
        match self {
            Self::DeviceBusy => 1,
            Self::DeviceNotAvailable => 2,
            Self::HostUnavailable => 3,
            Self::InvalidInput => 4,
            Self::PermissionDenied => 5,
            Self::ResourceExhausted => 6,
            Self::StreamInvalidated => 7,
            Self::UnsupportedConfig => 8,
            Self::UnsupportedOperation => 9,
            Self::BackendError => 10,
            Self::Other => 11,
            Self::Unknown => 12,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        let index = usize::from(code).checked_sub(1)?;
        Self::ALL.get(index).copied()
    }
}

impl fmt::Display for ErrorDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The most bytes of the backend's own message kept for a fatal error. Longer
/// messages are cut (at a character boundary) and end with `...`. The length
/// is stored in one byte, so it must stay below 256.
pub(crate) const NOTE_CAPACITY: usize = 128;

/// Marks a message that was cut.
const NOTE_ELLIPSIS: &str = "...";

const _: () = assert!(NOTE_CAPACITY > NOTE_ELLIPSIS.len() && NOTE_CAPACITY <= u8::MAX as usize);

/// A fatal error of a stream with everything recorded about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The coarse class of the error.
    pub error: StreamError,
    /// What the backend reported.
    pub detail: ErrorDetail,
    /// The backend's own message, when it gave one: cleaned up (control
    /// characters become spaces) and cut at a character boundary to a fixed
    /// number of bytes, with `...` at the end when it was cut.
    pub note: Option<String>,
}

/// The backend's message about a fatal error, in a fixed place that the
/// audio thread fills without allocating.
///
/// The audio thread writes the bytes first and publishes the length last
/// (`Release`); the control thread reads the length first (`Acquire`) and
/// then the bytes. Until the length is published the message cannot be read.
/// The two steps are separate functions so that this order can be tested.
struct Note {
    /// Taken by the first writer; nobody else writes the bytes after that.
    claimed: AtomicBool,
    /// 0 until the message is published, then its length in bytes.
    len: AtomicU8,
    bytes: [AtomicU8; NOTE_CAPACITY],
}

impl Default for Note {
    fn default() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            len: AtomicU8::new(0),
            bytes: std::array::from_fn(|_| AtomicU8::new(0)),
        }
    }
}

impl fmt::Debug for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Note")
            .field("len", &self.len.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Note {
    /// Returns whether the caller is the first and may write.
    fn claim(&self) -> bool {
        !self.claimed.swap(true, Ordering::Relaxed)
    }

    /// Step 1: writes the message into the fixed place and returns how many
    /// bytes it used. Nothing can be read yet. It does not allocate: it works
    /// on one character at a time.
    ///
    /// Spaces and control characters at both ends are dropped and control
    /// characters inside become spaces (a Windows message ends with `\r\n`;
    /// a line break would split the log line). The text stays UTF-8, because
    /// Windows gives messages in the language of the system. When it does not
    /// fit, it is cut at a character boundary and `...` is added.
    fn write_bytes(&self, message: &str) -> usize {
        let message = message.trim_matches(|c: char| c.is_whitespace() || c.is_control());
        let clean = |c: char| if c.is_control() { ' ' } else { c };
        let total: usize = message.chars().map(|c| clean(c).len_utf8()).sum();
        let cut = total > NOTE_CAPACITY;
        let limit = if cut {
            NOTE_CAPACITY - NOTE_ELLIPSIS.len()
        } else {
            total
        };

        let mut used = 0;
        let mut buffer = [0_u8; 4];
        for c in message.chars() {
            let encoded = clean(c).encode_utf8(&mut buffer).as_bytes();
            if used + encoded.len() > limit {
                break;
            }
            for byte in encoded {
                self.bytes[used].store(*byte, Ordering::Relaxed);
                used += 1;
            }
        }
        if cut {
            for byte in NOTE_ELLIPSIS.bytes() {
                self.bytes[used].store(byte, Ordering::Relaxed);
                used += 1;
            }
        }
        used
    }

    /// Step 2: makes the `len` bytes written by [`write_bytes`](Self::write_bytes)
    /// readable. Must come after step 1.
    fn publish(&self, len: usize) {
        self.len
            .store(u8::try_from(len).unwrap_or(0), Ordering::Release);
    }

    /// The message, or `None` when none is published.
    fn read(&self) -> Option<String> {
        let len = usize::from(self.len.load(Ordering::Acquire));
        if len == 0 {
            return None;
        }
        let bytes: Vec<u8> = self.bytes[..len]
            .iter()
            .map(|byte| byte.load(Ordering::Relaxed))
            .collect();
        String::from_utf8(bytes).ok()
    }
}

/// State written by the audio thread and read by the control thread.
///
/// Only atomics, so the audio thread can update it without allocating or
/// blocking. `Ordering::Relaxed` is enough for the counters: each stands on
/// its own and is not used to publish other data.
///
/// The fatal error and its [`ErrorDetail`] share one atomic, so the control
/// thread can never see the error without its detail. The backend's message
/// is too big for that: it is written first (see [`Note`]) and then the error
/// is set with `Release`; whoever sees the error with `Acquire` also sees the
/// message.
///
/// One stream reports its errors one at a time (the backend calls the error
/// callback from one thread, or under a lock), so the message and the error
/// recorded first belong together.
#[derive(Debug, Default)]
pub(crate) struct StreamStatus {
    /// 0 while there is no error. Otherwise the [`StreamError`] code in the
    /// high byte and the [`ErrorDetail`] code in the low byte.
    failure: AtomicU16,
    note: Note,
    xruns: AtomicU64,
    callbacks: AtomicU64,
    callback_frames: AtomicU32,
    realtime_denied: AtomicBool,
}

impl StreamStatus {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Keeps the backend's message for the fatal error that is about to be
    /// recorded with [`set_failure`](Self::set_failure). Call it first.
    /// Only the first message is kept, and none once an error is recorded:
    /// the message of a later error would not belong to the first one.
    pub(crate) fn set_note(&self, message: &str) {
        if self.failure.load(Ordering::Relaxed) != 0 || !self.note.claim() {
            return;
        }
        let len = self.note.write_bytes(message);
        self.note.publish(len);
    }

    /// Records a fatal error and what the backend reported, in one step.
    /// Only the first one is kept: a later call changes neither.
    pub(crate) fn set_failure(&self, error: StreamError, detail: ErrorDetail) {
        let packed = (u16::from(error.code()) << 8) | u16::from(detail.code());
        let _ = self
            .failure
            .compare_exchange(0, packed, Ordering::Release, Ordering::Relaxed);
    }

    pub(crate) fn error(&self) -> Option<StreamError> {
        let packed = self.failure.load(Ordering::Acquire);
        StreamError::from_code((packed >> 8) as u8)
    }

    /// The detail of the fatal error. `Some` whenever [`error`](Self::error)
    /// is `Some`: the value goes from 0 to its final form once and never
    /// changes, so a later load cannot see less than an earlier one.
    pub(crate) fn detail(&self) -> Option<ErrorDetail> {
        let packed = self.failure.load(Ordering::Acquire);
        ErrorDetail::from_code((packed & 0xff) as u8)
    }

    pub(crate) fn has_error(&self) -> bool {
        self.failure.load(Ordering::Acquire) != 0
    }

    /// The fatal error with its detail and message, read in one place: the
    /// message is complete whenever the error is seen.
    pub(crate) fn failure(&self) -> Option<Failure> {
        let packed = self.failure.load(Ordering::Acquire);
        let error = StreamError::from_code((packed >> 8) as u8)?;
        Some(Failure {
            error,
            detail: ErrorDetail::from_code((packed & 0xff) as u8).unwrap_or(ErrorDetail::Unknown),
            note: self.note.read(),
        })
    }

    pub(crate) fn add_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn xruns(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }

    /// Records one data callback from the device with `frames` frames.
    pub(crate) fn record_callback(&self, frames: usize) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        self.callback_frames
            .store(u32::try_from(frames).unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    pub(crate) fn callbacks(&self) -> u64 {
        self.callbacks.load(Ordering::Relaxed)
    }

    pub(crate) fn callback_frames(&self) -> u32 {
        self.callback_frames.load(Ordering::Relaxed)
    }

    pub(crate) fn set_realtime_denied(&self) {
        self.realtime_denied.store(true, Ordering::Relaxed);
    }

    pub(crate) fn realtime_denied(&self) -> bool {
        self.realtime_denied.load(Ordering::Relaxed)
    }
}

/// Marks an [`AudioStream`] that captures audio (an input stream).
#[derive(Debug)]
pub enum Capture {}

/// Marks an [`AudioStream`] that plays audio (an output stream).
#[derive(Debug)]
pub enum Playback {}

/// An open input stream.
pub type InputStream = AudioStream<Capture>;

/// An open output stream.
pub type OutputStream = AudioStream<Playback>;

/// An open audio stream. It stops when dropped.
///
/// Use it as [`InputStream`] or [`OutputStream`]: the direction is part of
/// the type, so an input and an output cannot be mixed up.
///
/// Create it on a control thread (the main thread of the CLI, the UI thread
/// of the GUI) and keep it there; it is `Send`, so it can also be moved to
/// another thread. To change devices, drop it and open a new one.
///
/// Check [`AudioStream::error`] and [`AudioStream::callbacks`] regularly:
/// some systems stop calling back without reporting an error when a device
/// is unplugged.
pub struct AudioStream<D> {
    /// The backend's stream object. Dropping it stops the stream.
    _stream: Box<dyn Send>,
    status: Arc<StreamStatus>,
    config: StreamConfig,
    sample_format: &'static str,
    _direction: PhantomData<fn() -> D>,
}

impl<D> AudioStream<D> {
    /// `sample_format` is the device's sample format as a short name such as
    /// `"f32"` or `"i16"`.
    pub(crate) fn new(
        stream: Box<dyn Send>,
        status: Arc<StreamStatus>,
        config: StreamConfig,
        sample_format: &'static str,
    ) -> Self {
        Self {
            _stream: stream,
            status,
            config,
            sample_format,
            _direction: PhantomData,
        }
    }

    /// Returns the format the stream was opened with.
    pub fn config(&self) -> StreamConfig {
        self.config
    }

    /// Returns the sample format of the device, such as `"f32"` or
    /// `"i16"`. Samples are converted between it and f32. For display and
    /// diagnostics.
    pub fn sample_format(&self) -> &'static str {
        self.sample_format
    }

    /// Returns the fatal error of the stream, if one occurred.
    pub fn error(&self) -> Option<StreamError> {
        self.status.error()
    }

    /// Returns what the backend reported for the fatal error, if one
    /// occurred. `Some` whenever [`error`](Self::error) is `Some`. For log
    /// lines; do not branch on it.
    pub fn error_detail(&self) -> Option<ErrorDetail> {
        self.status.detail()
    }

    /// Returns the fatal error with what the backend reported and its own
    /// message, if one occurred. Read the three together here: the message is
    /// complete whenever the error is seen. For log lines; do not branch on
    /// the detail or the message.
    pub fn failure(&self) -> Option<Failure> {
        self.status.failure()
    }

    /// Returns how many overruns the backend reported.
    pub fn xruns(&self) -> u64 {
        self.status.xruns()
    }

    /// Returns how many data callbacks the device has made. If this stops
    /// increasing while the stream is open, the stream has stopped.
    pub fn callbacks(&self) -> u64 {
        self.status.callbacks()
    }

    /// Returns the number of frames in the latest data callback from the
    /// device (before splitting into [`MAX_FRAMES_PER_CALLBACK`]).
    pub fn callback_frames(&self) -> u32 {
        self.status.callback_frames()
    }

    /// Returns whether the OS refused realtime scheduling for the audio
    /// thread. Audio still runs, but glitches are more likely under load.
    pub fn realtime_denied(&self) -> bool {
        self.status.realtime_denied()
    }
}

impl<D> fmt::Debug for AudioStream<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioStream")
            .field("config", &self.config)
            .field("sample_format", &self.sample_format)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

// Streams must be movable between control threads (#41). Boxing the
// backend's stream as `dyn Send` already requires the stream to be `Send`;
// this keeps the whole handle `Send` as fields are added.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<InputStream>();
    assert_send::<OutputStream>();
};

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: StreamConfig = StreamConfig {
        sample_rate: 48_000,
        channels: 2,
    };

    #[test]
    fn new_status_has_no_error_and_zero_counts() {
        let status = StreamStatus::new();
        assert_eq!(status.error(), None);
        assert!(!status.has_error());
        assert_eq!(status.xruns(), 0);
        assert_eq!(status.callbacks(), 0);
        assert_eq!(status.callback_frames(), 0);
        assert!(!status.realtime_denied());
    }

    #[test]
    fn first_error_is_kept() {
        let status = StreamStatus::new();
        status.set_failure(
            StreamError::DeviceUnavailable,
            ErrorDetail::DeviceNotAvailable,
        );
        status.set_failure(StreamError::Other, ErrorDetail::BackendError);
        assert_eq!(status.error(), Some(StreamError::DeviceUnavailable));
        assert!(status.has_error());
    }

    #[test]
    fn new_status_has_no_detail() {
        let status = StreamStatus::new();
        assert_eq!(status.detail(), None);
    }

    #[test]
    fn failure_keeps_error_and_detail_together() {
        let status = StreamStatus::new();
        status.set_failure(StreamError::Other, ErrorDetail::BackendError);
        assert_eq!(status.error(), Some(StreamError::Other));
        assert_eq!(status.detail(), Some(ErrorDetail::BackendError));
        assert!(status.has_error());
    }

    #[test]
    fn first_failure_keeps_its_detail() {
        let status = StreamStatus::new();
        status.set_failure(StreamError::DeviceUnavailable, ErrorDetail::DeviceBusy);
        status.set_failure(StreamError::Other, ErrorDetail::BackendError);
        status.set_failure(StreamError::PermissionDenied, ErrorDetail::PermissionDenied);
        assert_eq!(status.error(), Some(StreamError::DeviceUnavailable));
        assert_eq!(status.detail(), Some(ErrorDetail::DeviceBusy));
    }

    #[test]
    fn unknown_detail_is_a_detail_not_none() {
        let status = StreamStatus::new();
        status.set_failure(StreamError::Other, ErrorDetail::Unknown);
        assert_eq!(status.error(), Some(StreamError::Other));
        assert_eq!(status.detail(), Some(ErrorDetail::Unknown));
    }

    #[test]
    fn every_detail_survives_the_round_trip() {
        for detail in ErrorDetail::ALL {
            let status = StreamStatus::new();
            status.set_failure(StreamError::Other, detail);
            assert_eq!(status.detail(), Some(detail), "{detail}");
        }
    }

    #[test]
    fn detail_codes_are_in_order_and_zero_is_none() {
        assert_eq!(ErrorDetail::from_code(0), None);
        assert_eq!(ErrorDetail::from_code(13), None);
        for (index, detail) in ErrorDetail::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(detail.code()), index + 1, "{detail}");
        }
    }

    #[test]
    fn detail_names_are_distinct_lowercase_words() {
        let names: Vec<&str> = ErrorDetail::ALL.iter().map(|d| d.as_str()).collect();
        for (index, name) in names.iter().enumerate() {
            assert!(!name.is_empty());
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{name}"
            );
            assert!(!names[..index].contains(name), "{name} appears twice");
        }
        assert_eq!(ErrorDetail::BackendError.to_string(), "backend-error");
    }

    /// The control thread must never see the error without its detail, even
    /// while the audio thread is writing it.
    #[test]
    fn error_is_never_seen_without_its_detail() {
        for _ in 0..2_000 {
            let status = Arc::new(StreamStatus::new());
            let writer = {
                let status = Arc::clone(&status);
                std::thread::spawn(move || {
                    status.set_failure(StreamError::Other, ErrorDetail::BackendError);
                })
            };
            while status.error().is_none() {
                std::hint::spin_loop();
            }
            assert_eq!(status.detail(), Some(ErrorDetail::BackendError));
            writer.join().unwrap();
        }
    }

    /// Writes `message` into a fresh note and publishes it, as the audio
    /// thread does.
    fn noted(message: &str) -> Option<String> {
        let note = Note::default();
        let len = note.write_bytes(message);
        note.publish(len);
        note.read()
    }

    #[test]
    fn a_message_is_kept_as_it_is() {
        assert_eq!(
            noted("snd_pcm_readi: Input/output error").as_deref(),
            Some("snd_pcm_readi: Input/output error")
        );
        assert_eq!(
            noted("入力デバイスが無効です").as_deref(),
            Some("入力デバイスが無効です")
        );
    }

    #[test]
    fn a_message_cannot_be_read_before_its_length_is_published() {
        let note = Note::default();
        let len = note.write_bytes("snd_pcm_readi: Input/output error");
        assert!(len > 0);
        assert_eq!(note.read(), None, "the bytes are there, the length is not");
        note.publish(len);
        assert_eq!(
            note.read().as_deref(),
            Some("snd_pcm_readi: Input/output error")
        );
    }

    #[test]
    fn a_blank_message_is_no_message() {
        assert_eq!(noted(""), None);
        assert_eq!(noted("  \r\n\t "), None);
    }

    #[test]
    fn control_characters_become_spaces_and_the_ends_are_trimmed() {
        assert_eq!(
            noted("Failed: device\r\n").as_deref(),
            Some("Failed: device")
        );
        assert_eq!(noted("a\nb\tc").as_deref(), Some("a b c"));
        assert_eq!(noted("\u{85}x\u{85}y").as_deref(), Some("x y"));
    }

    #[test]
    fn a_message_that_fits_exactly_is_not_marked_as_cut() {
        let exact = "a".repeat(NOTE_CAPACITY);
        assert_eq!(noted(&exact).as_deref(), Some(exact.as_str()));

        let one_more = "a".repeat(NOTE_CAPACITY + 1);
        let cut = noted(&one_more).unwrap();
        assert_eq!(cut.len(), NOTE_CAPACITY);
        assert!(cut.ends_with("..."));
        assert!(cut.starts_with(&"a".repeat(NOTE_CAPACITY - 3)));
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        // 1, 2 and 3 byte characters, in every length around the limit.
        let pieces = ["a", "é", "あ", "漢"];
        for start in 0..pieces.len() {
            for count in 1..=80 {
                let message: String = (0..count)
                    .map(|i| pieces[(start + i) % pieces.len()])
                    .collect();
                let kept = noted(&message)
                    .unwrap_or_else(|| panic!("not valid UTF-8 for {count} characters"));
                assert!(kept.len() <= NOTE_CAPACITY);
                if message.len() <= NOTE_CAPACITY {
                    assert_eq!(kept, message);
                } else {
                    let body = kept
                        .strip_suffix("...")
                        .expect("a cut message ends with ...");
                    assert!(message.starts_with(body));
                    // As much as fits: one more character would not.
                    let next = message[body.len()..].chars().next().unwrap();
                    assert!(body.len() + next.len_utf8() > NOTE_CAPACITY - 3);
                }
            }
        }
    }

    #[test]
    fn only_the_first_message_is_kept() {
        let status = StreamStatus::new();
        status.set_note("first");
        status.set_note("second");
        status.set_failure(StreamError::Other, ErrorDetail::BackendError);
        assert_eq!(status.failure().unwrap().note.as_deref(), Some("first"));
    }

    #[test]
    fn a_message_after_the_error_is_ignored() {
        let status = StreamStatus::new();
        status.set_failure(StreamError::Other, ErrorDetail::DeviceBusy);
        status.set_note("belongs to a later error");
        assert_eq!(status.failure().unwrap().note, None);
    }

    #[test]
    fn a_first_error_without_a_message_keeps_a_later_message_out() {
        let status = StreamStatus::new();
        status.set_failure(StreamError::Other, ErrorDetail::DeviceBusy);
        status.set_note("second error");
        status.set_failure(StreamError::Other, ErrorDetail::BackendError);
        let failure = status.failure().unwrap();
        assert_eq!(failure.detail, ErrorDetail::DeviceBusy);
        assert_eq!(failure.note, None);
    }

    #[test]
    fn failure_reports_the_error_the_detail_and_the_message_together() {
        let status = StreamStatus::new();
        assert_eq!(status.failure(), None);
        status.set_note("snd_pcm_readi: Input/output error\n");
        status.set_failure(StreamError::Other, ErrorDetail::BackendError);
        assert_eq!(
            status.failure(),
            Some(Failure {
                error: StreamError::Other,
                detail: ErrorDetail::BackendError,
                note: Some("snd_pcm_readi: Input/output error".to_string()),
            })
        );
    }

    #[test]
    fn an_error_without_a_message_has_no_note() {
        let status = StreamStatus::new();
        status.set_failure(
            StreamError::DeviceUnavailable,
            ErrorDetail::DeviceNotAvailable,
        );
        let failure = status.failure().unwrap();
        assert_eq!(failure.note, None);
        assert_eq!(failure.error, StreamError::DeviceUnavailable);
    }

    #[test]
    fn the_stream_hands_out_the_failure() {
        let status = Arc::new(StreamStatus::new());
        let stream = AudioStream::<Capture>::new(Box::new(()), Arc::clone(&status), CONFIG, "f32");
        assert_eq!(stream.failure(), None);
        status.set_note("boom");
        status.set_failure(StreamError::Other, ErrorDetail::Other);
        let failure = stream.failure().unwrap();
        assert_eq!(failure.note.as_deref(), Some("boom"));
        assert_eq!(stream.error(), Some(failure.error));
        assert_eq!(stream.error_detail(), Some(failure.detail));
    }

    const LONG_MESSAGE: &str = "Failed to get audio clock frequency: \
        入力デバイスは無効になりました。デバイスが取り外されたか、無効化された可能性があります。 (os error 88890004)";

    /// The control thread must never see the error with a half-written
    /// message, while the audio thread is writing both.
    #[test]
    fn the_message_is_complete_whenever_the_error_is_seen() {
        let expected = noted(LONG_MESSAGE).unwrap();
        for _ in 0..2_000 {
            let status = Arc::new(StreamStatus::new());
            let writer = {
                let status = Arc::clone(&status);
                std::thread::spawn(move || {
                    status.set_note(LONG_MESSAGE);
                    status.set_failure(StreamError::Other, ErrorDetail::BackendError);
                })
            };
            let failure = loop {
                if let Some(failure) = status.failure() {
                    break failure;
                }
                std::hint::spin_loop();
            };
            assert_eq!(failure.note.as_deref(), Some(expected.as_str()));
            writer.join().unwrap();
        }
    }

    /// Whoever sees the length sees all the bytes before it.
    #[test]
    fn a_published_length_always_comes_with_all_its_bytes() {
        let expected = noted(LONG_MESSAGE).unwrap();
        for _ in 0..2_000 {
            let note = Arc::new(Note::default());
            let writer = {
                let note = Arc::clone(&note);
                std::thread::spawn(move || {
                    let len = note.write_bytes(LONG_MESSAGE);
                    note.publish(len);
                })
            };
            let seen = loop {
                if let Some(seen) = note.read() {
                    break seen;
                }
                std::hint::spin_loop();
            };
            assert_eq!(seen, expected);
            writer.join().unwrap();
        }
    }

    #[test]
    fn xruns_are_counted() {
        let status = StreamStatus::new();
        status.add_xrun();
        status.add_xrun();
        assert_eq!(status.xruns(), 2);
    }

    #[test]
    fn callbacks_are_counted_with_latest_size() {
        let status = StreamStatus::new();
        status.record_callback(480);
        status.record_callback(512);
        assert_eq!(status.callbacks(), 2);
        assert_eq!(status.callback_frames(), 512);
    }

    #[test]
    fn realtime_denied_is_flagged() {
        let status = StreamStatus::new();
        status.set_realtime_denied();
        assert!(status.realtime_denied());
    }

    fn check_handle_reports_status_and_config<D>() {
        let status = Arc::new(StreamStatus::new());
        let stream = AudioStream::<D>::new(Box::new(()), Arc::clone(&status), CONFIG, "i16");
        status.record_callback(256);
        assert_eq!(stream.error_detail(), None);
        status.set_failure(StreamError::PermissionDenied, ErrorDetail::PermissionDenied);

        assert_eq!(stream.config(), CONFIG);
        assert_eq!(stream.sample_format(), "i16");
        assert_eq!(stream.callbacks(), 1);
        assert_eq!(stream.callback_frames(), 256);
        assert_eq!(stream.error(), Some(StreamError::PermissionDenied));
        assert_eq!(stream.error_detail(), Some(ErrorDetail::PermissionDenied));
    }

    #[test]
    fn handle_reports_status_and_config() {
        check_handle_reports_status_and_config::<Capture>();
        check_handle_reports_status_and_config::<Playback>();
    }

    fn check_dropping_handle_drops_backend_stream<D>() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let stream = AudioStream::<D>::new(
            Box::new(Flag(Arc::clone(&dropped))),
            Arc::new(StreamStatus::new()),
            CONFIG,
            "f32",
        );
        assert!(!dropped.load(Ordering::Relaxed));
        drop(stream);
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[test]
    fn dropping_handle_drops_backend_stream() {
        check_dropping_handle_drops_backend_stream::<Capture>();
        check_dropping_handle_drops_backend_stream::<Playback>();
    }
}
