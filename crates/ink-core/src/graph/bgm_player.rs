//! The audio side of the background music (BGM) player (ADR-0009).
//!
//! [`BgmPlayer`] runs on the audio thread. It reads the frames the worker
//! wrote into the ring buffer, fades them in and out, and returns a stereo
//! block. It reads, but never writes the ring buffer, and it does not
//! allocate, lock (except `try_lock` when it is dropped), wait, or log.
//!
//! # States
//!
//! ```text
//!            want                  enough in the ring buffer
//!  Idle ---------------> Priming --------------------------> Playing
//!   ^                      ^                                    |  |
//!   |  fade-out done       | fade-out done        want = false  |  | the ring buffer
//!   |  (Pause)             | (underrun)                         |  | runs dry
//!   +--- FadingOut <-------+------------------------------------+--+
//!          |
//!          | a flush was asked for (Stop, other song)
//!          v
//!       Flushing -- stopped == gen: throw away, drained = gen --> Idle
//!
//!  Playing -- the last frames of a song that does not loop --> Finished
//! ```
//!
//! - **Priming:** play starts when [`bgm_prime_frames`] are in the ring
//!   buffer, or when the worker has marked the end of the song and there is
//!   something left (so a short song, or the tail after an interruption,
//!   starts too).
//! - **Playing** needs one block plus one fade in the ring buffer at the start
//!   of every block (unless the end of the song is marked). When there is
//!   less, the ring buffer is running dry: the player fades out with the
//!   frames that are left, counts an underrun, and primes again.
//! - **Fades** are made by a [`GainNode`]: a straight line of
//!   [`bgm_fade_frames`] frames (10 ms), one step for each frame, whatever
//!   the size of the block. A fade-out reads at most [`bgm_fade_frames`]
//!   frames from the ring buffer, from the start of the block; the rest of
//!   the block is silence.
//! - **Pause** fades out and stops reading. The frames read for the fade are
//!   not heard again: after play, the song goes on after them.
//! - **A flush** (Stop, or another song) has priority over everything. The
//!   player fades out, plays silence, and does not touch the ring buffer
//!   until the worker has written `stopped == gen` for the newest
//!   generation. Then it throws away what is in the ring buffer, puts the
//!   counters of the song back, and writes `drained = gen`. The worker never
//!   stops answering: if it is not running, a flush never ends.
//!
//! # Dropping
//!
//! Dropping the player (the audio backend does it on its worker thread when
//! the stream is closed) moves the reading end of the ring buffer to the
//! keeper that the [`BgmController`] also holds, so the ring buffer is freed
//! on the control side. This needs the controller to live longer than the
//! player; the session makes sure of it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rtrb::Consumer;

use super::bgm::{BgmShared, ConsumerKeeper, RingParts, ring_parts};
use super::{
    AudioBuffer, AudioNode, BgmController, BgmFeeder, GainNode, bgm_fade_frames, bgm_prime_frames,
};

/// What the player is doing (for tests and diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgmPlayerState {
    /// Silent, and not reading.
    Idle,
    /// Play was asked for; silent until enough is in the ring buffer.
    Priming,
    /// Reading and playing.
    Playing,
    /// Fading out (Pause, a ring buffer that ran dry, or a flush).
    FadingOut,
    /// Silent until the worker has answered a flush.
    Flushing,
    /// A song that does not loop has been played to its end.
    Finished,
}

/// What follows when a fade-out is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    /// Pause.
    Idle,
    /// The ring buffer ran dry.
    Priming,
    /// A flush was asked for.
    Flushing,
}

/// Makes the BGM player: the control side, the worker side, and the audio
/// side (to put into [`MicGraph::with_bgm`](super::MicGraph)).
///
/// The sound is at `sample_rate`; the player renders at most `max_frames`
/// frames at a time.
///
/// # Panics
///
/// Panics if `sample_rate` or `max_frames` is 0.
pub fn bgm_channel(sample_rate: u32, max_frames: usize) -> (BgmController, BgmFeeder, BgmPlayer) {
    let RingParts {
        controller,
        feeder,
        consumer,
        shared,
        keeper,
    } = ring_parts(sample_rate, max_frames);
    let player = BgmPlayer::new(consumer, shared, keeper, sample_rate, max_frames);
    (controller, feeder, player)
}

/// The audio side of the BGM player: the ring buffer in, a stereo block out.
#[derive(Debug)]
pub struct BgmPlayer {
    /// The reading end. It is `None` only while a block is processed, and
    /// after the drop has moved it to the keeper.
    consumer: Option<Consumer<f32>>,
    shared: Arc<BgmShared>,
    keeper: ConsumerKeeper,
    sample_rate: u32,
    /// The fade of the song itself (the volume and mute of the channel are
    /// in the graph).
    fade: GainNode,
    out: AudioBuffer,
    state: BgmPlayerState,
    after: After,
    /// The length of a fade, the same as the ramp of `fade`.
    fade_frames: usize,
    /// Frames of the running fade-out that are still to be read.
    fade_left: usize,
    prime_frames: usize,
    /// The generation of the song whose frames are in the ring buffer.
    synced: u32,
    played: u64,
    underruns: u64,
    underrun_frames: u64,
    /// Whether the silence now is part of an interruption.
    in_underrun: bool,
}

/// A fade that is at 0 (no ramp).
fn fresh_fade(sample_rate: u32) -> GainNode {
    let mut fade = GainNode::new(0.0);
    fade.prepare(sample_rate);
    fade
}

/// Takes `count` frames out of the ring buffer into the first `count` frames
/// of `out`. The ring buffer holds left and right one after the other.
fn read_frames(consumer: &mut Consumer<f32>, out: &mut AudioBuffer, count: usize) {
    if count == 0 {
        return;
    }
    let Ok(chunk) = consumer.read_chunk(count * 2) else {
        return;
    };
    let (first, second) = chunk.as_slices();
    for channel in 0..2 {
        let samples = first.iter().chain(second).skip(channel).step_by(2);
        for (target, &sample) in out.channel_mut(channel).iter_mut().zip(samples) {
            *target = sample;
        }
    }
    chunk.commit_all();
}

impl BgmPlayer {
    fn new(
        consumer: Consumer<f32>,
        shared: Arc<BgmShared>,
        keeper: ConsumerKeeper,
        sample_rate: u32,
        max_frames: usize,
    ) -> Self {
        Self {
            consumer: Some(consumer),
            shared,
            keeper,
            sample_rate,
            fade: fresh_fade(sample_rate),
            out: AudioBuffer::new(2, max_frames),
            state: BgmPlayerState::Idle,
            after: After::Idle,
            fade_frames: bgm_fade_frames(sample_rate),
            fade_left: 0,
            prime_frames: bgm_prime_frames(sample_rate, max_frames),
            synced: 0,
            played: 0,
            underruns: 0,
            underrun_frames: 0,
            in_underrun: false,
        }
    }

    /// Returns the most frames [`BgmPlayer::render`] can render at a time.
    pub fn max_frames(&self) -> usize {
        self.out.max_frames()
    }

    /// Returns what the player is doing.
    pub fn state(&self) -> BgmPlayerState {
        self.state
    }

    /// Renders the next `frames` frames: reads the ring buffer, applies the
    /// fade, and returns the stereo block. Silent when nothing is played.
    ///
    /// # Panics
    ///
    /// Panics if `frames` is more than the `max_frames` given to
    /// [`bgm_channel`].
    pub fn render(&mut self, frames: usize) -> &AudioBuffer {
        assert!(
            frames <= self.out.max_frames(),
            "a block of {frames} frames is longer than max_frames"
        );
        self.out.set_frames(frames);
        self.out.clear();
        if frames == 0 {
            return &self.out;
        }
        // The reading end is taken out so that the methods can use `self`.
        let Some(mut consumer) = self.consumer.take() else {
            return &self.out;
        };
        self.process(&mut consumer, frames);
        self.consumer = Some(consumer);
        &self.out
    }

    fn process(&mut self, consumer: &mut Consumer<f32>, frames: usize) {
        // The generation is read for every block: a new flush is seen at
        // once, also while the player waits for the answer to an older one.
        let generation = self.shared.generation.load(Ordering::Acquire);
        let want = self.shared.playing.load(Ordering::Relaxed);
        // The end mark is read before the amount: the worker marks it after
        // its last write, so with the mark, the last frames are counted too.
        let eof = self.shared.eof.load(Ordering::Acquire) == u64::from(self.synced) + 1;
        let available = consumer.slots() / 2;
        self.shared
            .fill_frames
            .store(available.min(u32::MAX as usize) as u32, Ordering::Relaxed);

        if generation != self.synced {
            self.on_flush_request();
        }

        loop {
            match self.state {
                BgmPlayerState::Idle => {
                    if want {
                        self.state = BgmPlayerState::Priming;
                        continue;
                    }
                }
                BgmPlayerState::Priming => {
                    if !want {
                        self.state = BgmPlayerState::Idle;
                        self.in_underrun = false;
                    } else if available >= self.prime_frames || (eof && available > 0) {
                        // Every start fades in from 0: the gain is at 0 after
                        // a fade-out, a flush, and at the beginning.
                        debug_assert_eq!(self.fade.current(), 0.0);
                        self.fade.set_target(1.0);
                        self.state = BgmPlayerState::Playing;
                        self.in_underrun = false;
                        continue;
                    } else if eof {
                        // Nothing left of a song that has ended.
                        self.finish();
                    } else if self.in_underrun {
                        self.count_silence(frames);
                    }
                }
                BgmPlayerState::Playing => {
                    if !want {
                        self.begin_fade(After::Idle);
                        continue;
                    }
                    if !eof && available < frames + self.fade_frames {
                        // Running dry: there is just enough left to fade out.
                        self.underruns += 1;
                        self.shared
                            .underruns
                            .store(self.underruns, Ordering::Relaxed);
                        self.in_underrun = true;
                        self.begin_fade(After::Priming);
                        continue;
                    }
                    let take = frames.min(available);
                    read_frames(consumer, &mut self.out, take);
                    self.apply_fade();
                    self.advance(take);
                    self.shared
                        .min_fill_frames
                        .fetch_min(available.min(u32::MAX as usize) as u32, Ordering::Relaxed);
                    if eof && available == take {
                        self.finish();
                    }
                }
                BgmPlayerState::FadingOut => {
                    if want && self.after == After::Idle {
                        // Play again during the fade of a Pause: the gain
                        // goes back up from where it is.
                        self.fade.set_target(1.0);
                        self.state = BgmPlayerState::Playing;
                        continue;
                    }
                    let wanted = frames.min(self.fade_left);
                    let take = wanted.min(available);
                    read_frames(consumer, &mut self.out, take);
                    self.apply_fade();
                    self.advance(take);
                    if self.after == After::Priming {
                        self.count_silence(frames - take);
                    }
                    // If the sound ran out in the middle of the fade (only
                    // when the worker stopped just then), the fade is cut.
                    self.fade_left = if take < wanted {
                        0
                    } else {
                        self.fade_left - take
                    };
                    if self.fade_left == 0 {
                        self.end_fade();
                    }
                }
                BgmPlayerState::Flushing => {
                    // Silence. The ring buffer is not touched until the
                    // worker has answered the newest generation.
                    if self.shared.stopped.load(Ordering::Acquire) == generation {
                        self.drain(consumer, generation);
                    }
                }
                BgmPlayerState::Finished => {}
            }
            break;
        }
    }

    /// A flush was asked for: fade out if something is heard, and wait.
    fn on_flush_request(&mut self) {
        match self.state {
            BgmPlayerState::Playing => self.begin_fade(After::Flushing),
            BgmPlayerState::FadingOut => {
                self.after = After::Flushing;
                self.in_underrun = false;
            }
            BgmPlayerState::Idle | BgmPlayerState::Priming | BgmPlayerState::Finished => {
                self.state = BgmPlayerState::Flushing;
                self.in_underrun = false;
            }
            BgmPlayerState::Flushing => {}
        }
    }

    fn begin_fade(&mut self, after: After) {
        self.fade.set_target(0.0);
        self.fade_left = self.fade_frames;
        self.after = after;
        self.state = BgmPlayerState::FadingOut;
    }

    fn end_fade(&mut self) {
        // The ramp ends on 0 exactly. If the fade was cut, start over at 0.
        if self.fade.current() != 0.0 {
            self.fade = fresh_fade(self.sample_rate);
        }
        self.state = match self.after {
            After::Idle => {
                self.in_underrun = false;
                BgmPlayerState::Idle
            }
            After::Priming => BgmPlayerState::Priming,
            After::Flushing => BgmPlayerState::Flushing,
        };
    }

    /// Applies the fade to the block, unless it is at 1 and not moving.
    fn apply_fade(&mut self) {
        if self.fade.current() == 1.0 && self.fade.target() == 1.0 {
            return;
        }
        self.fade.process(&mut self.out);
    }

    fn advance(&mut self, frames: usize) {
        self.played += frames as u64;
        self.shared
            .played_frames
            .store(self.played, Ordering::Relaxed);
    }

    fn count_silence(&mut self, frames: usize) {
        self.underrun_frames += frames as u64;
        self.shared
            .underrun_frames
            .store(self.underrun_frames, Ordering::Relaxed);
    }

    fn finish(&mut self) {
        self.shared.finished.store(true, Ordering::Relaxed);
        self.state = BgmPlayerState::Finished;
    }

    /// The worker has stopped writing: throw away what is in the ring buffer,
    /// put the values of the song back, and tell the worker.
    fn drain(&mut self, consumer: &mut Consumer<f32>, generation: u32) {
        let slots = consumer.slots();
        if slots > 0
            && let Ok(chunk) = consumer.read_chunk(slots)
        {
            chunk.commit_all();
        }
        self.played = 0;
        self.shared.played_frames.store(0, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
        self.shared.fill_frames.store(0, Ordering::Relaxed);
        self.shared
            .min_fill_frames
            .store(u32::MAX, Ordering::Relaxed);
        self.fade = fresh_fade(self.sample_rate);
        self.fade_left = 0;
        self.in_underrun = false;
        self.state = BgmPlayerState::Idle;
        self.synced = generation;
        // Last: the worker starts to write when it sees this.
        self.shared.drained.store(generation, Ordering::Release);
    }

    /// The gain of the fade, for the tests.
    #[cfg(test)]
    fn fade_gain(&self) -> f32 {
        self.fade.current()
    }
}

/// Moves the reading end of the ring buffer to the keeper of the controller,
/// so that the control side frees it (the audio backend drops the player on
/// its own thread). `try_lock` never waits; if it fails, the reading end is
/// dropped here.
impl Drop for BgmPlayer {
    fn drop(&mut self) {
        if let Some(consumer) = self.consumer.take()
            && let Ok(mut slot) = self.keeper.try_lock()
        {
            *slot = Some(consumer);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, Weak};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::graph::FlushPoll;

    /// A small rate: a fade is 10 frames, priming 100 frames, and the ring
    /// buffer 1000 frames.
    const RATE: u32 = 1_000;
    const MAX_FRAMES: usize = 8;
    const FADE: usize = 10;
    const PRIME: usize = 100;
    const RING: usize = 1_000;

    struct Rig {
        control: BgmController,
        feeder: BgmFeeder,
        player: BgmPlayer,
        /// The frames written so far (the next value is `written + 1`).
        written: usize,
    }

    fn rig() -> Rig {
        rig_with(MAX_FRAMES)
    }

    fn rig_with(max_frames: usize) -> Rig {
        let (control, feeder, player) = bgm_channel(RATE, max_frames);
        Rig {
            control,
            feeder,
            player,
            written: 0,
        }
    }

    /// Writes `count` frames of a counter: frame `i` is `i + 1` on the left
    /// and `-(i + 1)` on the right.
    fn feed(rig: &mut Rig, count: usize) {
        let left: Vec<f32> = (0..count).map(|i| (rig.written + i + 1) as f32).collect();
        let right: Vec<f32> = left.iter().map(|l| -l).collect();
        assert_eq!(rig.feeder.write(&left, &right), count);
        rig.written += count;
    }

    fn render(rig: &mut Rig, frames: usize) -> Vec<f32> {
        let out = rig.player.render(frames);
        let (left, right) = (out.channel(0), out.channel(1));
        for (l, r) in left.iter().zip(right) {
            assert_eq!(*l, -*r, "left and right differ");
        }
        left.to_vec()
    }

    /// Frames in the ring buffer.
    fn fill(rig: &Rig) -> usize {
        RING - rig.feeder.free_frames()
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-3,
            "{actual} is not {expected}"
        );
    }

    /// Plays a song of `frames` frames to full volume. Four blocks of 8
    /// frames are read: 32 frames.
    fn start(rig: &mut Rig, frames: usize) {
        rig.control.play();
        feed(rig, frames);
        for _ in 0..4 {
            render(rig, MAX_FRAMES);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        assert_eq!(rig.player.fade_gain(), 1.0);
    }

    /// Asks for a flush and runs the worker's side and the player until the
    /// player has thrown the sound away.
    fn flush(rig: &mut Rig) {
        rig.control.request_flush();
        for _ in 0..50 {
            rig.feeder.poll_flush();
            render(rig, MAX_FRAMES);
            if rig.control.is_flushed() {
                return;
            }
        }
        panic!("the flush did not end");
    }

    #[test]
    fn it_reports_its_max_frames_and_starts_idle() {
        let rig = rig();
        assert_eq!(rig.player.max_frames(), MAX_FRAMES);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
    }

    #[test]
    #[should_panic(expected = "longer than max_frames")]
    fn a_block_longer_than_max_frames_panics() {
        let mut rig = rig();
        rig.player.render(MAX_FRAMES + 1);
    }

    #[test]
    fn a_block_of_0_frames_is_empty() {
        let mut rig = rig();
        assert_eq!(rig.player.render(0).frames(), 0);
    }

    #[test]
    fn the_player_can_move_to_the_audio_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<BgmPlayer>();
    }

    #[test]
    fn idle_is_silent_and_does_not_read() {
        let mut rig = rig();
        feed(&mut rig, 500);
        for _ in 0..5 {
            assert!(render(&mut rig, 8).iter().all(|&s| s == 0.0));
        }
        assert_eq!(fill(&rig), 500);
        assert_eq!(rig.control.stats().fill_frames, 500);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
    }

    #[test]
    fn play_waits_for_priming_and_then_fades_in() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, PRIME - 1);
        for _ in 0..3 {
            assert!(render(&mut rig, 8).iter().all(|&s| s == 0.0));
            assert_eq!(rig.player.state(), BgmPlayerState::Priming);
        }
        // Waiting for the first priming is not an underrun.
        let stats = rig.control.stats();
        assert_eq!((stats.underruns, stats.underrun_frames), (0, 0));
        assert_eq!(fill(&rig), PRIME - 1);

        feed(&mut rig, 1);
        // Frame `i` is the value `i + 1` at the gain `(i + 1) / 10`.
        let block = render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        for (i, &sample) in block.iter().enumerate() {
            assert_close(sample, (i + 1) as f32 * (i + 1) as f32 / 10.0);
        }
        let block = render(&mut rig, 8);
        assert_close(block[0], 9.0 * 0.9);
        assert_close(block[1], 10.0 * 1.0);
        assert_eq!(block[2], 11.0);
        assert_eq!(block[7], 16.0);
        assert_eq!(rig.control.stats().played_frames, 16);
    }

    #[test]
    fn the_fade_of_a_song_starts_at_0_and_the_gain_node_has_no_ramp_to_get_there() {
        let mut rig = rig();
        // A new player: the gain is 0 from the start (`GainNode::new(0.0)`
        // starts there without a ramp).
        assert_eq!(rig.player.fade_gain(), 0.0);
        rig.control.play();
        feed(&mut rig, 200);
        let block = render(&mut rig, 8);
        // The first frame is already on the way up, not at full volume.
        assert!(block[0] < 1.0 * 0.11, "{}", block[0]);
        assert_eq!(rig.player.fade_gain(), 0.8);
    }

    #[test]
    fn frames_come_out_in_order_without_gaps() {
        let mut rig = rig();
        rig.control.play();
        let mut heard = Vec::new();
        let mut size = 1;
        for _ in 0..300 {
            if fill(&rig) < 400 {
                feed(&mut rig, 400);
            }
            size = size % 8 + 1;
            heard.extend(render(&mut rig, size));
        }
        assert!(heard.len() > 600);
        // The first 10 frames fade in; from then on the counter is exact.
        for (i, &sample) in heard.iter().enumerate().skip(FADE) {
            assert_eq!(sample, (i + 1) as f32, "frame {i}");
        }
        let stats = rig.control.stats();
        assert_eq!(stats.underruns, 0);
        assert!(stats.min_fill_frames >= 400 - 8 * 8 - 8);
    }

    #[test]
    fn pause_fades_out_and_play_goes_on_after_the_frames_of_the_fade() {
        let mut rig = rig();
        start(&mut rig, 300);
        // 4 blocks of 8 frames have been read: 32 frames.
        rig.control.pause();
        let first = render(&mut rig, 8);
        let second = render(&mut rig, 8);
        for (i, &sample) in first.iter().enumerate() {
            let gain = 1.0 - 0.1 * (i + 1) as f32;
            assert_close(sample, (33 + i) as f32 * gain);
        }
        assert_close(second[0], 41.0 * 0.1);
        assert_eq!(second[1], 0.0);
        assert!(second[2..].iter().all(|&s| s == 0.0));
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
        assert_eq!(rig.player.fade_gain(), 0.0, "exactly 0 after the fade");

        // Nothing is read while paused, and the position stays.
        let kept = fill(&rig);
        for _ in 0..5 {
            assert!(render(&mut rig, 8).iter().all(|&s| s == 0.0));
        }
        assert_eq!(fill(&rig), kept);
        assert_eq!(rig.control.stats().played_frames, 42);

        // The 10 frames of the fade (33..42) are not heard again.
        rig.control.play();
        let block = render(&mut rig, 8);
        for (i, &sample) in block.iter().enumerate() {
            assert_close(sample, (43 + i) as f32 * (i + 1) as f32 / 10.0);
        }
        let block = render(&mut rig, 8);
        assert_close(block[1], 52.0);
        assert_eq!(block[2], 53.0);
    }

    #[test]
    fn play_during_the_fade_of_a_pause_goes_back_up_from_where_it_is() {
        let mut rig = rig();
        start(&mut rig, 300);
        rig.control.pause();
        let fading = render(&mut rig, 4);
        // The gain is at 0.6 after 4 frames of the fade.
        assert_close(fading[3], 36.0 * 0.6);
        rig.control.play();
        let back = render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        // A straight line from 0.6 to 1.0 in 10 frames: no jump.
        for (i, &sample) in back.iter().enumerate() {
            let gain = 0.6 + 0.04 * (i + 1) as f32;
            assert_close(sample, (37 + i) as f32 * gain);
        }
    }

    #[test]
    fn a_block_longer_than_the_fade_reads_only_the_fade() {
        let mut rig = rig_with(32);
        rig.control.play();
        feed(&mut rig, 300);
        for _ in 0..3 {
            render(&mut rig, 32);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        rig.control.pause();
        let before = fill(&rig);
        let block = render(&mut rig, 32);
        for (i, &sample) in block.iter().enumerate().take(FADE) {
            assert_close(sample, (97 + i) as f32 * (1.0 - 0.1 * (i + 1) as f32));
        }
        // The rest of the block is silence, and was not read.
        assert!(block[FADE..].iter().all(|&s| s == 0.0));
        assert_eq!(before - fill(&rig), FADE);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
    }

    #[test]
    fn a_block_shorter_than_the_fade_spans_blocks() {
        let mut rig = rig_with(4);
        rig.control.play();
        feed(&mut rig, 300);
        for _ in 0..4 {
            render(&mut rig, 4);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        rig.control.pause();
        let before = fill(&rig);
        let first = render(&mut rig, 4);
        assert_eq!(rig.player.state(), BgmPlayerState::FadingOut);
        let second = render(&mut rig, 4);
        assert_eq!(rig.player.state(), BgmPlayerState::FadingOut);
        let third = render(&mut rig, 4);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
        assert_close(first[0], 17.0 * 0.9);
        assert_close(second[0], 21.0 * 0.5);
        assert_close(third[0], 25.0 * 0.1);
        assert_eq!(third[1], 0.0);
        assert!(third[2..].iter().all(|&s| s == 0.0));
        assert_eq!(before - fill(&rig), FADE);
    }

    #[test]
    fn a_ring_buffer_that_runs_dry_fades_out_is_counted_and_primes_again() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 120);
        // Playing needs one block and one fade (18 frames) at the start of
        // each block. Without more sound, it runs dry.
        let mut blocks = 0;
        while rig.control.stats().underruns == 0 {
            render(&mut rig, 8);
            blocks += 1;
            assert!(blocks < 30, "it never ran dry");
        }
        assert_eq!(rig.control.stats().underruns, 1);
        assert_eq!(rig.player.state(), BgmPlayerState::FadingOut);

        // The fade-out ends; the silence is counted for each block.
        let mut counted = rig.control.stats().underrun_frames;
        let mut silent_blocks = 0;
        for _ in 0..6 {
            let block = render(&mut rig, 8);
            let now = rig.control.stats().underrun_frames;
            if rig.player.state() == BgmPlayerState::Priming {
                if block.iter().all(|&s| s == 0.0) {
                    // A whole block of silence adds a whole block.
                    assert_eq!(now - counted, 8);
                    silent_blocks += 1;
                } else {
                    // The end of the fade: the zeros after its last frame.
                    assert!(now - counted < 8);
                }
            }
            counted = now;
        }
        assert!(silent_blocks >= 3);
        let stats = rig.control.stats();
        assert_eq!(stats.underruns, 1, "one interruption is one underrun");
        assert!(stats.underrun_frames >= 3 * 8);

        // Everything that was read is exactly what was written, in order:
        // the player goes on with the next frame.
        let left_over = fill(&rig);
        let consumed = rig.written - left_over;
        assert_eq!(rig.control.stats().played_frames, consumed as u64);
        feed(&mut rig, PRIME);
        let block = render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        assert_close(block[0], (consumed + 1) as f32 * 0.1);
        assert_close(block[1], (consumed + 2) as f32 * 0.2);
        assert_eq!(rig.control.stats().underruns, 1);
    }

    #[test]
    fn the_silence_of_an_underrun_is_counted_block_by_block() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 120);
        while rig.control.stats().underruns == 0 {
            render(&mut rig, 8);
        }
        // Block by block from here: every block that has silence adds it.
        let mut previous = rig.control.stats().underrun_frames;
        let mut grew = 0;
        for _ in 0..8 {
            render(&mut rig, 8);
            let now = rig.control.stats().underrun_frames;
            assert!(now >= previous);
            if now > previous {
                grew += 1;
            }
            previous = now;
        }
        assert!(grew >= 4, "the count grew in {grew} blocks");
    }

    #[test]
    fn a_priming_that_does_not_end_stays_silent_until_the_worker_writes() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 30);
        for _ in 0..50 {
            assert!(render(&mut rig, 8).iter().all(|&s| s == 0.0));
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Priming);
        assert_eq!(rig.control.stats().underruns, 0);
    }

    #[test]
    fn a_song_shorter_than_priming_plays_when_its_end_is_marked() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 30);
        rig.feeder.mark_eof();
        let mut heard = Vec::new();
        for _ in 0..6 {
            heard.extend(render(&mut rig, 8));
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Finished);
        assert!(rig.control.is_finished());
        let stats = rig.control.stats();
        assert_eq!(stats.played_frames, 30);
        assert_eq!(stats.underruns, 0);
        for (i, &sample) in heard.iter().enumerate().take(30).skip(FADE) {
            assert_eq!(sample, (i + 1) as f32);
        }
        // Silent after the last frame.
        assert!(heard[30..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn the_end_of_a_song_is_played_to_the_last_frame_and_is_not_an_underrun() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 150);
        rig.feeder.mark_eof();
        let mut heard = Vec::new();
        for _ in 0..30 {
            heard.extend(render(&mut rig, 8));
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Finished);
        let stats = rig.control.stats();
        assert_eq!((stats.played_frames, stats.underruns), (150, 0));
        assert!(stats.finished);
        for (i, &sample) in heard.iter().enumerate().take(150).skip(FADE) {
            assert_eq!(sample, (i + 1) as f32, "frame {i}");
        }
        assert!(heard[150..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn a_song_that_ends_exactly_on_a_block_is_finished_at_once() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 128);
        rig.feeder.mark_eof();
        for _ in 0..16 {
            render(&mut rig, 8);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Finished);
        assert_eq!(rig.control.stats().played_frames, 128);
    }

    #[test]
    fn a_song_with_no_frames_is_finished_at_once() {
        let mut rig = rig();
        rig.control.play();
        rig.feeder.mark_eof();
        render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::Finished);
        assert!(rig.control.is_finished());
        assert_eq!(rig.control.stats().played_frames, 0);
    }

    #[test]
    fn the_end_of_the_song_ends_the_priming_after_an_interruption_too() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 120);
        while rig.player.state() != BgmPlayerState::Priming {
            render(&mut rig, 8);
        }
        assert_eq!(rig.control.stats().underruns, 1);
        // The worker only had a little more, and it is the end.
        feed(&mut rig, 5);
        rig.feeder.mark_eof();
        for _ in 0..10 {
            render(&mut rig, 8);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Finished);
        // Every frame was heard once: 120 + 5.
        assert_eq!(rig.control.stats().played_frames, 125);
        assert_eq!(rig.control.stats().underruns, 1);
    }

    #[test]
    fn stop_fades_out_and_leaves_the_ring_buffer_alone_until_the_worker_answers() {
        let mut rig = rig();
        start(&mut rig, 200);
        rig.control.request_flush();

        let first = render(&mut rig, 8);
        let second = render(&mut rig, 8);
        for (i, &sample) in first.iter().enumerate() {
            assert_close(sample, (33 + i) as f32 * (1.0 - 0.1 * (i + 1) as f32));
        }
        assert_close(second[0], 41.0 * 0.1);
        assert!(second[1..].iter().all(|&s| s == 0.0));
        assert_eq!(rig.player.state(), BgmPlayerState::Flushing);

        // The worker has not answered: silent, and nothing is touched.
        let kept = fill(&rig);
        assert_eq!(kept, 200 - 42);
        for _ in 0..10 {
            assert!(render(&mut rig, 8).iter().all(|&s| s == 0.0));
        }
        assert_eq!(fill(&rig), kept);
        assert!(!rig.control.is_flushed());
        assert_eq!(rig.player.state(), BgmPlayerState::Flushing);

        // The worker answers; the next block throws the sound away.
        assert_eq!(rig.feeder.poll_flush(), FlushPoll::Waiting);
        render(&mut rig, 8);
        assert_eq!(fill(&rig), 0);
        assert!(rig.control.is_flushed());
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
        assert_eq!(rig.feeder.poll_flush(), FlushPoll::Done);

        let stats = rig.control.stats();
        assert_eq!(stats.played_frames, 0);
        assert!(!stats.finished);
        assert_eq!(stats.fill_frames, 0);
        assert_eq!(stats.min_fill_frames, 0, "nothing measured since");
        assert_eq!(rig.player.fade_gain(), 0.0);
    }

    #[test]
    fn the_old_sound_is_not_heard_after_a_stop() {
        let mut rig = rig();
        rig.control.play();
        let old = vec![1_000_000.0_f32; 300];
        let old_right = vec![-1_000_000.0_f32; 300];
        assert_eq!(rig.feeder.write(&old, &old_right), 300);
        for _ in 0..5 {
            render(&mut rig, 8);
        }
        flush(&mut rig);

        // A new song, from the start.
        rig.written = 0;
        rig.control.play();
        feed(&mut rig, 300);
        let mut heard = Vec::new();
        for _ in 0..20 {
            heard.extend(render(&mut rig, 8));
        }
        assert!(heard.iter().any(|&s| s != 0.0));
        assert!(heard.iter().all(|&s| s.abs() < 1_000.0), "old sound heard");
        assert_close(heard[0], 0.1);
    }

    #[test]
    fn play_right_after_stop_waits_for_the_flush_and_then_for_the_new_sound() {
        let mut rig = rig();
        start(&mut rig, 300);
        rig.control.request_flush();
        rig.control.play();

        // Until the sound is thrown away, only the fade-out and silence.
        let mut seen_flushing = false;
        for _ in 0..6 {
            let was_flushing = rig.player.state() == BgmPlayerState::Flushing;
            let block = render(&mut rig, 8);
            if was_flushing {
                seen_flushing = true;
                assert!(block.iter().all(|&s| s == 0.0));
            }
            assert_ne!(rig.player.state(), BgmPlayerState::Priming);
            assert_ne!(rig.player.state(), BgmPlayerState::Playing);
        }
        assert!(seen_flushing);
        assert!(!rig.control.is_flushed());

        // The worker answers; the sound is thrown away. The player then
        // primes (it wants to play) and waits for the new sound.
        assert_eq!(rig.feeder.poll_flush(), FlushPoll::Waiting);
        render(&mut rig, 8);
        assert!(rig.control.is_flushed());
        assert_eq!(rig.feeder.poll_flush(), FlushPoll::Done);
        for _ in 0..3 {
            assert!(render(&mut rig, 8).iter().all(|&s| s == 0.0));
            assert_eq!(rig.player.state(), BgmPlayerState::Priming);
        }
        assert_eq!(rig.control.stats().underruns, 0);

        rig.written = 0;
        feed(&mut rig, PRIME);
        let block = render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        assert_close(block[0], 0.1);
        assert_close(block[7], 8.0 * 0.8);
    }

    #[test]
    fn another_stop_while_waiting_waits_for_the_newest_generation() {
        let mut rig = rig();
        start(&mut rig, 300);
        rig.control.request_flush();
        for _ in 0..4 {
            render(&mut rig, 8);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Flushing);
        // The worker answers generation 1 ...
        assert_eq!(rig.feeder.poll_flush(), FlushPoll::Waiting);
        // ... and before the player sees it, Stop is asked for again.
        assert_eq!(rig.control.request_flush(), 2);
        let kept = fill(&rig);
        render(&mut rig, 8);
        render(&mut rig, 8);
        // The answer for generation 1 is no answer for generation 2.
        assert!(!rig.control.is_flushed());
        assert_eq!(fill(&rig), kept, "nothing was thrown away");
        assert_eq!(rig.player.state(), BgmPlayerState::Flushing);

        assert_eq!(rig.feeder.poll_flush(), FlushPoll::Waiting);
        render(&mut rig, 8);
        assert!(rig.control.is_flushed());
        assert_eq!(rig.feeder.poll_flush(), FlushPoll::Done);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
    }

    #[test]
    fn a_stop_while_fading_out_for_a_flush_is_taken_by_the_newest_generation() {
        let mut rig = rig();
        start(&mut rig, 300);
        rig.control.request_flush();
        render(&mut rig, 4);
        assert_eq!(rig.player.state(), BgmPlayerState::FadingOut);
        rig.control.request_flush();
        flush(&mut rig);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
        assert!(rig.control.is_flushed());
    }

    #[test]
    fn a_flush_works_from_every_state() {
        // Idle.
        let mut rig = rig();
        feed(&mut rig, 50);
        flush(&mut rig);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
        assert_eq!(fill(&rig), 0);

        // Priming.
        let mut rig = self::rig();
        rig.control.play();
        feed(&mut rig, 50);
        render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::Priming);
        flush(&mut rig);
        assert_eq!(fill(&rig), 0);

        // Playing.
        let mut rig = self::rig();
        start(&mut rig, 300);
        flush(&mut rig);
        assert_eq!(fill(&rig), 0);

        // The fade of a pause.
        let mut rig = self::rig();
        start(&mut rig, 300);
        rig.control.pause();
        render(&mut rig, 4);
        assert_eq!(rig.player.state(), BgmPlayerState::FadingOut);
        flush(&mut rig);
        assert_eq!(fill(&rig), 0);

        // The fade of a ring buffer that ran dry.
        let mut rig = self::rig();
        rig.control.play();
        feed(&mut rig, 120);
        while rig.control.stats().underruns == 0 {
            render(&mut rig, 8);
        }
        flush(&mut rig);
        assert_eq!(fill(&rig), 0);

        // An interruption's priming.
        let mut rig = self::rig();
        rig.control.play();
        feed(&mut rig, 120);
        while rig.player.state() != BgmPlayerState::Priming {
            render(&mut rig, 8);
        }
        flush(&mut rig);
        assert_eq!(fill(&rig), 0);

        // A song that ended.
        let mut rig = self::rig();
        rig.control.play();
        feed(&mut rig, 30);
        rig.feeder.mark_eof();
        for _ in 0..6 {
            render(&mut rig, 8);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Finished);
        flush(&mut rig);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
        assert!(!rig.control.is_finished());
        assert_eq!(rig.control.stats().played_frames, 0);
    }

    #[test]
    fn the_end_mark_of_an_old_song_is_not_the_end_of_the_new_one() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 30);
        rig.feeder.mark_eof();
        for _ in 0..6 {
            render(&mut rig, 8);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Finished);
        flush(&mut rig);

        // The new song is not at its end: priming waits for 100 frames.
        rig.written = 0;
        rig.control.play();
        feed(&mut rig, 30);
        for _ in 0..5 {
            assert!(render(&mut rig, 8).iter().all(|&s| s == 0.0));
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Priming);
    }

    #[test]
    fn the_fade_of_a_song_ends_exactly_on_0() {
        let mut rig = rig();
        start(&mut rig, 300);
        rig.control.pause();
        for _ in 0..3 {
            render(&mut rig, 8);
        }
        assert_eq!(rig.player.fade_gain(), 0.0);
        assert_eq!(rig.player.state(), BgmPlayerState::Idle);
    }

    /// The fade-out is cut when the sound runs out in the middle of it. That
    /// takes a Pause whose fade is interrupted by Play while only the last
    /// frames of the ring buffer are left. The gain must start again from 0.
    #[test]
    fn a_fade_that_is_cut_starts_the_next_fade_in_from_0() {
        let mut rig = rig();
        rig.control.play();
        // 122 frames: after 13 blocks of 8 there are 18 left, which is one
        // block and one fade, the least that is played.
        feed(&mut rig, 122);
        for _ in 0..14 {
            render(&mut rig, 8);
        }
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        assert_eq!(fill(&rig), 10, "one fade is left");
        assert_eq!(rig.control.stats().underruns, 0);

        // Pause: the fade reads 8 frames of the 10 that are left.
        rig.control.pause();
        render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::FadingOut);
        assert_eq!(fill(&rig), 2);

        // Play during the fade: the player is playing again, finds 2 frames
        // (less than a block and a fade), and fades out for an underrun. The
        // fade needs 10 frames but there are only 2.
        rig.control.play();
        let block = render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::Priming);
        assert_eq!(rig.control.stats().underruns, 1);
        assert_eq!(fill(&rig), 0);
        assert!(block[2..].iter().all(|&s| s == 0.0));
        assert_eq!(rig.control.stats().played_frames, 122);
        // The fade was cut, so its gain was not at 0: it is made again at 0.
        assert_eq!(rig.player.fade_gain(), 0.0);

        // The next start fades in from 0 (not from where the cut fade was).
        feed(&mut rig, PRIME);
        let block = render(&mut rig, 8);
        assert_eq!(rig.player.state(), BgmPlayerState::Playing);
        assert_close(block[0], 123.0 * 0.1);
        assert_close(block[7], 130.0 * 0.8);
    }

    #[test]
    fn the_least_fill_is_measured_only_while_playing() {
        let mut rig = rig();
        rig.control.play();
        feed(&mut rig, 30);
        for _ in 0..3 {
            render(&mut rig, 8);
        }
        // Priming: the small amount is not a measurement.
        assert_eq!(rig.control.take_min_fill_frames(), 30);
        feed(&mut rig, 400);
        render(&mut rig, 8);
        render(&mut rig, 8);
        assert_eq!(rig.control.take_min_fill_frames(), 422);
    }

    /// Each song of the test below has a tag; the values stay below 2^24,
    /// so that an `f32` holds them exactly.
    const TAG_STEP: u32 = 200_000;

    /// The tag and the index of a value of the test below.
    fn split(value: f32) -> (u32, u32) {
        let value = value as u32;
        (value / TAG_STEP, value % TAG_STEP)
    }

    #[test]
    fn a_worker_thread_and_an_audio_thread_keep_every_song_in_order() {
        let (control, feeder, mut player) = bgm_channel(RATE, MAX_FRAMES);
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut feeder = feeder;
                let mut state = 7_u64;
                let mut rand = move |n: u64| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state % n
                };
                let mut tag = 0_u32;
                let mut next = 0_u32;
                while !stop.load(Ordering::Relaxed) {
                    match feeder.poll_flush() {
                        FlushPoll::Waiting => {
                            thread::sleep(Duration::from_micros(20));
                            continue;
                        }
                        FlushPoll::Done => {
                            tag += 1;
                            next = 0;
                        }
                        FlushPoll::Idle => {}
                    }
                    if rand(60) == 0 {
                        thread::sleep(Duration::from_micros(400));
                    }
                    let size = 1 + rand(40) as usize;
                    let left: Vec<f32> = (0..size)
                        .map(|i| (tag * TAG_STEP + next + i as u32 + 1) as f32)
                        .collect();
                    let right: Vec<f32> = left.iter().map(|l| -l).collect();
                    let written = feeder.write(&left, &right);
                    next += written as u32;
                    if written == 0 {
                        thread::sleep(Duration::from_micros(20));
                    }
                }
            })
        };

        let mut state = 99_u64;
        let mut rand = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        control.play();
        let mut previous_exact = false;
        let mut last: Option<f32> = None;
        let mut newest_tag = 0;
        let mut exact_blocks = 0;
        for block in 0..20_000 {
            if block % 250 == 0 {
                match rand(3) {
                    0 => control.play(),
                    1 => control.pause(),
                    _ => {
                        control.request_flush();
                        control.play();
                    }
                }
            }
            let size = 1 + rand(8) as usize;
            let out = player.render(size);
            let (left, right) = (out.channel(0).to_vec(), out.channel(1).to_vec());
            for (l, r) in left.iter().zip(&right) {
                assert_eq!(*l, -*r);
            }
            // A block is exact (not faded) if the gain was 1 before and
            // after it.
            let exact = previous_exact
                && player.state() == BgmPlayerState::Playing
                && player.fade_gain() == 1.0;
            if exact {
                exact_blocks += 1;
                if let Some(last) = last {
                    assert_eq!(left[0], last + 1.0, "a gap or a repeat");
                }
                for pair in left.windows(2) {
                    assert_eq!(pair[1], pair[0] + 1.0);
                }
                let (tag, _) = split(left[0]);
                assert!(tag >= newest_tag, "a song of an older generation was heard");
                newest_tag = tag;
                last = left.last().copied();
            } else {
                last = None;
            }
            previous_exact = player.state() == BgmPlayerState::Playing && player.fade_gain() == 1.0;
            thread::sleep(Duration::from_micros(10));
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(exact_blocks > 100, "{exact_blocks} exact blocks");
        let stats = control.stats();
        assert!(stats.played_frames > 0);
    }

    /// The three parts and a weak reference to the keeper.
    type DropParts = (
        BgmController,
        BgmFeeder,
        BgmPlayer,
        Weak<Mutex<Option<Consumer<f32>>>>,
    );

    fn parts_for_drops() -> DropParts {
        let RingParts {
            controller,
            feeder,
            consumer,
            shared,
            keeper,
        } = ring_parts(RATE, MAX_FRAMES);
        let weak = Arc::downgrade(&keeper);
        let player = BgmPlayer::new(consumer, shared, keeper, RATE, MAX_FRAMES);
        (controller, feeder, player, weak)
    }

    #[test]
    fn the_control_side_frees_the_ring_buffer_when_the_controller_lives_longer() {
        let (controller, feeder, player, weak) = parts_for_drops();
        // The stream is closed: the player is dropped on another thread.
        thread::spawn(move || drop(player)).join().unwrap();
        // The reading end is in the keeper, which the controller holds.
        let keeper = weak.upgrade().expect("the controller holds the keeper");
        assert!(keeper.lock().unwrap().is_some());
        drop(keeper);
        drop(feeder);
        assert!(weak.upgrade().is_some());
        drop(controller);
        // The control side was the last: the reading end is gone with it.
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn a_controller_dropped_first_leaves_the_last_reference_to_the_player() {
        let (controller, feeder, player, weak) = parts_for_drops();
        drop(controller);
        drop(feeder);
        // The player holds the keeper now. Dropping it frees the ring buffer
        // there (this is why the controller has to live longer).
        assert!(weak.upgrade().is_some());
        drop(player);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn the_three_parts_can_be_dropped_in_any_order() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let (controller, feeder, player, weak) = parts_for_drops();
            let mut controller = Some(controller);
            let mut feeder = Some(feeder);
            let mut player = Some(player);
            for part in order {
                match part {
                    0 => drop(controller.take()),
                    1 => drop(feeder.take()),
                    _ => drop(player.take()),
                }
            }
            assert!(weak.upgrade().is_none(), "{order:?}");
        }
    }
}
