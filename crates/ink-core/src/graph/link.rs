//! The link from the input stream to the output stream (ADR-0007).
//!
//! The input callback writes the MIC channel into a lock-free single-producer
//! single-consumer ring buffer (`rtrb`); the output callback reads it. The two
//! callbacks run on different threads, at different times, and possibly on
//! devices with different clocks. The reader absorbs this:
//!
//! - **Priming:** after starting, and after running dry, it outputs silence
//!   until the target amount is buffered.
//! - **Underrun:** when there is not enough data, the rest of the block is
//!   silent, the underrun is counted, and priming starts again.
//! - **Drift:** when more than "target + margin" is buffered (the input clock
//!   is faster), the oldest data is dropped back to the target, and an
//!   overrun is counted.
//! - **Full:** when the ring buffer is full, the writer drops the new data
//!   and counts an overrun. This is a last resort; the reader normally keeps
//!   the fill far below the capacity.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};

use super::{AudioBuffer, AudioSource};

/// The smallest target amount (and margin) to buffer.
pub const MIN_TARGET: Duration = Duration::from_millis(20);

/// The ring buffer holds at least this long.
pub const MIN_CAPACITY: Duration = Duration::from_millis(200);

/// Returns the amount, in frames, the reader buffers before playing. The same
/// amount is used as the margin above it before old data is dropped.
///
/// It is the largest of [`MIN_TARGET`], the largest input block, and the
/// largest output block seen so far (ADR-0007), so that one block arriving
/// or leaving never counts as an underrun or a drift by itself.
///
/// The input and output callbacks are not aligned: the input may not have
/// arrived yet when the output reads. If underruns keep occurring on real
/// devices, using "input block + output block" instead is a possible change.
pub fn target_frames(sample_rate: u32, in_block: u32, out_block: u32) -> u32 {
    let min = (f64::from(sample_rate) * MIN_TARGET.as_secs_f64()).round() as u32;
    min.max(in_block).max(out_block)
}

/// Returns the capacity of the ring buffer in frames: [`MIN_CAPACITY`], or
/// four blocks of `max_block_frames` if that is more.
///
/// With large callbacks, "target + margin + one block" may not fit in
/// [`MIN_CAPACITY`]; four blocks always hold it, because the target and the
/// margin are each at most one block (or [`MIN_TARGET`]).
pub fn capacity_frames(sample_rate: u32, max_block_frames: usize) -> usize {
    let min = (f64::from(sample_rate) * MIN_CAPACITY.as_secs_f64()).round() as usize;
    min.max(max_block_frames * 4)
}

/// Statistics of a link, written by the audio threads and read by the
/// control thread. All counters use `Ordering::Relaxed`: each value stands on
/// its own.
#[derive(Debug, Default)]
pub struct LinkStats {
    underruns: AtomicU64,
    overruns: AtomicU64,
    fill_sum: AtomicU64,
    fill_count: AtomicU64,
    fill: AtomicU32,
    target: AtomicU32,
    in_block_max: AtomicU32,
    out_block_max: AtomicU32,
}

impl LinkStats {
    /// Returns how many times the reader ran dry after priming.
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    /// Returns how many times data was dropped: old data because of drift,
    /// or new data because the ring buffer was full.
    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }

    /// Returns the frames buffered when the reader last ran.
    pub fn fill_frames(&self) -> u32 {
        self.fill.load(Ordering::Relaxed)
    }

    /// Returns the average frames buffered when the reader ran, or 0 before
    /// it has run.
    pub fn average_fill_frames(&self) -> f64 {
        let count = self.fill_count.load(Ordering::Relaxed);
        if count == 0 {
            0.0
        } else {
            self.fill_sum.load(Ordering::Relaxed) as f64 / count as f64
        }
    }

    /// Returns the current target amount in frames (see [`target_frames`]).
    pub fn target_frames(&self) -> u32 {
        self.target.load(Ordering::Relaxed)
    }

    /// Returns the largest input block seen, in frames.
    pub fn in_block_max(&self) -> u32 {
        self.in_block_max.load(Ordering::Relaxed)
    }

    /// Returns the largest output block seen, in frames.
    pub fn out_block_max(&self) -> u32 {
        self.out_block_max.load(Ordering::Relaxed)
    }
}

/// Creates a link for `sample_rate`, sized for blocks of up to
/// `max_block_frames` (see [`capacity_frames`]).
///
/// This allocates: call it outside the realtime path, before the streams
/// start. Give the writer to the input callback and the reader to the output
/// callback.
pub fn audio_link(
    sample_rate: u32,
    max_block_frames: usize,
) -> (LinkWriter, LinkReader, Arc<LinkStats>) {
    let (producer, consumer) = RingBuffer::new(capacity_frames(sample_rate, max_block_frames));
    let stats = Arc::new(LinkStats::default());
    let writer = LinkWriter {
        producer,
        stats: Arc::clone(&stats),
    };
    let reader = LinkReader {
        consumer,
        stats: Arc::clone(&stats),
        sample_rate,
        priming: true,
    };
    (writer, reader, stats)
}

/// The input side of a link.
pub struct LinkWriter {
    producer: Producer<f32>,
    stats: Arc<LinkStats>,
}

impl LinkWriter {
    /// Writes channel `channel` of the interleaved `samples` with `channels`
    /// channels. What does not fit is dropped and counted as an overrun.
    ///
    /// Safe to call on the realtime path: it does not allocate or block.
    pub fn push_channel(&mut self, samples: &[f32], channels: usize, channel: usize) {
        if channels == 0 || channel >= channels {
            return;
        }
        let frames = samples.len() / channels;
        self.stats
            .in_block_max
            .fetch_max(u32::try_from(frames).unwrap_or(u32::MAX), Ordering::Relaxed);

        let n = frames.min(self.producer.slots());
        if n > 0
            && let Ok(chunk) = self.producer.write_chunk_uninit(n)
        {
            chunk.fill_from_iter(samples.iter().skip(channel).step_by(channels).copied());
        }
        if n < frames {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The output side of a link.
pub struct LinkReader {
    consumer: Consumer<f32>,
    stats: Arc<LinkStats>,
    sample_rate: u32,
    /// Outputting silence until the target amount is buffered.
    priming: bool,
}

impl LinkReader {
    /// Fills `out` (mono) with buffered samples, handling priming,
    /// underruns, and drift (see [`audio_link`] and the module docs).
    ///
    /// Safe to call on the realtime path: it does not allocate or block.
    pub fn pull(&mut self, out: &mut [f32]) {
        let frames = out.len();
        let stats = &self.stats;
        stats
            .out_block_max
            .fetch_max(u32::try_from(frames).unwrap_or(u32::MAX), Ordering::Relaxed);
        let target = target_frames(
            self.sample_rate,
            stats.in_block_max(),
            stats.out_block_max(),
        );
        stats.target.store(target, Ordering::Relaxed);
        let target = target as usize;

        // Drift: too much buffered. Drop the oldest data back to the target.
        let mut available = self.consumer.slots();
        if available > target * 2 {
            let drop = available - target;
            if let Ok(chunk) = self.consumer.read_chunk(drop) {
                chunk.commit_all();
                available -= drop;
                stats.overruns.fetch_add(1, Ordering::Relaxed);
            }
        }

        stats
            .fill_sum
            .fetch_add(available as u64, Ordering::Relaxed);
        stats.fill_count.fetch_add(1, Ordering::Relaxed);
        stats.fill.store(
            u32::try_from(available).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );

        if self.priming {
            if available < target {
                out.fill(0.0);
                return;
            }
            self.priming = false;
        }

        if available >= frames {
            // Cannot fail: `frames` samples are available.
            let _ = self.consumer.pop_entire_slice(out);
        } else {
            // Underrun: play what there is, then silence, and prime again.
            let (head, tail) = out.split_at_mut(available);
            let _ = self.consumer.pop_entire_slice(head);
            tail.fill(0.0);
            stats.underruns.fetch_add(1, Ordering::Relaxed);
            self.priming = true;
        }
    }

    /// Returns the capacity of the ring buffer in frames.
    pub fn capacity(&self) -> usize {
        self.consumer.buffer().capacity()
    }
}

/// The MIC as an [`AudioSource`]: the output side of a link.
pub struct MicrophoneSource {
    reader: LinkReader,
}

impl MicrophoneSource {
    /// Wraps the reader of a link.
    pub fn new(reader: LinkReader) -> Self {
        Self { reader }
    }
}

impl AudioSource for MicrophoneSource {
    /// The sample rate is fixed when the link is created; nothing to do.
    fn prepare(&mut self, _sample_rate: u32) {}

    /// Fills channel 0 from the link and copies it to any other channels.
    fn render(&mut self, out: &mut AudioBuffer) {
        if out.channels() == 0 {
            return;
        }
        self.reader.pull(out.channel_mut(0));
        for ch in 1..out.channels() {
            let frames = out.frames();
            for i in 0..frames {
                let sample = out.channel(0)[i];
                out.channel_mut(ch)[i] = sample;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1000 Hz makes the numbers easy: the minimum target is 20 frames and
    /// the minimum capacity 200 frames.
    const RATE: u32 = 1000;

    fn link() -> (LinkWriter, LinkReader, Arc<LinkStats>) {
        audio_link(RATE, 8)
    }

    /// Writes `values` as mono in blocks of `block` frames.
    fn push(writer: &mut LinkWriter, values: impl IntoIterator<Item = f32>, block: usize) {
        let values: Vec<f32> = values.into_iter().collect();
        for chunk in values.chunks(block) {
            writer.push_channel(chunk, 1, 0);
        }
    }

    fn pull(reader: &mut LinkReader, frames: usize) -> Vec<f32> {
        let mut out = vec![-1.0; frames];
        reader.pull(&mut out);
        out
    }

    fn ramp(from: u32, to: u32) -> impl Iterator<Item = f32> {
        (from..=to).map(|i| i as f32)
    }

    #[test]
    fn target_is_the_largest_of_minimum_and_blocks() {
        assert_eq!(target_frames(RATE, 8, 4), 20);
        assert_eq!(target_frames(RATE, 30, 4), 30);
        assert_eq!(target_frames(RATE, 8, 40), 40);
        assert_eq!(target_frames(48_000, 1024, 1024), 1024);
        assert_eq!(target_frames(48_000, 256, 512), 960);
    }

    #[test]
    fn capacity_is_at_least_four_blocks() {
        assert_eq!(capacity_frames(48_000, 256), 9600);
        assert_eq!(capacity_frames(48_000, 4096), 16_384);
        let (_, reader, _) = audio_link(48_000, 4096);
        assert_eq!(reader.capacity(), 16_384);
    }

    #[test]
    fn priming_outputs_silence_without_counting_underruns() {
        let (mut writer, mut reader, stats) = link();
        assert_eq!(pull(&mut reader, 5), vec![0.0; 5]);

        push(&mut writer, ramp(1, 15), 5);
        assert_eq!(pull(&mut reader, 5), vec![0.0; 5], "15 < target 20");

        push(&mut writer, ramp(16, 25), 5);
        assert_eq!(pull(&mut reader, 5), vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(stats.underruns(), 0);
        assert_eq!(stats.overruns(), 0);
    }

    #[test]
    fn only_the_requested_channel_is_linked() {
        let (mut writer, mut reader, _) = link();
        // Three channels: channel 0 counts up, the others are 99.
        let interleaved: Vec<f32> = (1..=25).flat_map(|i| [i as f32, 99.0, 99.0]).collect();
        for chunk in interleaved.chunks(15) {
            writer.push_channel(chunk, 3, 0);
        }
        assert_eq!(pull(&mut reader, 4), vec![1.0, 2.0, 3.0, 4.0]);

        let (mut writer, mut reader, _) = link();
        let interleaved: Vec<f32> = (1..=25).flat_map(|i| [99.0, i as f32]).collect();
        for chunk in interleaved.chunks(10) {
            writer.push_channel(chunk, 2, 1);
        }
        assert_eq!(pull(&mut reader, 3), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn underrun_plays_the_rest_as_silence_and_primes_again() {
        let (mut writer, mut reader, stats) = link();
        push(&mut writer, ramp(1, 20), 5);
        assert_eq!(pull(&mut reader, 8), ramp(1, 8).collect::<Vec<_>>());
        assert_eq!(pull(&mut reader, 8), ramp(9, 16).collect::<Vec<_>>());

        // Four frames left for an 8-frame block.
        assert_eq!(
            pull(&mut reader, 8),
            vec![17.0, 18.0, 19.0, 20.0, 0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(stats.underruns(), 1);

        // Priming again: silent until 20 frames are buffered.
        push(&mut writer, ramp(21, 30), 5);
        assert_eq!(pull(&mut reader, 8), vec![0.0; 8]);
        push(&mut writer, ramp(31, 40), 5);
        assert_eq!(pull(&mut reader, 8), ramp(21, 28).collect::<Vec<_>>());
        assert_eq!(stats.underruns(), 1, "priming is not an underrun");
    }

    #[test]
    fn drift_drops_the_oldest_data_back_to_the_target() {
        let (mut writer, mut reader, stats) = link();
        push(&mut writer, ramp(1, 25), 5);
        assert_eq!(pull(&mut reader, 5), ramp(1, 5).collect::<Vec<_>>());

        // 20 buffered + 25 more = 45 > target 20 + margin 20.
        push(&mut writer, ramp(26, 50), 5);
        assert_eq!(pull(&mut reader, 5), ramp(31, 35).collect::<Vec<_>>());
        assert_eq!(stats.overruns(), 1);
        assert_eq!(stats.fill_frames(), 20);
        assert_eq!(stats.underruns(), 0);
    }

    #[test]
    fn full_ring_buffer_drops_new_data() {
        let (mut writer, mut reader, stats) = link();
        assert_eq!(reader.capacity(), 200);
        push(&mut writer, ramp(1, 250), 10);
        // Calls 21 to 25 found the buffer full.
        assert_eq!(stats.overruns(), 5);

        // The drift check then drops all but the newest kept 20 frames.
        assert_eq!(pull(&mut reader, 4), ramp(181, 184).collect::<Vec<_>>());
    }

    #[test]
    fn target_follows_the_largest_blocks() {
        let (mut writer, mut reader, stats) = link();
        push(&mut writer, ramp(1, 30), 30);
        // One 30-frame input block raises the target from 20 to 30.
        assert_eq!(pull(&mut reader, 4), vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(stats.in_block_max(), 30);
        assert_eq!(stats.target_frames(), 30);

        let (_, mut reader, stats) = link();
        pull(&mut reader, 40);
        assert_eq!(stats.out_block_max(), 40);
        assert_eq!(stats.target_frames(), 40);
    }

    #[test]
    fn average_fill_is_tracked() {
        let (mut writer, mut reader, stats) = link();
        assert_eq!(stats.average_fill_frames(), 0.0);
        push(&mut writer, ramp(1, 30), 5);
        pull(&mut reader, 10); // fill 30
        pull(&mut reader, 10); // fill 20
        assert_eq!(stats.average_fill_frames(), 25.0);
        assert_eq!(stats.fill_frames(), 20);
    }

    #[test]
    fn microphone_source_fills_every_channel() {
        let (mut writer, reader, _) = link();
        push(&mut writer, ramp(1, 25), 5);
        let mut source = MicrophoneSource::new(reader);
        source.prepare(RATE);

        let mut out = AudioBuffer::new(2, 3);
        source.render(&mut out);
        assert_eq!(out.channel(0), &[1.0, 2.0, 3.0]);
        assert_eq!(out.channel(1), &[1.0, 2.0, 3.0]);
    }
}
