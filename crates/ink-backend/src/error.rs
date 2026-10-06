//! Error type shared by all audio backends.

use std::fmt;

/// An error returned by an [`AudioBackend`](crate::AudioBackend).
///
/// Backend-specific error types (for example `cpal::Error`) are never stored
/// here; implementations convert them into a message so that those types do
/// not leak out of this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendError {
    /// The audio system (host) itself could not be queried.
    HostUnavailable(String),
    /// A specific device could not be found or used (for example, it was
    /// unplugged).
    DeviceUnavailable(String),
    /// The device does not support the requested stream configuration
    /// (sample rate, channel count).
    UnsupportedConfig(String),
    /// Any other backend-specific failure.
    Other(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable(msg) => write!(f, "audio host unavailable: {msg}"),
            Self::DeviceUnavailable(msg) => write!(f, "audio device unavailable: {msg}"),
            Self::UnsupportedConfig(msg) => write!(f, "unsupported stream configuration: {msg}"),
            Self::Other(msg) => write!(f, "audio backend error: {msg}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// Result type used by [`AudioBackend`](crate::AudioBackend) methods.
pub type Result<T> = std::result::Result<T, BackendError>;
