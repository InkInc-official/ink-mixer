//! The error type of this crate.

use std::fmt;
use std::io;
use std::time::Duration;

/// An error while reading or resampling an audio file.
///
/// The error types of the libraries behind this crate (`symphonia`,
/// `rubato`) are turned into messages, so that they do not appear in this
/// crate's API (the same way `ink_core::ConfigError` does it).
#[derive(Debug)]
#[non_exhaustive]
pub enum MediaError {
    /// The file could not be opened or read.
    Io(io::Error),
    /// The file is not a WAV or MP3 file (or its codec is not supported).
    UnsupportedFormat,
    /// The file has no audio track.
    NoAudioTrack,
    /// The file has no sound: it has no frames, or none could be decoded.
    Empty,
    /// The audio could not be decoded.
    Decode(String),
    /// The audio is longer than the allowed length.
    TooLong {
        /// The allowed length.
        limit: Duration,
    },
    /// The sample rate is 0, or one the resampler cannot use. A broken file
    /// can have a sample rate of 0 in its header.
    InvalidSampleRate(u32),
    /// The audio could not be resampled.
    Resample(String),
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "audio file I/O error: {e}"),
            Self::UnsupportedFormat => write!(
                f,
                "unsupported audio format (only WAV and MP3 are supported)"
            ),
            Self::NoAudioTrack => write!(f, "the file has no audio track"),
            Self::Empty => write!(f, "the audio file has no sound"),
            Self::Decode(msg) => write!(f, "failed to decode the audio: {msg}"),
            Self::TooLong { limit } => write!(
                f,
                "the audio is longer than the limit of {} s",
                limit.as_secs_f64()
            ),
            Self::InvalidSampleRate(rate) => write!(f, "invalid sample rate: {rate} Hz"),
            Self::Resample(msg) => write!(f, "failed to resample the audio: {msg}"),
        }
    }
}

impl std::error::Error for MediaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for MediaError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn messages_say_what_went_wrong() {
        assert_eq!(
            MediaError::UnsupportedFormat.to_string(),
            "unsupported audio format (only WAV and MP3 are supported)"
        );
        assert_eq!(
            MediaError::NoAudioTrack.to_string(),
            "the file has no audio track"
        );
        assert_eq!(MediaError::Empty.to_string(), "the audio file has no sound");
        assert_eq!(
            MediaError::Decode("bad frame".to_string()).to_string(),
            "failed to decode the audio: bad frame"
        );
        assert_eq!(
            MediaError::Resample("bad ratio".to_string()).to_string(),
            "failed to resample the audio: bad ratio"
        );
        assert_eq!(
            MediaError::InvalidSampleRate(0).to_string(),
            "invalid sample rate: 0 Hz"
        );
    }

    #[test]
    fn the_length_limit_is_shown_in_seconds() {
        let sixty = MediaError::TooLong {
            limit: Duration::from_secs(60),
        };
        assert_eq!(
            sixty.to_string(),
            "the audio is longer than the limit of 60 s"
        );
        let half = MediaError::TooLong {
            limit: Duration::from_millis(500),
        };
        assert_eq!(
            half.to_string(),
            "the audio is longer than the limit of 0.5 s"
        );
    }

    #[test]
    fn an_io_error_is_converted_and_kept_as_the_source() {
        let io = io::Error::new(io::ErrorKind::NotFound, "no such file");
        let error = MediaError::from(io);
        assert!(matches!(&error, MediaError::Io(e) if e.kind() == io::ErrorKind::NotFound));
        assert_eq!(error.to_string(), "audio file I/O error: no such file");
        assert!(error.source().is_some());
        assert!(MediaError::Empty.source().is_none());
    }

    #[test]
    fn the_error_can_cross_threads() {
        // A worker thread returns it to the control thread.
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<MediaError>();
    }
}
