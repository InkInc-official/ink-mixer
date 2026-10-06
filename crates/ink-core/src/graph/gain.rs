//! A gain stage with smoothing.

use std::time::Duration;

use super::{AudioBuffer, AudioNode};

/// Multiplies every channel by a gain factor.
///
/// When the target changes, the gain moves to it in a straight line over
/// [`GainNode::RAMP`] instead of jumping, so that moving a slider does not
/// cause zipper noise and muting (a target of 0) does not click (ADR-0007).
///
/// The gain is a linear factor (1.0 = unchanged). Converting from dB is the
/// caller's job.
#[derive(Debug, Clone, PartialEq)]
pub struct GainNode {
    /// The gain applied to the last processed frame.
    current: f32,
    /// The gain to reach.
    target: f32,
    /// The change per frame while ramping.
    step: f32,
    /// Frames left until `target` is reached; 0 when not ramping.
    remaining: u32,
    /// Ramp length in frames; 0 until [`AudioNode::prepare`] is called.
    ramp_frames: u32,
}

impl GainNode {
    /// How long a change of the target takes to complete.
    pub const RAMP: Duration = Duration::from_millis(10);

    /// Creates a node whose gain starts at `initial` without ramping.
    pub fn new(initial: f32) -> Self {
        Self {
            current: initial,
            target: initial,
            step: 0.0,
            remaining: 0,
            ramp_frames: 0,
        }
    }

    /// Sets the gain to move to.
    ///
    /// Calling it again with the current target does nothing, so it can be
    /// called every block without restarting the ramp. A non-finite value is
    /// ignored. Before [`AudioNode::prepare`] the gain switches to the target
    /// at once.
    ///
    /// Safe to call on the realtime path: it does not allocate or block.
    pub fn set_target(&mut self, target: f32) {
        if target == self.target || !target.is_finite() {
            return;
        }
        self.target = target;
        if self.ramp_frames == 0 {
            self.current = target;
            self.step = 0.0;
            self.remaining = 0;
        } else {
            self.step = (target - self.current) / self.ramp_frames as f32;
            self.remaining = self.ramp_frames;
        }
    }

    /// Returns the gain applied to the last processed frame.
    pub fn current(&self) -> f32 {
        self.current
    }

    /// Returns the gain being moved to.
    pub fn target(&self) -> f32 {
        self.target
    }

    /// Returns the gain for frame `index` (0-based) of the current ramp.
    /// The last frame of the ramp uses the target as is, so no rounding
    /// error is left behind.
    fn ramp_gain(&self, index: u32) -> f32 {
        if index + 1 >= self.remaining {
            self.target
        } else {
            self.current + self.step * (index + 1) as f32
        }
    }
}

impl AudioNode for GainNode {
    /// Computes the ramp length for `sample_rate` (480 frames at 48 kHz).
    /// Any ramp in progress is finished at once.
    fn prepare(&mut self, sample_rate: u32) {
        self.ramp_frames = (f64::from(sample_rate) * Self::RAMP.as_secs_f64()).round() as u32;
        self.current = self.target;
        self.step = 0.0;
        self.remaining = 0;
    }

    fn process(&mut self, buffer: &mut AudioBuffer) {
        let frames = buffer.frames();
        // Frames of this block that are still on the ramp.
        let ramp = (self.remaining as usize).min(frames);

        for ch in 0..buffer.channels() {
            let samples = buffer.channel_mut(ch);
            for (i, sample) in samples[..ramp].iter_mut().enumerate() {
                *sample *= self.ramp_gain(i as u32);
            }
            if ramp < frames {
                let gain = self.target;
                for sample in &mut samples[ramp..] {
                    *sample *= gain;
                }
            }
        }

        if ramp > 0 {
            self.current = self.ramp_gain(ramp as u32 - 1);
            self.remaining -= ramp as u32;
        } else if frames > 0 {
            self.current = self.target;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Runs `node` over `frames` frames of 1.0 in blocks of `block` frames
    /// and returns the output, which is the gain of each frame.
    fn gains(node: &mut GainNode, frames: usize, block: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames);
        let mut buffer = AudioBuffer::new(1, block);
        while out.len() < frames {
            buffer.set_frames((frames - out.len()).min(block));
            buffer.channel_mut(0).fill(1.0);
            node.process(&mut buffer);
            out.extend_from_slice(buffer.channel(0));
        }
        out
    }

    fn prepared(initial: f32) -> GainNode {
        let mut node = GainNode::new(initial);
        node.prepare(RATE);
        node
    }

    #[test]
    fn unity_gain_leaves_signal_unchanged() {
        let mut node = prepared(1.0);
        let mut buffer = AudioBuffer::new(1, 4);
        buffer
            .channel_mut(0)
            .copy_from_slice(&[0.1, -0.5, 0.9, -1.0]);
        node.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.1, -0.5, 0.9, -1.0]);
    }

    #[test]
    fn initial_gain_applies_without_ramp() {
        let mut node = prepared(0.5);
        assert!(gains(&mut node, 16, 16).iter().all(|&g| g == 0.5));
    }

    #[test]
    fn ramp_is_linear_and_reaches_target_exactly() {
        let mut node = prepared(1.0);
        node.set_target(0.5);
        let g = gains(&mut node, 600, 600);

        let step = -0.5 / 480.0;
        for (i, &gain) in g[..479].iter().enumerate() {
            let expected = 1.0 + step * (i + 1) as f32;
            assert!((gain - expected).abs() < 1e-6, "frame {i}: {gain}");
        }
        assert_eq!(g[479], 0.5, "480th frame must be the target");
        assert!(g[480..].iter().all(|&gain| gain == 0.5));
        assert_eq!(node.current(), 0.5);
    }

    #[test]
    fn ramp_length_follows_sample_rate() {
        let mut node = GainNode::new(1.0);
        node.prepare(44_100);
        node.set_target(0.0);
        let g = gains(&mut node, 500, 500);
        assert!(g[439] > 0.0);
        assert_eq!(g[440], 0.0, "441st frame must be the target");
    }

    #[test]
    fn block_size_does_not_change_result() {
        let mut whole = prepared(0.2);
        whole.set_target(0.8);
        let mut split = whole.clone();

        let a = gains(&mut whole, 600, 600);
        let b = gains(&mut split, 600, 64);
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert!((x - y).abs() < 1e-6, "frame {i}: {x} vs {y}");
        }
        assert_eq!(b[479], 0.8);
    }

    #[test]
    fn same_target_every_block_does_not_restart_ramp() {
        let mut node = prepared(1.0);
        node.set_target(0.0);
        let mut g = Vec::new();
        for _ in 0..10 {
            node.set_target(0.0);
            g.extend(gains(&mut node, 64, 64));
        }
        assert!(g[478] > 0.0);
        assert_eq!(g[479], 0.0, "480th frame must be the target");
    }

    #[test]
    fn new_target_mid_ramp_starts_from_current_gain() {
        let mut node = prepared(0.0);
        node.set_target(1.0);
        let first = gains(&mut node, 240, 240);
        let before = *first.last().unwrap();
        assert!((before - 0.5).abs() < 1e-6);

        node.set_target(0.0);
        let second = gains(&mut node, 480, 480);
        // The first frame continues from where the ramp was; no jump.
        assert!((second[0] - before).abs() <= 0.5 / 480.0 + 1e-6);
        assert_eq!(second[479], 0.0);
    }

    #[test]
    fn mute_ramps_to_zero_without_click() {
        let mut node = prepared(1.0);
        node.set_target(0.0);
        let g = gains(&mut node, 600, 128);

        let mut previous = 1.0;
        for (i, &gain) in g.iter().enumerate() {
            assert!(
                (previous - gain).abs() <= 1.0 / 480.0 + 1e-6,
                "frame {i}: step from {previous} to {gain} is too large"
            );
            previous = gain;
        }
        assert_eq!(*g.last().unwrap(), 0.0);
    }

    #[test]
    fn same_gain_on_every_channel() {
        let mut node = prepared(1.0);
        node.set_target(0.25);
        let mut buffer = AudioBuffer::new(2, 600);
        buffer.channel_mut(0).fill(1.0);
        buffer.channel_mut(1).fill(1.0);
        node.process(&mut buffer);
        assert_eq!(buffer.channel(0), buffer.channel(1));
        assert_eq!(buffer.channel(1)[599], 0.25);
    }

    #[test]
    fn before_prepare_target_applies_at_once() {
        let mut node = GainNode::new(1.0);
        node.set_target(0.5);
        let g = gains(&mut node, 32, 32);
        assert!(g.iter().all(|&gain| gain == 0.5));
        assert!(g.iter().all(|gain| !gain.is_nan()));
    }

    #[test]
    fn non_finite_target_is_ignored() {
        let mut node = prepared(0.5);
        node.set_target(f32::NAN);
        node.set_target(f32::INFINITY);
        assert_eq!(node.target(), 0.5);
        assert!(gains(&mut node, 16, 16).iter().all(|&g| g == 0.5));
    }
}
