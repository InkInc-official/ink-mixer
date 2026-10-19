//! What the three parts of the background music (BGM) player share
//! (ADR-0009): the sizes, the state in atomics, the control side, and the
//! worker side.
//!
//! ```text
//! worker thread                                              audio thread
//! BgmFeeder --write--> rtrb ring buffer (L,R interleaved) --> BgmPlayer
//!      ^                                                         |
//!      +-- gen / stopped / drained / eof / looping / playing  <--+-- BgmController
//!                                                                    (control thread)
//! ```
//!
//! The worker decodes the file and is the only writer of the ring buffer; the
//! audio thread is the only reader and only reads (no allocation, no
//! blocking). The control side sets a few atomics. There is no command queue
//! for the BGM.
//!
//! # Stop and switching to another song
//!
//! The ring buffer holds up to a second of the old song, which has to be
//! thrown away without the worker writing at the same time. Three
//! generation numbers do this (ADR-0009 §3):
//!
//! 1. The control side makes `gen` one larger ([`BgmController::request_flush`]).
//! 2. The worker sees it, stops writing, and writes `stopped = gen`
//!    ([`BgmFeeder::poll_flush`]).
//! 3. The audio thread fades out, plays silence until `stopped == gen`, then
//!    throws away everything in the ring buffer and writes `drained = gen`.
//! 4. The worker sees `drained == gen` and starts to write the new sound.
//!
//! The audio side of this is in `BgmPlayer`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rtrb::{Consumer, Producer, RingBuffer};

use super::GainNode;

/// How long the ring buffer is, in milliseconds. The worker keeps it full.
pub const BGM_RING_MS: u32 = 1000;

/// How much has to be in the ring buffer before the audio thread starts to
/// play (or to play again after running dry), in milliseconds. See
/// [`bgm_prime_frames`].
pub const BGM_PRIME_MS: u32 = 100;

/// How long the fades of the BGM take, in milliseconds: starting, pausing,
/// and the end of a song that ran dry. Also the amount of sound the audio
/// thread keeps in the ring buffer to be able to fade out.
pub const BGM_FADE_MS: u32 = 10;

/// The most frames the worker writes into the ring buffer at a time.
pub const BGM_WRITE_CHUNK_FRAMES: usize = 2048;

/// `ms` milliseconds in frames at `sample_rate`, rounded down.
fn ms_to_frames(sample_rate: u32, ms: u32) -> usize {
    (u64::from(sample_rate) * u64::from(ms) / 1000) as usize
}

/// The length of a fade, in frames ([`BGM_FADE_MS`], at least 1). This is
/// also the amount the audio thread must be able to read after it finds that
/// the ring buffer is running dry.
///
/// It is worked out in the same way as the ramp of a [`GainNode`] (rounded to
/// the nearest frame), because the fades of the BGM are made by one, and the
/// frames that are read must end when its ramp ends.
pub fn bgm_fade_frames(sample_rate: u32) -> usize {
    ((f64::from(sample_rate) * GainNode::RAMP.as_secs_f64()).round() as usize).max(1)
}

/// The amount in frames that has to be in the ring buffer before the audio
/// thread starts to play: [`BGM_PRIME_MS`], or one block plus one fade if
/// that is more (`max(BGM_PRIME_MS, max_frames + fade)`).
///
/// The audio thread only plays while at least one block plus one fade is in
/// the ring buffer, so a start with less would run dry at once.
pub fn bgm_prime_frames(sample_rate: u32, max_frames: usize) -> usize {
    ms_to_frames(sample_rate, BGM_PRIME_MS).max(max_frames + bgm_fade_frames(sample_rate))
}

/// The capacity of the ring buffer in frames: [`BGM_RING_MS`], or four blocks
/// of `max_frames` if that is more. It always holds
/// `bgm_prime_frames(..) + max_frames`.
pub fn bgm_ring_frames(sample_rate: u32, max_frames: usize) -> usize {
    ms_to_frames(sample_rate, BGM_RING_MS).max(max_frames * 4)
}

/// The values of the counters of a BGM player at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BgmStats {
    /// How many times the ring buffer ran dry while the song was playing
    /// (one for each interruption).
    pub underruns: u64,
    /// The frames of silence played during interruptions.
    pub underrun_frames: u64,
    /// How much is in the ring buffer now, in frames.
    pub fill_frames: u32,
    /// The least that was in the ring buffer since the last
    /// [`BgmController::take_min_fill_frames`] (or since the start).
    pub min_fill_frames: u32,
    /// How many times the worker went back to the start of the song
    /// (a loop). It counts since the channel was made.
    pub loops: u64,
    /// The packets of the file that could not be decoded and were skipped.
    /// It counts since the channel was made.
    pub skipped_packets: u64,
    /// The frames the audio thread has read since the song was started (the
    /// playing position, at the output rate).
    pub played_frames: u64,
    /// Whether a song that does not loop has been played to its end.
    pub finished: bool,
}

/// What [`BgmFeeder::poll_flush`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushPoll {
    /// Nothing was asked for. The worker may write.
    Idle,
    /// A flush was asked for, and the audio thread has not thrown the old
    /// sound away yet. The worker must not write.
    Waiting,
    /// The audio thread has thrown the old sound away. The worker starts a
    /// new song, or the same one from the start. It is returned once.
    Done,
}

/// The state that the three parts share. It only holds atomics (and the
/// numbers that never change), so no lock is needed.
#[derive(Debug)]
pub(super) struct BgmShared {
    pub(super) sample_rate: u32,
    pub(super) ring_frames: usize,
    /// Whether the song is to be played (set by the control side).
    pub(super) playing: AtomicBool,
    /// Whether the worker goes back to the start at the end of the song.
    pub(super) looping: AtomicBool,
    /// The generation: made larger by every flush (Stop, other song).
    pub(super) generation: AtomicU32,
    /// The generation for which the worker has stopped writing.
    pub(super) stopped: AtomicU32,
    /// The generation for which the audio thread has thrown the sound away.
    pub(super) drained: AtomicU32,
    /// 0, or `generation + 1` of the song whose last frames are all in the
    /// ring buffer. A value of an older generation is ignored.
    pub(super) eof: AtomicU64,
    /// The length of the song in frames at the output rate, or `u64::MAX`.
    pub(super) total_frames: AtomicU64,
    pub(super) finished: AtomicBool,
    pub(super) underruns: AtomicU64,
    pub(super) underrun_frames: AtomicU64,
    pub(super) fill_frames: AtomicU32,
    pub(super) min_fill_frames: AtomicU32,
    pub(super) loops: AtomicU64,
    pub(super) skipped_packets: AtomicU64,
    pub(super) played_frames: AtomicU64,
}

/// Where the audio side hands the reading end of the ring buffer back when
/// it is dropped, so that the control side frees it (see `BgmPlayer`).
///
/// The player moves the reading end in with `try_lock` (it never waits, and
/// moving a value allocates and frees nothing). Nothing else locks it while
/// the stream runs. A `Mutex` is used because a `Consumer` is `Send` but not
/// `Sync`.
pub(super) type ConsumerKeeper = Arc<Mutex<Option<Consumer<f32>>>>;

/// The control side of the BGM player: what the UI and `run` use.
///
/// It only sets and reads atomics, so it can be used from any thread.
#[derive(Debug)]
pub struct BgmController {
    shared: Arc<BgmShared>,
    /// Holds the reading end of the ring buffer after the player is dropped,
    /// so that the memory is freed on the control side.
    _keeper: ConsumerKeeper,
}

impl BgmController {
    /// Starts or continues playing (after a short wait for the ring buffer
    /// to fill, and a fade-in).
    pub fn play(&self) {
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    /// Pauses with a fade-out. What is in the ring buffer stays. The frames
    /// read for the fade (10 ms) are not heard again: after
    /// [`play`](Self::play), the song goes on after them.
    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    /// Asks to throw the sound in the ring buffer away: Stop, or the start
    /// of another song. It pauses first. The song goes back to its start (or
    /// the worker opens the other song) when the audio thread has fully
    /// faded out and thrown the sound away. Returns the new generation.
    ///
    /// The worker has to answer ([`BgmFeeder::poll_flush`]) for the audio
    /// thread to finish: if the worker is not running, a flush never ends.
    pub fn request_flush(&self) -> u32 {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared
            .generation
            .fetch_add(1, Ordering::Release)
            .wrapping_add(1)
    }

    /// Sets whether the song starts again when it ends. The worker reads it
    /// when it reaches the end, up to a second ahead of the playing
    /// position, so a change can take up to a second to be heard.
    pub fn set_looping(&self, on: bool) {
        self.shared.looping.store(on, Ordering::Relaxed);
    }

    /// Returns whether the song loops.
    pub fn is_looping(&self) -> bool {
        self.shared.looping.load(Ordering::Relaxed)
    }

    /// Returns whether the song is to be played (even if it is not heard
    /// yet because the ring buffer is filling).
    pub fn wants_play(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Returns whether the audio thread has thrown the sound away for the
    /// last [`request_flush`](Self::request_flush) (the worker may not have
    /// started the new song yet).
    pub fn is_flushed(&self) -> bool {
        self.shared.drained.load(Ordering::Acquire)
            == self.shared.generation.load(Ordering::Acquire)
    }

    /// Returns whether a song that does not loop has been played to its end.
    /// Stop ([`request_flush`](Self::request_flush)) and play again to hear
    /// it again.
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }

    /// Returns the counters.
    pub fn stats(&self) -> BgmStats {
        let fill = self.shared.fill_frames.load(Ordering::Relaxed);
        BgmStats {
            underruns: self.shared.underruns.load(Ordering::Relaxed),
            underrun_frames: self.shared.underrun_frames.load(Ordering::Relaxed),
            fill_frames: fill,
            min_fill_frames: min_or(self.shared.min_fill_frames.load(Ordering::Relaxed), fill),
            loops: self.shared.loops.load(Ordering::Relaxed),
            skipped_packets: self.shared.skipped_packets.load(Ordering::Relaxed),
            played_frames: self.shared.played_frames.load(Ordering::Relaxed),
            finished: self.shared.finished.load(Ordering::Relaxed),
        }
    }

    /// Returns the least the ring buffer held since the last call, and starts
    /// to measure again. This shows whether the second of buffer is enough:
    /// it should stay far above a block.
    pub fn take_min_fill_frames(&self) -> u32 {
        let fill = self.shared.fill_frames.load(Ordering::Relaxed);
        min_or(
            self.shared
                .min_fill_frames
                .swap(u32::MAX, Ordering::Relaxed),
            fill,
        )
    }

    /// Returns the playing position in frames at the output rate. While the
    /// song loops and its length is known, it is the position in the song.
    pub fn position_frames(&self) -> u64 {
        let played = self.shared.played_frames.load(Ordering::Relaxed);
        match self.total_frames() {
            Some(total) if total > 0 && self.is_looping() => played % total,
            _ => played,
        }
    }

    /// Returns the length of the song in frames at the output rate, if the
    /// file says it (an MP3 file may not).
    pub fn total_frames(&self) -> Option<u64> {
        match self.shared.total_frames.load(Ordering::Relaxed) {
            u64::MAX => None,
            total => Some(total),
        }
    }

    /// Returns the sample rate of the player.
    pub fn sample_rate(&self) -> u32 {
        self.shared.sample_rate
    }

    /// Returns the capacity of the ring buffer in frames.
    pub fn ring_capacity_frames(&self) -> usize {
        self.shared.ring_frames
    }
}

/// `u32::MAX` means "no block since the last reset": use `fallback`.
fn min_or(min: u32, fallback: u32) -> u32 {
    if min == u32::MAX { fallback } else { min }
}

/// The worker side of the BGM player: the only writer of the ring buffer.
///
/// It is for one worker thread. The worker decodes the file (that is
/// `ink-media`'s job) and writes the frames here; nothing here reads a file.
#[derive(Debug)]
pub struct BgmFeeder {
    producer: Producer<f32>,
    shared: Arc<BgmShared>,
    /// The last generation the worker has answered with `stopped`.
    answered: u32,
    /// Whether it has answered and `drained` has not followed yet.
    waiting: bool,
}

impl BgmFeeder {
    /// Returns how many frames fit into the ring buffer now.
    pub fn free_frames(&self) -> usize {
        self.producer.slots() / 2
    }

    /// Writes frames (left and right) into the ring buffer, as many as fit
    /// (and as the two slices have), and returns how many it wrote. The audio
    /// thread sees them all at once, whole frames only.
    pub fn write(&mut self, left: &[f32], right: &[f32]) -> usize {
        let frames = left.len().min(right.len()).min(self.free_frames());
        if frames == 0 {
            return 0;
        }
        let Ok(chunk) = self.producer.write_chunk_uninit(frames * 2) else {
            return 0;
        };
        let written = chunk.fill_from_iter(
            left[..frames]
                .iter()
                .zip(&right[..frames])
                .flat_map(|(&l, &r)| [l, r]),
        );
        written / 2
    }

    /// Checks whether a flush was asked for. Call it in every turn of the
    /// worker's loop, before writing.
    ///
    /// - [`FlushPoll::Idle`]: write as usual.
    /// - [`FlushPoll::Waiting`]: do not write. A new request is answered at
    ///   once, so the worker only has to call this again after a short sleep.
    /// - [`FlushPoll::Done`]: the ring buffer is empty. Open the other song
    ///   or go back to the start of this one, and call
    ///   [`begin_stream`](Self::begin_stream).
    pub fn poll_flush(&mut self) -> FlushPoll {
        let generation = self.shared.generation.load(Ordering::Acquire);
        if generation != self.answered {
            self.answered = generation;
            self.waiting = true;
            self.shared.stopped.store(generation, Ordering::Release);
            return FlushPoll::Waiting;
        }
        if self.waiting {
            if self.shared.drained.load(Ordering::Acquire) == generation {
                self.waiting = false;
                return FlushPoll::Done;
            }
            return FlushPoll::Waiting;
        }
        FlushPoll::Idle
    }

    /// Tells that a song starts (the first one, or after a flush): its length
    /// in frames at the output rate, if known.
    pub fn begin_stream(&mut self, total_frames: Option<u64>) {
        self.shared
            .total_frames
            .store(total_frames.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Tells that the last frames of the song are in the ring buffer. Call it
    /// after the last [`write`](Self::write), and not for a song that loops.
    pub fn mark_eof(&mut self) {
        self.shared
            .eof
            .store(u64::from(self.answered) + 1, Ordering::Release);
    }

    /// Returns whether the song starts again when it ends.
    pub fn is_looping(&self) -> bool {
        self.shared.looping.load(Ordering::Relaxed)
    }

    /// Counts a loop (the worker went back to the start of the song).
    pub fn count_loop(&mut self) {
        self.shared.loops.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts packets of the file that could not be decoded.
    pub fn add_skipped_packets(&mut self, count: u64) {
        self.shared
            .skipped_packets
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Returns the sample rate of the player.
    pub fn sample_rate(&self) -> u32 {
        self.shared.sample_rate
    }
}

/// The parts that `bgm_channel` (in `bgm_player.rs`) puts together.
pub(super) struct RingParts {
    pub(super) controller: BgmController,
    pub(super) feeder: BgmFeeder,
    /// The reading end of the ring buffer, for the player.
    pub(super) consumer: Consumer<f32>,
    pub(super) shared: Arc<BgmShared>,
    /// The same as in the controller.
    pub(super) keeper: ConsumerKeeper,
}

/// Makes the ring buffer and the shared state.
///
/// # Panics
///
/// Panics if `sample_rate` or `max_frames` is 0. Both are mistakes of the
/// caller.
pub(super) fn ring_parts(sample_rate: u32, max_frames: usize) -> RingParts {
    assert!(sample_rate > 0, "the sample rate must not be 0");
    assert!(max_frames > 0, "max_frames must not be 0");
    let ring_frames = bgm_ring_frames(sample_rate, max_frames);
    let (producer, consumer) = RingBuffer::new(ring_frames * 2);
    let shared = Arc::new(BgmShared {
        sample_rate,
        ring_frames,
        playing: AtomicBool::new(false),
        looping: AtomicBool::new(false),
        generation: AtomicU32::new(0),
        stopped: AtomicU32::new(0),
        drained: AtomicU32::new(0),
        eof: AtomicU64::new(0),
        total_frames: AtomicU64::new(u64::MAX),
        finished: AtomicBool::new(false),
        underruns: AtomicU64::new(0),
        underrun_frames: AtomicU64::new(0),
        fill_frames: AtomicU32::new(0),
        min_fill_frames: AtomicU32::new(u32::MAX),
        loops: AtomicU64::new(0),
        skipped_packets: AtomicU64::new(0),
        played_frames: AtomicU64::new(0),
    });
    let keeper: ConsumerKeeper = Arc::new(Mutex::new(None));
    RingParts {
        controller: BgmController {
            shared: Arc::clone(&shared),
            _keeper: Arc::clone(&keeper),
        },
        feeder: BgmFeeder {
            producer,
            shared: Arc::clone(&shared),
            answered: 0,
            waiting: false,
        },
        consumer,
        shared,
        keeper,
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    const RATE: u32 = 48_000;
    const MAX_FRAMES: usize = 1024;

    fn parts() -> RingParts {
        ring_parts(RATE, MAX_FRAMES)
    }

    fn frames_of(parts: &mut RingParts) -> Vec<(f32, f32)> {
        let mut out = Vec::new();
        while let (Ok(l), Ok(r)) = (parts.consumer.pop(), parts.consumer.pop()) {
            out.push((l, r));
        }
        out
    }

    #[test]
    fn the_constants_are_those_of_adr_0009() {
        assert_eq!(BGM_RING_MS, 1000);
        assert_eq!(BGM_PRIME_MS, 100);
        assert_eq!(BGM_FADE_MS, 10);
        assert_eq!(BGM_WRITE_CHUNK_FRAMES, 2048);
    }

    #[test]
    fn a_fade_is_10_ms_at_the_sample_rate_and_at_least_one_frame() {
        assert_eq!(bgm_fade_frames(48_000), 480);
        assert_eq!(bgm_fade_frames(44_100), 441);
        assert_eq!(bgm_fade_frames(96_000), 960);
        assert_eq!(bgm_fade_frames(1_000), 10);
        assert_eq!(bgm_fade_frames(1), 1);
    }

    #[test]
    fn a_fade_is_as_long_as_the_ramp_of_a_gain_node_at_any_rate() {
        use std::time::Duration;

        use crate::graph::{AudioBuffer, AudioNode};

        assert_eq!(
            GainNode::RAMP,
            Duration::from_millis(u64::from(BGM_FADE_MS))
        );
        for rate in [
            100, 1_000, 8_000, 11_025, 22_050, 22_051, 32_000, 44_100, 44_101, 47_999, 48_000,
            88_200, 96_000, 192_000,
        ] {
            let mut node = GainNode::new(1.0);
            node.prepare(rate);
            node.set_target(0.0);
            let mut buffer = AudioBuffer::new(1, 1);
            let mut frames = 0;
            loop {
                buffer.channel_mut(0)[0] = 1.0;
                node.process(&mut buffer);
                frames += 1;
                if buffer.channel(0)[0] == 0.0 || frames > 100_000 {
                    break;
                }
            }
            assert_eq!(frames, bgm_fade_frames(rate), "{rate} Hz");
        }
        // 22,050 Hz: 220.5 frames, which the ramp rounds up.
        assert_eq!(bgm_fade_frames(22_050), 221);
    }

    #[test]
    fn the_ring_buffer_holds_a_second_or_four_blocks() {
        assert_eq!(bgm_ring_frames(48_000, 1024), 48_000);
        assert_eq!(bgm_ring_frames(44_100, 1024), 44_100);
        assert_eq!(bgm_ring_frames(96_000, 4096), 96_000);
        // Four blocks are more than a second.
        assert_eq!(bgm_ring_frames(8_000, 4096), 16_384);
        // The memory: 48000 frames, two f32 each.
        assert_eq!(
            bgm_ring_frames(48_000, 1024) * 2 * size_of::<f32>(),
            384_000
        );
        assert_eq!(
            bgm_ring_frames(96_000, 1024) * 2 * size_of::<f32>(),
            768_000
        );
    }

    #[test]
    fn priming_is_100_ms_or_one_block_plus_one_fade() {
        // 100 ms is more than a block and a fade.
        assert_eq!(bgm_prime_frames(48_000, 1024), 4_800);
        assert_eq!(bgm_prime_frames(44_100, 1024), 4_410);
        assert_eq!(bgm_prime_frames(96_000, 4096), 9_600);
        // A block and a fade are more than 100 ms: the fade comes from the
        // sample rate, not from a fixed 480.
        assert_eq!(bgm_prime_frames(48_000, 8_192), 8_192 + 480);
        assert_eq!(bgm_prime_frames(44_100, 8_192), 8_192 + 441);
        assert_eq!(bgm_prime_frames(96_000, 16_384), 16_384 + 960);
        assert_eq!(bgm_prime_frames(8_000, 4_096), 4_096 + 80);
        // At 1000 Hz the numbers of the tests of the graph.
        assert_eq!(bgm_prime_frames(1_000, 8), 100);
    }

    #[test]
    fn priming_covers_a_block_and_a_fade_and_fits_the_ring_buffer() {
        let rates = [
            1_000, 8_000, 11_025, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 192_000,
        ];
        let blocks = [
            1, 2, 8, 64, 256, 480, 1024, 2048, 4096, 8192, 16_384, 100_000,
        ];
        for rate in rates {
            for max_frames in blocks {
                let prime = bgm_prime_frames(rate, max_frames);
                let fade = bgm_fade_frames(rate);
                let ring = bgm_ring_frames(rate, max_frames);
                assert!(prime >= max_frames + fade, "{rate} Hz, {max_frames}");
                assert!(
                    prime >= ms_to_frames(rate, BGM_PRIME_MS),
                    "{rate} Hz, {max_frames}"
                );
                // After priming there is room to write at least one more
                // block, and the ring buffer is never smaller than a block
                // and a fade (the reserve of the audio thread).
                assert!(ring >= prime + max_frames, "{rate} Hz, {max_frames}");
                assert!(ring >= max_frames + fade, "{rate} Hz, {max_frames}");
            }
        }
    }

    #[test]
    fn the_channel_starts_empty_and_stopped() {
        let parts = parts();
        let stats = parts.controller.stats();
        assert_eq!(stats, BgmStats::default());
        assert!(!parts.controller.wants_play());
        assert!(!parts.controller.is_looping());
        assert!(!parts.controller.is_finished());
        assert!(parts.controller.is_flushed());
        assert_eq!(parts.controller.total_frames(), None);
        assert_eq!(parts.controller.sample_rate(), RATE);
        assert_eq!(parts.controller.ring_capacity_frames(), 48_000);
        assert_eq!(parts.feeder.free_frames(), 48_000);
        assert_eq!(parts.feeder.sample_rate(), RATE);
    }

    #[test]
    #[should_panic(expected = "sample rate")]
    fn a_sample_rate_of_0_panics() {
        let _ = ring_parts(0, 1024);
    }

    #[test]
    #[should_panic(expected = "max_frames")]
    fn a_max_frames_of_0_panics() {
        let _ = ring_parts(48_000, 0);
    }

    #[test]
    fn frames_are_written_interleaved_and_counted() {
        let mut parts = parts();
        let written = parts.feeder.write(&[1.0, 2.0, 3.0], &[-1.0, -2.0, -3.0]);
        assert_eq!(written, 3);
        assert_eq!(parts.feeder.free_frames(), 48_000 - 3);
        assert_eq!(parts.consumer.slots(), 6);
        assert_eq!(
            frames_of(&mut parts),
            [(1.0, -1.0), (2.0, -2.0), (3.0, -3.0)]
        );
        assert_eq!(parts.feeder.free_frames(), 48_000);
    }

    #[test]
    fn write_takes_the_shorter_of_the_two_channels() {
        let mut parts = parts();
        assert_eq!(parts.feeder.write(&[1.0, 2.0, 3.0], &[-1.0]), 1);
        assert_eq!(parts.feeder.write(&[], &[]), 0);
        assert_eq!(frames_of(&mut parts), [(1.0, -1.0)]);
    }

    #[test]
    fn write_stops_when_the_ring_buffer_is_full() {
        let mut parts = ring_parts(1_000, 8);
        assert_eq!(parts.controller.ring_capacity_frames(), 1_000);
        let left = vec![0.5_f32; 1_500];
        let right = vec![-0.5_f32; 1_500];
        assert_eq!(parts.feeder.write(&left, &right), 1_000);
        assert_eq!(parts.feeder.free_frames(), 0);
        assert_eq!(parts.feeder.write(&left, &right), 0);

        // Room for 10 frames: only 10 are written.
        for _ in 0..20 {
            parts.consumer.pop().unwrap();
        }
        assert_eq!(parts.feeder.free_frames(), 10);
        assert_eq!(parts.feeder.write(&left, &right), 10);
        assert_eq!(parts.feeder.free_frames(), 0);
    }

    #[test]
    fn frames_stay_whole_and_in_order_across_the_wrap_of_the_ring() {
        let mut parts = ring_parts(1_000, 8);
        let mut next = 0_u32;
        let mut expected = 0_u32;
        // Many times the capacity, in odd sizes.
        for round in 0..400 {
            let size = 1 + (round * 37) % 300;
            let left: Vec<f32> = (0..size).map(|i| (next + i as u32) as f32).collect();
            let right: Vec<f32> = left.iter().map(|l| -l).collect();
            let written = parts.feeder.write(&left, &right);
            next += written as u32;
            for (l, r) in frames_of(&mut parts) {
                assert_eq!(l, expected as f32);
                assert_eq!(r, -(expected as f32));
                expected += 1;
            }
        }
        assert_eq!(expected, next);
        assert!(next > 10 * 1_000, "{next}");
    }

    #[test]
    fn the_worker_and_a_reader_on_two_threads_see_every_frame_once() {
        let mut parts = ring_parts(1_000, 8);
        let total = 200_000_u32;
        let mut feeder = parts.feeder;
        let writer = thread::spawn(move || {
            let mut next = 0_u32;
            while next < total {
                let size = (1 + next % 97).min(total - next) as usize;
                let left: Vec<f32> = (0..size).map(|i| (next + i as u32) as f32).collect();
                let right: Vec<f32> = left.iter().map(|l| -l).collect();
                let written = feeder.write(&left, &right);
                next += written as u32;
                if written == 0 {
                    thread::yield_now();
                }
            }
        });
        let mut expected = 0_u32;
        while expected < total {
            let Ok(l) = parts.consumer.pop() else {
                thread::yield_now();
                continue;
            };
            // Frames are committed whole: the right sample is there too.
            let r = parts.consumer.pop().expect("half a frame");
            assert_eq!((l, r), (expected as f32, -(expected as f32)));
            expected += 1;
        }
        writer.join().unwrap();
    }

    #[test]
    fn play_and_pause_set_what_is_wanted() {
        let parts = parts();
        parts.controller.play();
        assert!(parts.controller.wants_play());
        parts.controller.pause();
        assert!(!parts.controller.wants_play());
    }

    #[test]
    fn looping_is_seen_by_the_worker() {
        let parts = parts();
        assert!(!parts.feeder.is_looping());
        parts.controller.set_looping(true);
        assert!(parts.controller.is_looping());
        assert!(parts.feeder.is_looping());
        parts.controller.set_looping(false);
        assert!(!parts.feeder.is_looping());
    }

    #[test]
    fn a_flush_pauses_and_counts_generations() {
        let parts = parts();
        parts.controller.play();
        assert_eq!(parts.controller.request_flush(), 1);
        assert!(!parts.controller.wants_play());
        assert!(!parts.controller.is_flushed());
        assert_eq!(parts.controller.request_flush(), 2);
        assert_eq!(parts.controller.request_flush(), 3);

        // The audio thread throws the sound away for generation 3.
        parts.shared.drained.store(3, Ordering::Release);
        assert!(parts.controller.is_flushed());
    }

    #[test]
    fn the_worker_answers_a_flush_and_waits_for_the_audio_thread() {
        let mut parts = parts();
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Idle);
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Idle);

        parts.controller.request_flush();
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Waiting);
        assert_eq!(parts.shared.stopped.load(Ordering::Acquire), 1);
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Waiting);

        // The audio thread throws the sound away.
        parts.shared.drained.store(1, Ordering::Release);
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Done);
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Idle);
    }

    #[test]
    fn a_new_request_while_waiting_is_answered_at_once() {
        let mut parts = parts();
        parts.controller.request_flush();
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Waiting);
        assert_eq!(parts.shared.stopped.load(Ordering::Acquire), 1);

        parts.controller.request_flush();
        parts.controller.request_flush();
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Waiting);
        // Only the latest generation is answered.
        assert_eq!(parts.shared.stopped.load(Ordering::Acquire), 3);

        // The old generation being drained does not finish the wait.
        parts.shared.drained.store(1, Ordering::Release);
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Waiting);
        parts.shared.drained.store(3, Ordering::Release);
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Done);
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Idle);
    }

    #[test]
    fn the_end_of_a_song_is_marked_for_its_generation() {
        let mut parts = parts();
        assert_eq!(parts.shared.eof.load(Ordering::Acquire), 0);
        parts.feeder.mark_eof();
        // Generation 0 is stored as 1.
        assert_eq!(parts.shared.eof.load(Ordering::Acquire), 1);

        // After a flush the mark is of an older generation: the audio
        // thread, which compares it with `generation + 1`, ignores it.
        parts.controller.request_flush();
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Waiting);
        assert_ne!(parts.shared.eof.load(Ordering::Acquire), 2);
        parts.shared.drained.store(1, Ordering::Release);
        assert_eq!(parts.feeder.poll_flush(), FlushPoll::Done);
        parts.feeder.mark_eof();
        assert_eq!(parts.shared.eof.load(Ordering::Acquire), 2);
    }

    #[test]
    fn the_length_of_a_song_is_known_or_not() {
        let mut parts = parts();
        assert_eq!(parts.controller.total_frames(), None);
        parts.feeder.begin_stream(Some(480_000));
        assert_eq!(parts.controller.total_frames(), Some(480_000));
        parts.feeder.begin_stream(None);
        assert_eq!(parts.controller.total_frames(), None);
        parts.feeder.begin_stream(Some(0));
        assert_eq!(parts.controller.total_frames(), Some(0));
    }

    #[test]
    fn the_position_wraps_at_the_length_only_while_looping() {
        let mut parts = parts();
        parts.feeder.begin_stream(Some(1_000));
        parts.shared.played_frames.store(2_500, Ordering::Relaxed);
        assert_eq!(parts.controller.position_frames(), 2_500);

        parts.controller.set_looping(true);
        assert_eq!(parts.controller.position_frames(), 500);

        // An unknown or empty length never divides.
        parts.feeder.begin_stream(None);
        assert_eq!(parts.controller.position_frames(), 2_500);
        parts.feeder.begin_stream(Some(0));
        assert_eq!(parts.controller.position_frames(), 2_500);
    }

    #[test]
    fn the_worker_counts_loops_and_skipped_packets() {
        let mut parts = parts();
        parts.feeder.count_loop();
        parts.feeder.count_loop();
        parts.feeder.add_skipped_packets(3);
        parts.feeder.add_skipped_packets(2);
        let stats = parts.controller.stats();
        assert_eq!((stats.loops, stats.skipped_packets), (2, 5));
    }

    #[test]
    fn the_stats_show_what_the_audio_thread_wrote() {
        let parts = parts();
        let shared = &parts.shared;
        shared.underruns.store(2, Ordering::Relaxed);
        shared.underrun_frames.store(9_600, Ordering::Relaxed);
        shared.fill_frames.store(30_000, Ordering::Relaxed);
        shared.min_fill_frames.store(12_000, Ordering::Relaxed);
        shared.played_frames.store(77, Ordering::Relaxed);
        shared.finished.store(true, Ordering::Relaxed);
        assert_eq!(
            parts.controller.stats(),
            BgmStats {
                underruns: 2,
                underrun_frames: 9_600,
                fill_frames: 30_000,
                min_fill_frames: 12_000,
                loops: 0,
                skipped_packets: 0,
                played_frames: 77,
                finished: true,
            }
        );
        assert!(parts.controller.is_finished());
    }

    #[test]
    fn the_least_fill_is_read_and_measured_again() {
        let parts = parts();
        // Nothing measured yet: the current fill is shown.
        parts.shared.fill_frames.store(5_000, Ordering::Relaxed);
        assert_eq!(parts.controller.stats().min_fill_frames, 5_000);
        assert_eq!(parts.controller.take_min_fill_frames(), 5_000);

        parts.shared.min_fill_frames.store(1_234, Ordering::Relaxed);
        assert_eq!(parts.controller.stats().min_fill_frames, 1_234);
        assert_eq!(parts.controller.take_min_fill_frames(), 1_234);
        // It starts again: until the audio thread measures, the current
        // fill is shown.
        parts.shared.fill_frames.store(40_000, Ordering::Relaxed);
        assert_eq!(parts.controller.take_min_fill_frames(), 40_000);
    }

    #[test]
    fn the_parts_can_cross_threads() {
        fn assert_send<T: Send>() {}
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send::<BgmFeeder>();
        assert_send_sync::<BgmController>();
        assert_send_sync::<BgmShared>();
    }

    #[test]
    fn the_keeper_holds_the_reading_end_after_the_player_is_gone() {
        let parts = parts();
        let RingParts {
            controller,
            feeder,
            consumer,
            keeper,
            ..
        } = parts;
        // What the player does when it is dropped.
        *keeper.try_lock().unwrap() = Some(consumer);
        // The controller's copy of the keeper sees it, so the control side
        // is the one that frees the ring buffer last.
        drop(feeder);
        assert!(controller._keeper.lock().unwrap().is_some());
        assert_eq!(Arc::strong_count(&controller._keeper), 2);
    }
}
