//! Ink Mixer media: reading audio files and resampling (ADR-0009).
//!
//! This crate decodes audio files (WAV and MP3) into the sound data the
//! mixer plays, and converts the sample rate of that data. It keeps the
//! decoders (`symphonia`, MPL-2.0) and the resampler (`rubato`) out of
//! `ink-core`, which stays free of them, as it stays free of `cpal`
//! (ADR-0003). The dependency points one way: `ink-media` depends on
//! `ink-core` (for `ink_core::graph::SoundData`), never the other way.
//!
//! # Not for the audio thread
//!
//! Everything here reads files, allocates memory, and takes unbounded time.
//! Call it only from a control thread or a worker thread, never from an
//! audio callback or any other realtime path (AGENTS.md §5, ADR-0009). The
//! audio thread only reads sound that was prepared here beforehand.
//!
//! # Logging
//!
//! The crate does not log. Where something should be reported to the user
//! (for example, how many undecodable packets were skipped), it returns the
//! information, and the application's worker thread decides what to log.

#![forbid(unsafe_code)]

mod decode;
mod error;
mod reader;
mod resample;
mod stream;
mod stream_resample;
#[cfg(test)]
mod test_util;

pub use decode::{Decoded, decode_file};
pub use error::MediaError;
pub use reader::{SoundFormat, SoundInfo, SoundReader, probe};
pub use resample::{load_sound, resample};
pub use stream::{Chunk, SoundStream};
