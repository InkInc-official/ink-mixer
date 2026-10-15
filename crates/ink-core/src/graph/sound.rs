//! Decoded audio for BGM and sound effects (ADR-0009).

use std::time::Duration;

/// Decoded audio ready to be played: stereo, f32, with one buffer per
/// channel (the same layout as [`AudioBuffer`](super::AudioBuffer)).
///
/// Audio files are decoded and resampled to the output rate outside the
/// realtime path (the `ink-media` crate does this); this type only holds the
/// result. A sound effect is shared with the audio thread as an
/// `Arc<SoundData>`, which the audio thread hands back instead of dropping
/// (ADR-0009).
#[derive(Debug, Clone, PartialEq)]
pub struct SoundData {
    sample_rate: u32,
    left: Box<[f32]>,
    right: Box<[f32]>,
}

impl SoundData {
    /// Creates a sound from the left and right channels.
    ///
    /// # Panics
    ///
    /// Panics if the channels differ in length or `sample_rate` is 0. Both
    /// are mistakes of the caller.
    pub fn new(sample_rate: u32, left: Vec<f32>, right: Vec<f32>) -> Self {
        assert!(sample_rate > 0, "the sample rate must not be 0");
        assert_eq!(
            left.len(),
            right.len(),
            "the channels must have the same length"
        );
        Self {
            sample_rate,
            left: left.into_boxed_slice(),
            right: right.into_boxed_slice(),
        }
    }

    /// Returns the sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns the number of frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.left.len()
    }

    /// Returns whether the sound has no frames.
    pub fn is_empty(&self) -> bool {
        self.left.is_empty()
    }

    /// Returns the left channel.
    pub fn left(&self) -> &[f32] {
        &self.left
    }

    /// Returns the right channel.
    pub fn right(&self) -> &[f32] {
        &self.right
    }

    /// Returns channel `index` (0 is left, 1 is right).
    ///
    /// # Panics
    ///
    /// Panics if `index` is 2 or more.
    pub fn channel(&self, index: usize) -> &[f32] {
        match index {
            0 => &self.left,
            1 => &self.right,
            _ => panic!("a sound has 2 channels, not {}", index + 1),
        }
    }

    /// Returns the length of the sound.
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.frames() as f64 / f64::from(self.sample_rate))
    }

    /// Returns the memory the samples take, in bytes (to keep the total of
    /// the loaded sounds under a limit).
    pub fn memory_bytes(&self) -> usize {
        2 * self.frames() * size_of::<f32>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sound_reports_its_rate_length_and_channels() {
        let sound = SoundData::new(48_000, vec![0.1, 0.2, 0.3], vec![-0.1, -0.2, -0.3]);
        assert_eq!(sound.sample_rate(), 48_000);
        assert_eq!(sound.frames(), 3);
        assert!(!sound.is_empty());
        assert_eq!(sound.left(), [0.1, 0.2, 0.3]);
        assert_eq!(sound.right(), [-0.1, -0.2, -0.3]);
        assert_eq!(sound.channel(0), sound.left());
        assert_eq!(sound.channel(1), sound.right());
    }

    #[test]
    fn duration_follows_the_frames_and_the_rate() {
        let one_second = SoundData::new(48_000, vec![0.0; 48_000], vec![0.0; 48_000]);
        assert_eq!(one_second.duration(), Duration::from_secs(1));

        let half = SoundData::new(44_100, vec![0.0; 22_050], vec![0.0; 22_050]);
        assert_eq!(half.duration(), Duration::from_millis(500));
    }

    #[test]
    fn memory_counts_both_channels_of_f32() {
        let sound = SoundData::new(48_000, vec![0.0; 48_000], vec![0.0; 48_000]);
        assert_eq!(sound.memory_bytes(), 48_000 * 2 * 4);
    }

    #[test]
    fn an_empty_sound_is_allowed() {
        let sound = SoundData::new(48_000, Vec::new(), Vec::new());
        assert!(sound.is_empty());
        assert_eq!(sound.frames(), 0);
        assert_eq!(sound.duration(), Duration::ZERO);
        assert_eq!(sound.memory_bytes(), 0);
    }

    #[test]
    #[should_panic(expected = "the channels must have the same length")]
    fn channels_of_different_length_panic() {
        let _ = SoundData::new(48_000, vec![0.0; 3], vec![0.0; 2]);
    }

    #[test]
    #[should_panic(expected = "the sample rate must not be 0")]
    fn a_zero_sample_rate_panics() {
        let _ = SoundData::new(0, vec![0.0], vec![0.0]);
    }

    #[test]
    #[should_panic(expected = "a sound has 2 channels")]
    fn a_third_channel_panics() {
        let sound = SoundData::new(48_000, vec![0.0], vec![0.0]);
        let _ = sound.channel(2);
    }
}
