//! Backend-independent stream types.
//!
//! No `cpal` type appears here: a backend keeps its own stream object inside
//! [`InputStream`] as an opaque box.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

/// The format of a stream: sample rate and number of channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
}

/// The largest number of frames passed to an [`InputCallback`] in one call.
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

/// State written by the audio thread and read by the control thread.
///
/// Only atomics, so the audio thread can update it without allocating or
/// blocking. `Ordering::Relaxed` is enough: each value stands on its own and
/// is not used to publish other data.
#[derive(Debug, Default)]
pub(crate) struct StreamStatus {
    /// 0 while there is no error, otherwise a [`StreamError`] code.
    error: AtomicU8,
    xruns: AtomicU64,
    callbacks: AtomicU64,
    callback_frames: AtomicU32,
    realtime_denied: AtomicBool,
}

impl StreamStatus {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records a fatal error. Only the first one is kept.
    pub(crate) fn set_error(&self, error: StreamError) {
        let _ = self
            .error
            .compare_exchange(0, error.code(), Ordering::Relaxed, Ordering::Relaxed);
    }

    pub(crate) fn error(&self) -> Option<StreamError> {
        StreamError::from_code(self.error.load(Ordering::Relaxed))
    }

    pub(crate) fn has_error(&self) -> bool {
        self.error.load(Ordering::Relaxed) != 0
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

/// An open input stream. Capturing stops when it is dropped.
///
/// Create it on a control thread (the main thread of the CLI, the UI thread
/// of the GUI) and keep it there; it is `Send`, so it can also be moved to
/// another thread. To change devices, drop it and open a new one.
///
/// Check [`InputStream::error`] and [`InputStream::callbacks`] regularly:
/// some systems stop calling back without reporting an error when a device
/// is unplugged.
pub struct InputStream {
    /// The backend's stream object. Dropping it stops the stream.
    _stream: Box<dyn Send>,
    status: Arc<StreamStatus>,
    config: StreamConfig,
    sample_format: &'static str,
}

impl InputStream {
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
        }
    }

    /// Returns the format the stream was opened with.
    pub fn config(&self) -> StreamConfig {
        self.config
    }

    /// Returns the sample format delivered by the device, such as `"f32"`
    /// or `"i16"`, before conversion to f32. For display and diagnostics.
    pub fn sample_format(&self) -> &'static str {
        self.sample_format
    }

    /// Returns the fatal error of the stream, if one occurred.
    pub fn error(&self) -> Option<StreamError> {
        self.status.error()
    }

    /// Returns how many overruns the backend reported.
    pub fn xruns(&self) -> u64 {
        self.status.xruns()
    }

    /// Returns how many data callbacks the device has made. If this stops
    /// increasing while the stream is open, the input has stopped.
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

impl fmt::Debug for InputStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputStream")
            .field("config", &self.config)
            .field("sample_format", &self.sample_format)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

// An `InputStream` must be movable between control threads (#41). Boxing the
// backend's stream as `dyn Send` already requires the stream to be `Send`;
// this keeps the whole handle `Send` as fields are added.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<InputStream>();
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
        status.set_error(StreamError::DeviceUnavailable);
        status.set_error(StreamError::Other);
        assert_eq!(status.error(), Some(StreamError::DeviceUnavailable));
        assert!(status.has_error());
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

    #[test]
    fn handle_reports_status_and_config() {
        let status = Arc::new(StreamStatus::new());
        let stream = InputStream::new(Box::new(()), Arc::clone(&status), CONFIG, "i16");
        status.record_callback(256);
        status.set_error(StreamError::PermissionDenied);

        assert_eq!(stream.config(), CONFIG);
        assert_eq!(stream.sample_format(), "i16");
        assert_eq!(stream.callbacks(), 1);
        assert_eq!(stream.callback_frames(), 256);
        assert_eq!(stream.error(), Some(StreamError::PermissionDenied));
    }

    #[test]
    fn dropping_handle_drops_backend_stream() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let stream = InputStream::new(
            Box::new(Flag(Arc::clone(&dropped))),
            Arc::new(StreamStatus::new()),
            CONFIG,
            "f32",
        );
        assert!(!dropped.load(Ordering::Relaxed));
        drop(stream);
        assert!(dropped.load(Ordering::Relaxed));
    }
}
