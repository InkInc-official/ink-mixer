//! Sums several inputs into one stereo output.

use super::AudioBuffer;

/// Sums its inputs into a stereo (2-channel) output.
///
/// A mono input is added to both L and R as is, without lowering its level
/// (ADR-0007). A stereo input is added to L and R respectively. The sum is
/// not clipped or limited.
///
/// This is not an [`AudioNode`](super::AudioNode): it takes several inputs
/// instead of processing one buffer in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MixerNode;

impl MixerNode {
    /// Creates a mixer.
    pub fn new() -> Self {
        Self
    }

    /// Clears `output` and adds every input to it.
    ///
    /// The number of frames is set on `output` by the caller, and the mixer
    /// follows it. Every input must have the same number of frames as
    /// `output`, inputs must be mono or stereo, and `output` must be stereo;
    /// otherwise a debug build panics. A release build then mixes only the
    /// frames and channels that exist.
    ///
    /// Safe to call on the realtime path: it does not allocate or block.
    pub fn process(&mut self, inputs: &[&AudioBuffer], output: &mut AudioBuffer) {
        debug_assert_eq!(output.channels(), 2, "mixer output must be stereo");
        output.clear();
        if output.channels() < 2 {
            return;
        }
        let frames = output.frames();

        for input in inputs {
            debug_assert_eq!(input.frames(), frames, "input frames differ from output");
            debug_assert!(
                matches!(input.channels(), 1 | 2),
                "mixer input must be mono or stereo"
            );
            let n = input.frames().min(frames);
            match input.channels() {
                1 => {
                    let mono = &input.channel(0)[..n];
                    add(&mut output.channel_mut(0)[..n], mono);
                    add(&mut output.channel_mut(1)[..n], mono);
                }
                2 => {
                    add(&mut output.channel_mut(0)[..n], &input.channel(0)[..n]);
                    add(&mut output.channel_mut(1)[..n], &input.channel(1)[..n]);
                }
                _ => {}
            }
        }
    }
}

/// Adds `src` to `dst` sample by sample.
fn add(dst: &mut [f32], src: &[f32]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d += s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(channels: &[&[f32]]) -> AudioBuffer {
        let mut buffer = AudioBuffer::new(channels.len(), channels[0].len());
        for (i, samples) in channels.iter().enumerate() {
            buffer.channel_mut(i).copy_from_slice(samples);
        }
        buffer
    }

    #[test]
    fn mono_input_goes_to_both_channels_at_full_level() {
        let mic = buffer(&[&[0.5, -0.25, 1.0]]);
        let mut out = AudioBuffer::new(2, 3);
        MixerNode::new().process(&[&mic], &mut out);
        assert_eq!(out.channel(0), &[0.5, -0.25, 1.0]);
        assert_eq!(out.channel(1), &[0.5, -0.25, 1.0]);
    }

    #[test]
    fn stereo_inputs_are_summed_per_channel() {
        let a = buffer(&[&[0.1, 0.2], &[0.3, 0.4]]);
        let b = buffer(&[&[0.5, 0.5], &[-0.5, -0.5]]);
        let mut out = AudioBuffer::new(2, 2);
        MixerNode::new().process(&[&a, &b], &mut out);
        assert_eq!(out.channel(0), &[0.6, 0.7]);
        assert_eq!(out.channel(1), &[0.3 - 0.5, 0.4 - 0.5]);
    }

    #[test]
    fn mono_and_stereo_inputs_are_summed() {
        let mic = buffer(&[&[0.25, 0.25]]);
        let bgm = buffer(&[&[0.5, 0.0], &[0.0, 0.5]]);
        let mut out = AudioBuffer::new(2, 2);
        MixerNode::new().process(&[&mic, &bgm], &mut out);
        assert_eq!(out.channel(0), &[0.75, 0.25]);
        assert_eq!(out.channel(1), &[0.25, 0.75]);
    }

    #[test]
    fn no_inputs_gives_silence_for_output_frames() {
        let mut out = AudioBuffer::new(2, 8);
        out.channel_mut(0).fill(1.0);
        out.channel_mut(1).fill(1.0);
        out.set_frames(5);
        MixerNode::new().process(&[], &mut out);
        assert_eq!(out.frames(), 5);
        assert!(out.channel(0).iter().all(|&s| s == 0.0));
        assert!(out.channel(1).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn previous_output_is_cleared_before_mixing() {
        let mic = buffer(&[&[0.5, 0.5]]);
        let mut out = AudioBuffer::new(2, 2);
        out.channel_mut(0).fill(9.0);
        out.channel_mut(1).fill(9.0);
        MixerNode::new().process(&[&mic], &mut out);
        assert_eq!(out.channel(0), &[0.5, 0.5]);
        assert_eq!(out.channel(1), &[0.5, 0.5]);
    }

    #[test]
    fn sum_above_one_is_not_limited() {
        let a = buffer(&[&[0.8]]);
        let b = buffer(&[&[0.8]]);
        let mut out = AudioBuffer::new(2, 1);
        MixerNode::new().process(&[&a, &b], &mut out);
        assert_eq!(out.channel(0), &[1.6]);
        assert_eq!(out.channel(1), &[1.6]);
    }

    #[test]
    fn only_frames_in_use_are_written() {
        let mut mic = AudioBuffer::new(1, 8);
        mic.channel_mut(0).fill(0.5);
        mic.set_frames(3);
        let mut out = AudioBuffer::new(2, 8);
        out.channel_mut(0).fill(9.0);
        out.channel_mut(1).fill(9.0);
        out.set_frames(3);

        MixerNode::new().process(&[&mic], &mut out);

        out.set_frames(8);
        for ch in 0..2 {
            assert_eq!(&out.channel(ch)[..3], &[0.5, 0.5, 0.5]);
            assert!(out.channel(ch)[3..].iter().all(|&s| s == 9.0));
        }
    }

    #[test]
    #[should_panic(expected = "input frames differ from output")]
    #[cfg(debug_assertions)]
    fn mismatched_frames_panic_in_debug() {
        let mic = AudioBuffer::new(1, 4);
        let mut out = AudioBuffer::new(2, 8);
        MixerNode::new().process(&[&mic], &mut out);
    }

    #[test]
    #[should_panic(expected = "mixer output must be stereo")]
    #[cfg(debug_assertions)]
    fn non_stereo_output_panics_in_debug() {
        let mic = AudioBuffer::new(1, 4);
        let mut out = AudioBuffer::new(1, 4);
        MixerNode::new().process(&[&mic], &mut out);
    }
}
