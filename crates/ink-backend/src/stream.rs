//! Backend-independent stream types.
//!
//! No `cpal` type appears here: a backend keeps its own stream object inside
//! [`AudioStream`] as an opaque box.

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};

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

/// State written by the audio thread and read by the control thread.
///
/// Only atomics, so the audio thread can update it without allocating or
/// blocking. `Ordering::Relaxed` is enough: each value stands on its own and
/// is not used to publish other data.
///
/// The fatal error and its [`ErrorDetail`] share one atomic, so the control
/// thread can never see the error without its detail. Two atomics would need
/// the detail written first and a release/acquire pair on the error.
#[derive(Debug, Default)]
pub(crate) struct StreamStatus {
    /// 0 while there is no error. Otherwise the [`StreamError`] code in the
    /// high byte and the [`ErrorDetail`] code in the low byte.
    failure: AtomicU16,
    xruns: AtomicU64,
    callbacks: AtomicU64,
    callback_frames: AtomicU32,
    realtime_denied: AtomicBool,
}

impl StreamStatus {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records a fatal error and what the backend reported, in one step.
    /// Only the first one is kept: a later call changes neither.
    pub(crate) fn set_failure(&self, error: StreamError, detail: ErrorDetail) {
        let packed = (u16::from(error.code()) << 8) | u16::from(detail.code());
        let _ = self
            .failure
            .compare_exchange(0, packed, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub(crate) fn error(&self) -> Option<StreamError> {
        let packed = self.failure.load(Ordering::Relaxed);
        StreamError::from_code((packed >> 8) as u8)
    }

    /// The detail of the fatal error. `Some` whenever [`error`](Self::error)
    /// is `Some`: the value goes from 0 to its final form once and never
    /// changes, so a later load cannot see less than an earlier one.
    pub(crate) fn detail(&self) -> Option<ErrorDetail> {
        let packed = self.failure.load(Ordering::Relaxed);
        ErrorDetail::from_code((packed & 0xff) as u8)
    }

    pub(crate) fn has_error(&self) -> bool {
        self.failure.load(Ordering::Relaxed) != 0
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
