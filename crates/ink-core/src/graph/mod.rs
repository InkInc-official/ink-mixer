//! The Audio Graph: Source -> Node -> Mixer -> Sink (ARCHITECTURE.md §6).
//!
//! Phase 1 uses a fixed path, `MIC -> Gain -> Mixer -> MASTER Gain -> Sink`,
//! built from the types here (ADR-0007). Samples are f32 and processed in
//! blocks; buffers are allocated before the stream starts, never on the
//! realtime path.
//!
//! Values cross between the UI and the audio thread only through atomics:
//! gains and mute through [`MixerControl`], levels through [`PeakMeter`]
//! (ADR-0007).
//!
//! This module does no I/O. Sources and sinks that talk to audio devices are
//! added with the first stream implementation.

mod buffer;
mod control;
mod gain;
mod level;
mod meter;
mod mixer;

pub use buffer::AudioBuffer;
pub use control::{ChannelControl, MixerControl};
pub use gain::GainNode;
pub use level::{
    DbRange, MASTER_VOLUME_RANGE, MIC_GAIN_RANGE, SILENCE_DB, block_peak, db_to_gain, gain_to_db,
};
pub use meter::PeakMeter;
pub use mixer::MixerNode;

/// A processing step that transforms one buffer in place.
///
/// Implementations are moved to the audio thread, so they must be `Send`.
pub trait AudioNode: Send {
    /// Prepares for processing at `sample_rate`.
    ///
    /// Called outside the realtime path, for example when a stream is
    /// opened. Anything that depends on the sample rate is computed here.
    fn prepare(&mut self, sample_rate: u32);

    /// Processes the frames in use of `buffer` in place.
    ///
    /// Called on the realtime path: implementations must not allocate,
    /// block, or log.
    fn process(&mut self, buffer: &mut AudioBuffer);
}
