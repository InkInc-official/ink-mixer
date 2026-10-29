//! What the sound library needs to know about a file (ADR-0010).
//!
//! `ink-core` keeps the sound library, but does not read sound files. The
//! application gives it [`asset_info`] as the function that does: it reads
//! what a file says about its audio with [`probe`], and turns it into the
//! types of `ink-core`.
//!
//! Like everything in this crate, this reads a file: call it from a control or
//! worker thread, never from the audio thread.

use std::path::Path;

use ink_core::{AssetFormat, AssetInfo, ProbeFailure};

use crate::{MediaError, SoundFormat, SoundInfo, probe};

/// Reads what the file at `path` says about its audio, as the information of
/// an Asset of the sound library. The file is not decoded.
///
/// The frames are the sound that is played (for an MP3 file with the
/// encoder's delay in its header, they are not counted), or `None` if the file
/// does not say. It is the same as [`probe`] gives.
///
/// This is the `probe` function that `Library::scan` and `Library::import`
/// of `ink-core` take.
///
/// # Errors
///
/// - [`ProbeFailure::NotAudio`] if the file is not a WAV or MP3 file that can
///   be played: another format, a file with no sound, or a broken one
/// - [`ProbeFailure::Unreadable`] if the file could not be opened or read (it
///   is not there, no permission, ...)
pub fn asset_info(path: &Path) -> Result<AssetInfo, ProbeFailure> {
    probe(path)
        .map(|info| to_asset_info(&info))
        .map_err(probe_failure)
}

fn to_asset_info(info: &SoundInfo) -> AssetInfo {
    AssetInfo {
        format: asset_format(info.format()),
        sample_rate: info.sample_rate(),
        channels: info.channels(),
        frames: info.frames(),
    }
}

fn asset_format(format: SoundFormat) -> AssetFormat {
    match format {
        SoundFormat::Wav => AssetFormat::Wav,
        SoundFormat::Mp3 => AssetFormat::Mp3,
    }
}

/// Turns the error of reading a file into the reason the library tells.
///
/// | `MediaError` | `ProbeFailure` | Why |
/// |---|---|---|
/// | `Io` | `Unreadable` | the file could not be read: trying again later may work |
/// | `UnsupportedFormat` | `NotAudio` | not a WAV or MP3 file |
/// | `NoAudioTrack` | `NotAudio` | nothing to play |
/// | `Empty` | `NotAudio` | no sound in it |
/// | `Decode` | `NotAudio` | the content is broken |
/// | `InvalidSampleRate` | `NotAudio` | the header is broken |
/// | `TooLong`, `Resample` | `NotAudio` | not made by a probe; if it were, the file cannot be played as it is |
///
/// Every kind of `MediaError` is written out: a new one does not compile
/// until it is put in this table.
fn probe_failure(error: MediaError) -> ProbeFailure {
    let reason = error.to_string();
    match error {
        MediaError::Io(_) => ProbeFailure::Unreadable(reason),
        MediaError::UnsupportedFormat
        | MediaError::NoAudioTrack
        | MediaError::Empty
        | MediaError::Decode(_)
        | MediaError::InvalidSampleRate(_)
        | MediaError::TooLong { .. }
        | MediaError::Resample(_) => ProbeFailure::NotAudio(reason),
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_two_formats_are_not_mixed_up() {
        assert_eq!(asset_format(SoundFormat::Wav), AssetFormat::Wav);
        assert_eq!(asset_format(SoundFormat::Mp3), AssetFormat::Mp3);
    }

    #[test]
    fn a_failure_to_read_is_unreadable_and_everything_else_is_not_audio() {
        let io = |kind| MediaError::Io(io::Error::from(kind));
        for error in [
            io(io::ErrorKind::NotFound),
            io(io::ErrorKind::PermissionDenied),
            io(io::ErrorKind::UnexpectedEof),
        ] {
            assert!(
                matches!(probe_failure(error), ProbeFailure::Unreadable(_)),
                "an I/O error is unreadable"
            );
        }

        let not_audio = [
            MediaError::UnsupportedFormat,
            MediaError::NoAudioTrack,
            MediaError::Empty,
            MediaError::Decode("broken".to_string()),
            MediaError::InvalidSampleRate(0),
            MediaError::InvalidSampleRate(1),
            MediaError::TooLong {
                limit: Duration::from_secs(1),
            },
            MediaError::Resample("x".to_string()),
        ];
        for error in not_audio {
            let text = error.to_string();
            match probe_failure(error) {
                ProbeFailure::NotAudio(reason) => {
                    assert_eq!(reason, text, "the reason is what the error says");
                }
                other => panic!("{text}: {other}"),
            }
        }
    }

    #[test]
    fn the_reason_of_an_io_error_says_what_went_wrong() {
        let error = MediaError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        match probe_failure(error) {
            ProbeFailure::Unreadable(reason) => assert!(reason.contains("denied"), "{reason}"),
            other => panic!("{other}"),
        }
    }
}
