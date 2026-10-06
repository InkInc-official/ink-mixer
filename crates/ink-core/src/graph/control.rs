//! Mixer controls shared between the UI and the audio thread.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::{DbRange, MASTER_VOLUME_RANGE, MIC_GAIN_RANGE, PeakMeter, db_to_gain};

/// An f32 stored as its bit pattern in an `AtomicU32`.
#[derive(Debug)]
struct AtomicF32(AtomicU32);

impl AtomicF32 {
    fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn store(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// The controls and the meter of one channel.
///
/// The UI sets the gain in dB and the mute state; the audio thread reads
/// them as linear gains every block and passes them to
/// [`GainNode::set_target`](super::GainNode::set_target). The audio thread
/// writes the meter and the UI reads it (ADR-0007).
///
/// All values are atomics with `Ordering::Relaxed`: each value stands on its
/// own and is not used to publish other data. A change that becomes visible
/// one block late is fine, and the gain nodes smooth every change anyway.
#[derive(Debug)]
pub struct ChannelControl {
    range: DbRange,
    /// The gain in dB, clamped to `range`. Read by the UI and the settings.
    gain_db: AtomicF32,
    /// `gain_db` as a linear gain. Read by the audio thread.
    gain: AtomicF32,
    muted: AtomicBool,
    meter: PeakMeter,
}

impl ChannelControl {
    /// Creates controls with a gain of 0 dB, not muted, and a meter at 0.
    /// The gain is limited to `range`.
    pub fn new(range: DbRange) -> Self {
        let db = range.clamp(0.0);
        Self {
            range,
            gain_db: AtomicF32::new(db),
            gain: AtomicF32::new(db_to_gain(db)),
            muted: AtomicBool::new(false),
            meter: PeakMeter::new(),
        }
    }

    /// Returns the allowed range of the gain in dB.
    pub fn range(&self) -> DbRange {
        self.range
    }

    /// Sets the gain in dB (UI side).
    ///
    /// The value is clamped to the range first. Both the clamped dB value and
    /// its linear gain are stored, so [`ChannelControl::gain_db`] returns the
    /// exact setting, including the lowest value (which is silence).
    ///
    /// `gain_db` and `gain` are written separately, so this assumes a single
    /// writer (the UI in Phase 1). Revisit this when more writers are added
    /// (for example, a SetVolume action of the Event Engine).
    pub fn set_gain_db(&self, db: f32) {
        let db = self.range.clamp(db);
        self.gain_db.store(db);
        self.gain.store(db_to_gain(db));
    }

    /// Returns the gain in dB, as set and clamped (UI and settings side).
    pub fn gain_db(&self) -> f32 {
        self.gain_db.load()
    }

    /// Returns the gain as a linear factor (audio thread side).
    ///
    /// Safe to call on the realtime path.
    pub fn gain(&self) -> f32 {
        self.gain.load()
    }

    /// Mutes or unmutes the channel (UI side).
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Returns whether the channel is muted.
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Returns the target for the mute gain node: 0.0 when muted, 1.0
    /// otherwise (audio thread side).
    ///
    /// Safe to call on the realtime path.
    pub fn mute_gain(&self) -> f32 {
        if self.is_muted() { 0.0 } else { 1.0 }
    }

    /// Returns the meter of this channel.
    pub fn meter(&self) -> &PeakMeter {
        &self.meter
    }
}

/// The controls of the Phase 1 mixer: MIC and MASTER.
///
/// Shared as an `Arc<MixerControl>` by the UI and the audio thread; it holds
/// only atomics, so no lock is needed.
///
/// The MASTER mute is not used in Phase 1 and stays off.
#[derive(Debug)]
pub struct MixerControl {
    /// MIC gain (-60 to +24 dB), mute, and meter.
    pub mic: ChannelControl,
    /// Master volume (-60 to +6 dB) and meter.
    pub master: ChannelControl,
}

impl MixerControl {
    /// Creates controls with every gain at 0 dB and nothing muted.
    pub fn new() -> Self {
        Self {
            mic: ChannelControl::new(MIC_GAIN_RANGE),
            master: ChannelControl::new(MASTER_VOLUME_RANGE),
        }
    }
}

impl Default for MixerControl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{AudioBuffer, AudioNode, GainNode, block_peak};

    #[test]
    fn initial_values() {
        let control = MixerControl::new();
        for channel in [&control.mic, &control.master] {
            assert_eq!(channel.gain_db(), 0.0);
            assert_eq!(channel.gain(), 1.0);
            assert!(!channel.is_muted());
            assert_eq!(channel.mute_gain(), 1.0);
            assert_eq!(channel.meter().take(), 0.0);
        }
    }

    #[test]
    fn gain_is_clamped_to_channel_range() {
        let control = MixerControl::new();

        control.mic.set_gain_db(30.0);
        assert_eq!(control.mic.gain_db(), 24.0);
        assert_eq!(control.mic.gain(), db_to_gain(24.0));

        control.master.set_gain_db(10.0);
        assert_eq!(control.master.gain_db(), 6.0);
        assert_eq!(control.master.gain(), db_to_gain(6.0));
    }

    #[test]
    fn lowest_gain_is_silence_but_reads_back_in_db() {
        let control = MixerControl::new();
        control.mic.set_gain_db(-60.0);
        assert_eq!(control.mic.gain(), 0.0);
        assert_eq!(control.mic.gain_db(), -60.0);

        control.mic.set_gain_db(-100.0);
        assert_eq!(control.mic.gain_db(), -60.0);
        assert!(control.mic.gain_db().is_finite());
    }

    #[test]
    fn mute_sets_mute_gain() {
        let control = MixerControl::new();
        control.mic.set_muted(true);
        assert!(control.mic.is_muted());
        assert_eq!(control.mic.mute_gain(), 0.0);
        control.mic.set_muted(false);
        assert_eq!(control.mic.mute_gain(), 1.0);
    }

    #[test]
    fn gain_node_follows_control_every_block() {
        let control = MixerControl::new();
        let mut node = GainNode::new(control.mic.gain());
        node.prepare(48_000);
        control.mic.set_gain_db(-6.0);

        let mut buffer = AudioBuffer::new(1, 64);
        let mut out = Vec::new();
        for _ in 0..10 {
            node.set_target(control.mic.gain());
            buffer.channel_mut(0).fill(1.0);
            node.process(&mut buffer);
            out.extend_from_slice(buffer.channel(0));
        }
        assert!(out[478] > control.mic.gain());
        assert_eq!(
            out[479],
            control.mic.gain(),
            "480th frame must be the target"
        );
    }

    #[test]
    fn mic_meter_is_after_gain_and_before_mute() {
        let control = MixerControl::new();
        control.mic.set_gain_db(-6.0);
        control.mic.set_muted(true);

        let mut gain = GainNode::new(control.mic.gain());
        let mut mute = GainNode::new(control.mic.mute_gain());
        gain.prepare(48_000);
        mute.prepare(48_000);

        let mut buffer = AudioBuffer::new(1, 64);
        for _ in 0..10 {
            buffer.channel_mut(0).fill(0.5);
            gain.set_target(control.mic.gain());
            gain.process(&mut buffer);
            control.mic.meter().record(block_peak(buffer.channel(0)));
            mute.set_target(control.mic.mute_gain());
            mute.process(&mut buffer);
        }

        // The meter shows the MIC level while muted; the output is silent.
        let level = control.mic.meter().take();
        assert!((level - 0.5 * db_to_gain(-6.0)).abs() < 1e-6, "{level}");
        assert!(buffer.channel(0).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn mixer_control_can_be_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MixerControl>();
    }
}
