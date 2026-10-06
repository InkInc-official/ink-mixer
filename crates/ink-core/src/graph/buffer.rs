//! A block of planar f32 audio with a fixed capacity.

/// Planar f32 audio: one array per channel.
///
/// The memory is allocated once by [`AudioBuffer::new`]. Afterwards the
/// number of frames in use can change from block to block with
/// [`AudioBuffer::set_frames`], up to the capacity, without allocating. This
/// makes the buffer usable on the realtime path (ADR-0007).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioBuffer {
    channels: Vec<Vec<f32>>,
    frames: usize,
}

impl AudioBuffer {
    /// Creates a silent buffer with `channels` channels and room for
    /// `max_frames` frames. All frames are in use.
    ///
    /// This allocates: call it outside the realtime path, for example when a
    /// stream is opened.
    pub fn new(channels: usize, max_frames: usize) -> Self {
        Self {
            channels: vec![vec![0.0; max_frames]; channels],
            frames: max_frames,
        }
    }

    /// Returns the number of channels.
    pub fn channels(&self) -> usize {
        self.channels.len()
    }

    /// Returns the capacity in frames.
    pub fn max_frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    /// Returns the number of frames in use.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Sets the number of frames in use.
    ///
    /// A value larger than the capacity is clamped to the capacity; the
    /// buffer never grows. Callers that receive a larger block must split it.
    pub fn set_frames(&mut self, frames: usize) {
        self.frames = frames.min(self.max_frames());
    }

    /// Returns the frames in use of channel `index`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is not less than [`AudioBuffer::channels`].
    pub fn channel(&self, index: usize) -> &[f32] {
        &self.channels[index][..self.frames]
    }

    /// Returns the frames in use of channel `index`, mutably.
    ///
    /// # Panics
    ///
    /// Panics if `index` is not less than [`AudioBuffer::channels`].
    pub fn channel_mut(&mut self, index: usize) -> &mut [f32] {
        &mut self.channels[index][..self.frames]
    }

    /// Sets the frames in use of every channel to silence.
    pub fn clear(&mut self) {
        let frames = self.frames;
        for channel in &mut self.channels {
            channel[..frames].fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_buffer_is_silent_with_given_size() {
        let buffer = AudioBuffer::new(2, 64);
        assert_eq!(buffer.channels(), 2);
        assert_eq!(buffer.max_frames(), 64);
        assert_eq!(buffer.frames(), 64);
        for i in 0..2 {
            assert!(buffer.channel(i).iter().all(|&s| s == 0.0));
        }
    }

    #[test]
    fn set_frames_is_clamped_to_capacity() {
        let mut buffer = AudioBuffer::new(1, 64);
        buffer.set_frames(16);
        assert_eq!(buffer.frames(), 16);
        assert_eq!(buffer.channel(0).len(), 16);

        buffer.set_frames(1000);
        assert_eq!(buffer.frames(), 64);
        assert_eq!(buffer.max_frames(), 64);
    }

    #[test]
    fn clear_silences_only_frames_in_use() {
        let mut buffer = AudioBuffer::new(2, 8);
        for i in 0..2 {
            buffer.channel_mut(i).fill(1.0);
        }
        buffer.set_frames(4);
        buffer.clear();

        buffer.set_frames(8);
        for i in 0..2 {
            assert_eq!(buffer.channel(i), &[0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
        }
    }
}
