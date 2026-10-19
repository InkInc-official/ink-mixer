//! The control side of the sound effects (SE) player (ADR-0009).
//!
//! [`SeController`] lives on the control side (the GUI, `run`). It holds the
//! sounds in their slots, sends [`SeCommand`]s to the [`SePlayer`] on the
//! audio thread, and collects the `Arc`s the player gives back.
//!
//! # Why the return queue never overflows
//!
//! Every `Play` command carries one `Arc` to the audio side, and the audio
//! side gives that `Arc` back exactly once. The controller counts the `Arc`s
//! that are out (sent minus received) and refuses a play ([`SeError::Busy`])
//! when that number has reached [`SE_RETURN_CAPACITY`]. At most that many
//! `Arc`s can be in the return queue, so the audio thread never finds it
//! full. The `Arc`s that are out are in the command queue, in the voices, or
//! in the return queue, and a collect takes the last ones out.
//!
//! # Memory
//!
//! The sounds in the slots may take [`SE_TOTAL_BYTES_LIMIT`] together. A
//! sound that is replaced or cleared while it plays stays in memory until
//! its voices end, so the total can be higher for a moment. Loading a file
//! takes about 3.3 times the size of the result for a while (decode,
//! resample, copy); load one file at a time.

use std::fmt;
use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use super::{
    SE_COMMAND_CAPACITY, SE_RETURN_CAPACITY, SE_SLOTS, SE_TOTAL_BYTES_LIMIT, SeCommand, SePlayer,
    SePolicy, SeStats, SeStatsSnapshot, SoundData,
};

/// Why the controller refused something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeError {
    /// The slot number is [`SE_SLOTS`] or more.
    SlotOutOfRange { slot: usize },
    /// The slot has no sound.
    NoSound { slot: usize },
    /// The sound has no frames.
    EmptySound,
    /// The sound is not at the sample rate of the player.
    RateMismatch { expected: u32, actual: u32 },
    /// The sounds in the slots would take more memory than the limit.
    TotalTooLarge {
        total_bytes: usize,
        limit_bytes: usize,
    },
    /// Too many sounds are out (playing or waiting). Try again later.
    Busy,
    /// The command queue is full. Try again later.
    QueueFull,
}

impl fmt::Display for SeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SlotOutOfRange { slot } => {
                write!(f, "slot {slot} does not exist (there are {SE_SLOTS})")
            }
            Self::NoSound { slot } => write!(f, "slot {slot} has no sound"),
            Self::EmptySound => write!(f, "the sound has no frames"),
            Self::RateMismatch { expected, actual } => write!(
                f,
                "the sound is at {actual} Hz, but the output is at {expected} Hz"
            ),
            Self::TotalTooLarge {
                total_bytes,
                limit_bytes,
            } => write!(
                f,
                "the sounds would take {total_bytes} bytes, more than the limit of {limit_bytes} bytes"
            ),
            Self::Busy => write!(f, "too many sounds are playing or waiting"),
            Self::QueueFull => write!(f, "the command queue is full"),
        }
    }
}

impl std::error::Error for SeError {}

/// The control side of the SE player.
#[derive(Debug)]
pub struct SeController {
    commands: Producer<SeCommand>,
    returns: Consumer<Arc<SoundData>>,
    stats: Arc<SeStats>,
    sample_rate: u32,
    slots: Vec<Option<Arc<SoundData>>>,
    total_bytes: usize,
    total_limit: usize,
    /// How many `Arc`s were sent to the audio side.
    sent: u64,
    /// How many `Arc`s came back.
    received: u64,
}

/// Creates the two ends of an SE player: the controller for the control
/// side and the player for the audio thread.
///
/// The sounds must be at `sample_rate`. The player renders at most
/// `max_frames` frames at a time.
pub fn se_channel(sample_rate: u32, max_frames: usize) -> (SeController, SePlayer) {
    channel_with_total_limit(sample_rate, max_frames, SE_TOTAL_BYTES_LIMIT)
}

fn channel_with_total_limit(
    sample_rate: u32,
    max_frames: usize,
    total_limit: usize,
) -> (SeController, SePlayer) {
    let (commands, command_consumer) = RingBuffer::new(SE_COMMAND_CAPACITY);
    let (return_producer, returns) = RingBuffer::new(SE_RETURN_CAPACITY);
    let stats = Arc::new(SeStats::new());
    let player = SePlayer::new(
        command_consumer,
        return_producer,
        Arc::clone(&stats),
        sample_rate,
        max_frames,
    );
    let controller = SeController {
        commands,
        returns,
        stats,
        sample_rate,
        slots: vec![None; SE_SLOTS],
        total_bytes: 0,
        total_limit,
        sent: 0,
        received: 0,
    };
    (controller, player)
}

impl SeController {
    /// Returns the sample rate the sounds must have.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns the sound in `slot`, if there is one.
    pub fn sound(&self, slot: usize) -> Option<&Arc<SoundData>> {
        self.slots.get(slot).and_then(Option::as_ref)
    }

    /// Returns the memory the sounds in the slots take, in bytes.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Returns how many `Arc`s are out: in the command queue, in the voices,
    /// or in the return queue. It is never more than [`SE_RETURN_CAPACITY`].
    pub fn outstanding(&self) -> usize {
        (self.sent - self.received) as usize
    }

    /// Returns the counters of the player and the controller.
    pub fn stats(&self) -> SeStatsSnapshot {
        self.stats.snapshot()
    }

    /// Puts `sound` in `slot`, in place of the sound that is there. A sound
    /// that is playing goes on playing until it ends.
    ///
    /// The sound must not be empty, must be at the sample rate of the
    /// player, and the sounds in the slots must stay under
    /// [`SE_TOTAL_BYTES_LIMIT`]. If one of these fails, nothing changes.
    pub fn set_sound(&mut self, slot: usize, sound: Arc<SoundData>) -> Result<(), SeError> {
        self.check_slot(slot)?;
        if sound.is_empty() {
            return Err(SeError::EmptySound);
        }
        if sound.sample_rate() != self.sample_rate {
            return Err(SeError::RateMismatch {
                expected: self.sample_rate,
                actual: sound.sample_rate(),
            });
        }
        let replaced = self.slots[slot]
            .as_ref()
            .map_or(0, |old| old.memory_bytes());
        let total_bytes = self.total_bytes - replaced + sound.memory_bytes();
        if total_bytes > self.total_limit {
            return Err(SeError::TotalTooLarge {
                total_bytes,
                limit_bytes: self.total_limit,
            });
        }
        self.total_bytes = total_bytes;
        self.slots[slot] = Some(sound);
        Ok(())
    }

    /// Takes the sound out of `slot`. A sound that is playing goes on
    /// playing until it ends.
    pub fn clear(&mut self, slot: usize) -> Result<(), SeError> {
        self.check_slot(slot)?;
        if let Some(old) = self.slots[slot].take() {
            self.total_bytes -= old.memory_bytes();
        }
        Ok(())
    }

    /// Plays the sound in `slot` at the linear gain `gain`.
    ///
    /// Returns [`SeError::Busy`] if too many `Arc`s are out (nothing is
    /// sent, and `plays_busy` counts it) and [`SeError::QueueFull`] if the
    /// command queue is full (`commands_dropped` counts it).
    pub fn play(&mut self, slot: usize, gain: f32, policy: SePolicy) -> Result<(), SeError> {
        self.check_slot(slot)?;
        let Some(sound) = self.slots[slot].as_ref().map(Arc::clone) else {
            return Err(SeError::NoSound { slot });
        };
        self.collect();
        if self.outstanding() >= SE_RETURN_CAPACITY {
            self.stats.record_play_busy();
            return Err(SeError::Busy);
        }
        let command = SeCommand::Play {
            slot: slot as u16,
            sound,
            gain,
            policy,
        };
        match self.commands.push(command) {
            Ok(()) => {
                self.sent += 1;
                self.stats.record_handle_sent();
                Ok(())
            }
            // The command, with its `Arc`, is dropped here on the control
            // side.
            Err(PushError::Full(_)) => {
                self.stats.record_command_dropped();
                Err(SeError::QueueFull)
            }
        }
    }

    /// Fades out the sound that plays `slot`.
    pub fn stop(&mut self, slot: usize) -> Result<(), SeError> {
        self.check_slot(slot)?;
        self.send(SeCommand::Stop { slot: slot as u16 })
    }

    /// Fades out all sounds.
    pub fn stop_all(&mut self) -> Result<(), SeError> {
        self.send(SeCommand::StopAll)
    }

    /// Takes the `Arc`s the player gave back and drops them here, on the
    /// control side. Returns how many there were.
    ///
    /// [`SeController::play`] calls this itself, but call it from time to
    /// time (for example when the screen is redrawn) so that the sounds that
    /// were replaced or cleared are freed.
    pub fn collect(&mut self) -> usize {
        let mut count = 0;
        while let Ok(sound) = self.returns.pop() {
            drop(sound);
            self.received += 1;
            self.stats.record_handle_returned();
            count += 1;
        }
        count
    }

    fn send(&mut self, command: SeCommand) -> Result<(), SeError> {
        self.commands.push(command).map_err(|_| {
            self.stats.record_command_dropped();
            SeError::QueueFull
        })
    }

    fn check_slot(&self, slot: usize) -> Result<(), SeError> {
        if slot < SE_SLOTS {
            Ok(())
        } else {
            Err(SeError::SlotOutOfRange { slot })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::graph::{SE_COMMANDS_PER_BLOCK, SE_FADING_VOICES, SE_PLAYING_VOICES, SE_VOICES};

    const RATE: u32 = 48_000;
    const MAX_FRAMES: usize = 1024;

    fn channel() -> (SeController, SePlayer) {
        se_channel(RATE, MAX_FRAMES)
    }

    fn constant(frames: usize, level: f32) -> Arc<SoundData> {
        Arc::new(SoundData::new(
            RATE,
            vec![level; frames],
            vec![-level; frames],
        ))
    }

    /// Renders `total` frames in blocks that fit the player.
    fn run(player: &mut SePlayer, total: usize) {
        let mut left = total;
        while left > 0 {
            let frames = left.min(256);
            player.render(frames);
            left -= frames;
        }
    }

    #[test]
    fn a_new_controller_has_empty_slots() {
        let (control, _player) = channel();
        assert_eq!(control.sample_rate(), RATE);
        assert_eq!(control.total_bytes(), 0);
        assert_eq!(control.outstanding(), 0);
        assert!((0..SE_SLOTS).all(|slot| control.sound(slot).is_none()));
        assert!(control.sound(SE_SLOTS).is_none());
        assert_eq!(control.stats(), SeStatsSnapshot::default());
    }

    #[test]
    fn a_sound_goes_into_a_slot_and_out_again() {
        let (mut control, _player) = channel();
        let sound = constant(100, 0.5);
        control.set_sound(3, Arc::clone(&sound)).unwrap();
        assert!(Arc::ptr_eq(control.sound(3).unwrap(), &sound));
        assert_eq!(control.total_bytes(), 800);

        control.clear(3).unwrap();
        assert!(control.sound(3).is_none());
        assert_eq!(control.total_bytes(), 0);
        assert_eq!(Arc::strong_count(&sound), 1);
        // Clearing an empty slot is fine.
        control.clear(3).unwrap();
    }

    #[test]
    fn set_sound_refuses_what_does_not_fit() {
        let (mut control, _player) = channel();
        let sound = constant(100, 0.5);
        assert_eq!(
            control.set_sound(SE_SLOTS, Arc::clone(&sound)),
            Err(SeError::SlotOutOfRange { slot: SE_SLOTS })
        );
        assert_eq!(
            control.set_sound(0, Arc::new(SoundData::new(RATE, Vec::new(), Vec::new()))),
            Err(SeError::EmptySound)
        );
        let other_rate = Arc::new(SoundData::new(44_100, vec![0.0; 10], vec![0.0; 10]));
        assert_eq!(
            control.set_sound(0, other_rate),
            Err(SeError::RateMismatch {
                expected: RATE,
                actual: 44_100
            })
        );
        assert_eq!(control.total_bytes(), 0);
        assert!(control.sound(0).is_none());
    }

    #[test]
    fn the_total_of_the_sounds_is_limited_and_a_replaced_sound_does_not_count() {
        // A limit of 2000 bytes: 250 frames.
        let (mut control, _player) = channel_with_total_limit(RATE, MAX_FRAMES, 2000);
        control.set_sound(0, constant(100, 0.1)).unwrap();
        control.set_sound(1, constant(100, 0.1)).unwrap();
        assert_eq!(control.total_bytes(), 1600);

        assert_eq!(
            control.set_sound(2, constant(100, 0.1)),
            Err(SeError::TotalTooLarge {
                total_bytes: 2400,
                limit_bytes: 2000
            })
        );
        assert_eq!(control.total_bytes(), 1600);
        assert!(control.sound(2).is_none());

        // Replacing counts the new sound, not the old one.
        control.set_sound(0, constant(150, 0.1)).unwrap();
        assert_eq!(control.total_bytes(), 2000);
        control.clear(1).unwrap();
        assert_eq!(control.total_bytes(), 1200);
        control.set_sound(2, constant(100, 0.1)).unwrap();
        assert_eq!(control.total_bytes(), 2000);
    }

    #[test]
    fn the_real_limit_is_256_mib() {
        let (control, _player) = channel();
        assert_eq!(control.total_limit, 256 * 1024 * 1024);
    }

    #[test]
    fn play_needs_a_sound_in_the_slot_and_a_slot_that_exists() {
        let (mut control, _player) = channel();
        assert_eq!(
            control.play(5, 1.0, SePolicy::Overlap),
            Err(SeError::NoSound { slot: 5 })
        );
        assert_eq!(
            control.play(SE_SLOTS, 1.0, SePolicy::Overlap),
            Err(SeError::SlotOutOfRange { slot: SE_SLOTS })
        );
        assert_eq!(
            control.stop(SE_SLOTS),
            Err(SeError::SlotOutOfRange { slot: SE_SLOTS })
        );
        assert_eq!(control.outstanding(), 0);
        assert_eq!(control.stats(), SeStatsSnapshot::default());
    }

    #[test]
    fn a_played_sound_is_heard_and_comes_back_to_the_controller() {
        let (mut control, mut player) = channel();
        let sound = constant(100, 0.5);
        control.set_sound(2, Arc::clone(&sound)).unwrap();
        control.play(2, 0.5, SePolicy::Overlap).unwrap();
        assert_eq!(control.outstanding(), 1);
        assert_eq!(Arc::strong_count(&sound), 3, "test, slot, command");

        let out = player.render(64);
        assert!(out.channel(0).iter().all(|&sample| sample == 0.25));
        assert!(out.channel(1).iter().all(|&sample| sample == -0.25));
        assert_eq!(control.collect(), 0, "it is still playing");

        player.render(64);
        assert_eq!(control.collect(), 1);
        assert_eq!(control.outstanding(), 0);
        assert_eq!(Arc::strong_count(&sound), 2, "test and slot");
        let stats = control.stats();
        assert_eq!(
            (
                stats.plays_started,
                stats.handles_sent,
                stats.handles_returned,
                stats.leaked
            ),
            (1, 1, 1, 0)
        );
    }

    #[test]
    fn stop_and_stop_all_reach_the_player() {
        let (mut control, mut player) = channel();
        control.set_sound(0, constant(10_000, 1.0)).unwrap();
        control.set_sound(1, constant(10_000, 1.0)).unwrap();
        control.play(0, 1.0, SePolicy::Overlap).unwrap();
        control.play(1, 1.0, SePolicy::Overlap).unwrap();
        player.render(64);

        control.stop(0).unwrap();
        run(&mut player, 600);
        assert_eq!(player.active_voices(), 1);
        control.stop_all().unwrap();
        run(&mut player, 600);
        assert_eq!(player.active_voices(), 0);
        assert_eq!(control.collect(), 2);
    }

    #[test]
    fn a_sound_that_is_replaced_while_it_plays_goes_on_and_is_freed_here() {
        let (mut control, mut player) = channel();
        let old = constant(1000, 0.5);
        control.set_sound(0, Arc::clone(&old)).unwrap();
        control.play(0, 1.0, SePolicy::Overlap).unwrap();
        player.render(100);

        control.set_sound(0, constant(1000, 0.25)).unwrap();
        // The slot let go of the old sound, but the voice still has it.
        assert_eq!(Arc::strong_count(&old), 2, "test and voice");
        let out = player.render(100);
        assert!(out.channel(0).iter().all(|&sample| sample == 0.5));

        run(&mut player, 1000);
        assert_eq!(Arc::strong_count(&old), 2, "test and the return queue");
        assert_eq!(control.collect(), 1);
        assert_eq!(Arc::strong_count(&old), 1);
    }

    #[test]
    fn a_full_command_queue_is_refused_and_counted() {
        let (mut control, mut player) = channel();
        for _ in 0..SE_COMMAND_CAPACITY {
            control.stop_all().unwrap();
        }
        assert_eq!(control.stop_all(), Err(SeError::QueueFull));
        assert_eq!(control.stop(0), Err(SeError::QueueFull));
        assert_eq!(control.stats().commands_dropped, 2);

        // A refused play does not keep its `Arc` or count as sent.
        let sound = constant(100, 0.5);
        control.set_sound(0, Arc::clone(&sound)).unwrap();
        assert_eq!(
            control.play(0, 1.0, SePolicy::Overlap),
            Err(SeError::QueueFull)
        );
        assert_eq!(control.outstanding(), 0);
        assert_eq!(Arc::strong_count(&sound), 2, "test and slot");
        assert_eq!(control.stats().commands_dropped, 3);
        assert_eq!(control.stats().handles_sent, 0);

        // When the player has taken commands, sending works again.
        for _ in 0..SE_COMMAND_CAPACITY / SE_COMMANDS_PER_BLOCK {
            player.render(32);
        }
        control.play(0, 1.0, SePolicy::Overlap).unwrap();
    }

    /// Capacity of the return queue: with 32 `Arc`s in the voices and 256
    /// in the command queue (288 out), nothing is collected, the player
    /// takes everything, and every `Arc` fits into the return queue.
    #[test]
    fn the_worst_case_fills_the_return_queue_exactly_and_the_next_play_is_busy() {
        let (mut control, mut player) = channel();
        let sound = constant(1_000_000, 0.0001);
        control.set_sound(0, Arc::clone(&sound)).unwrap();

        // 32 voices in use: 16 fading and 16 playing.
        for _ in 0..SE_PLAYING_VOICES {
            control.play(0, 1.0, SePolicy::Overlap).unwrap();
        }
        player.render(32);
        control.stop_all().unwrap();
        player.render(32);
        for _ in 0..SE_PLAYING_VOICES {
            control.play(0, 1.0, SePolicy::Overlap).unwrap();
        }
        player.render(32);
        assert_eq!(player.active_voices(), SE_VOICES);
        assert_eq!(control.outstanding(), SE_VOICES);

        // The command queue is filled with plays, never collected: the
        // controller collects only what has come back, which is nothing.
        for _ in 0..SE_COMMAND_CAPACITY {
            control.play(0, 1.0, SePolicy::Overlap).unwrap();
        }
        assert_eq!(control.outstanding(), SE_RETURN_CAPACITY);
        assert_eq!(control.outstanding(), 288);
        assert_eq!(control.play(0, 1.0, SePolicy::Overlap), Err(SeError::Busy));
        assert_eq!(control.stats().plays_busy, 1);
        assert_eq!(control.outstanding(), 288, "a refused play sends nothing");

        // The player drops all 256 (the playing voices are full) and gives
        // them back. They all fit; nothing is lost.
        for _ in 0..SE_COMMAND_CAPACITY / SE_COMMANDS_PER_BLOCK {
            player.render(32);
        }
        assert_eq!(control.stats().leaked, 0);
        assert_eq!(control.stats().plays_dropped, SE_COMMAND_CAPACITY as u64);

        // After a collect, plays are accepted again.
        control.play(0, 1.0, SePolicy::Overlap).unwrap();
        assert_eq!(control.outstanding(), SE_VOICES + 1);

        control.stop_all().unwrap();
        run(&mut player, 2000);
        control.collect();
        assert_eq!(control.outstanding(), 0);
        let stats = control.stats();
        assert_eq!(stats.leaked, 0);
        assert_eq!(stats.handles_sent, stats.handles_returned);
        assert_eq!(Arc::strong_count(&sound), 2, "test and slot");
    }

    #[test]
    fn the_capacities_do_not_leave_room_for_an_overflow() {
        // The return queue holds every Arc that can be out.
        assert_eq!(
            SE_RETURN_CAPACITY,
            SE_PLAYING_VOICES + SE_FADING_VOICES + SE_COMMAND_CAPACITY
        );
        // The controller stops at that number.
        let (control, _player) = channel();
        assert!(control.returns.buffer().capacity() >= SE_RETURN_CAPACITY);
        assert!(control.commands.buffer().capacity() >= SE_COMMAND_CAPACITY);
    }

    /// The stream is closed while sounds play: the audio backend drops the
    /// player on its own thread. The sounds must not be freed there; they
    /// come back to the controller, which frees them with `collect()`.
    #[test]
    fn closing_the_player_on_the_audio_thread_hands_the_sounds_back() {
        let (mut control, mut player) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let audio = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    player.render(128);
                    thread::sleep(Duration::from_micros(20));
                }
                // `player` is dropped here, on this thread, as the backend
                // does when it closes the stream.
            })
        };

        // The slots hold the only strong references (besides the player).
        let mut weak = Vec::new();
        for slot in 0..5 {
            let sound = constant(1_000_000, 0.01);
            weak.push(Arc::downgrade(&sound));
            control.set_sound(slot, sound).unwrap();
            control.play(slot, 1.0, SePolicy::Overlap).unwrap();
        }
        // Wait until the player has started them all (or the test hangs).
        while control.stats().plays_started < 5 {
            thread::sleep(Duration::from_micros(100));
        }
        // Two more, which may still be waiting in the command queue when
        // the player is dropped.
        for slot in 5..7 {
            let sound = constant(1_000_000, 0.01);
            weak.push(Arc::downgrade(&sound));
            control.set_sound(slot, sound).unwrap();
            control.play(slot, 1.0, SePolicy::Overlap).unwrap();
        }
        for slot in 0..7 {
            control.clear(slot).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        audio.join().unwrap();

        // The player is gone, and nothing was freed on its thread: every
        // sound is still alive, held by the return queue.
        let sent = control.stats().handles_sent as usize;
        assert_eq!(sent, 7);
        assert_eq!(control.outstanding(), sent);
        let alive = weak.iter().filter(|weak| weak.upgrade().is_some()).count();
        assert_eq!(alive, sent, "the sounds that were played are alive");

        // The controller frees them.
        assert_eq!(control.collect(), sent);
        assert_eq!(control.outstanding(), 0);
        assert!(weak.iter().all(|weak| weak.upgrade().is_none()));
        let stats = control.stats();
        assert_eq!(stats.leaked, 0);
        assert_eq!(stats.handles_sent, stats.handles_returned);
    }

    /// A small random generator, so the tests are the same every time.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn pool() -> Vec<Arc<SoundData>> {
        [1, 50, 300, 480, 481, 1000, 3000, 20_000]
            .into_iter()
            .map(|frames| constant(frames, 0.01))
            .collect()
    }

    fn random_policy(rng: &mut Rng) -> SePolicy {
        [SePolicy::Overlap, SePolicy::Restart, SePolicy::Ignore][rng.below(3) as usize]
    }

    /// One random action of the control side.
    fn act(control: &mut SeController, rng: &mut Rng, pool: &[Arc<SoundData>]) {
        let slot = rng.below(8) as usize;
        match rng.below(20) {
            0..=11 => {
                let policy = random_policy(rng);
                let result = control.play(slot, 1.0, policy);
                assert!(
                    matches!(
                        result,
                        Ok(()) | Err(SeError::NoSound { .. } | SeError::Busy | SeError::QueueFull)
                    ),
                    "{result:?}"
                );
            }
            12..=13 => {
                let _ = control.stop(slot);
            }
            14 => {
                let _ = control.stop_all();
            }
            15..=17 => {
                let sound = Arc::clone(&pool[rng.below(pool.len() as u64) as usize]);
                control.set_sound(slot, sound).unwrap();
            }
            18 => {
                control.clear(slot).unwrap();
            }
            _ => {
                control.collect();
            }
        }
    }

    /// Random actions on the controller and thousands of blocks on the
    /// player (one thread). Every `Arc` comes back exactly once.
    #[test]
    fn every_arc_comes_back_exactly_once_with_random_actions() {
        for seed in [3_u64, 99, 2024, 555_555_555] {
            let (mut control, mut player) = channel();
            let pool = pool();
            let mut rng = Rng(seed);

            for _ in 0..6000 {
                for _ in 0..rng.below(6) {
                    act(&mut control, &mut rng, &pool);
                    assert!(control.outstanding() <= SE_RETURN_CAPACITY);
                }
                player.render(1 + rng.below(256) as usize);
                assert!(control.outstanding() <= SE_RETURN_CAPACITY);
            }

            let _ = control.stop_all();
            for _ in 0..20 {
                run(&mut player, 2000);
            }
            control.collect();

            let stats = control.stats();
            assert_eq!(player.active_voices(), 0, "seed {seed}");
            assert_eq!(control.outstanding(), 0, "seed {seed}");
            assert_eq!(stats.handles_sent, stats.handles_returned, "seed {seed}");
            assert_eq!(stats.leaked, 0, "seed {seed}");
            assert_eq!(
                stats.plays_started + stats.plays_ignored + stats.plays_dropped,
                stats.handles_sent,
                "seed {seed}"
            );
            // Only the slots and the pool still hold the sounds.
            for slot in 0..SE_SLOTS {
                control.clear(slot).unwrap();
            }
            for sound in &pool {
                assert_eq!(Arc::strong_count(sound), 1, "seed {seed}");
            }
        }
    }

    /// The same with a real audio thread: the player renders in a loop on
    /// its own thread while the controller plays, stops, replaces and
    /// collects.
    #[test]
    fn every_arc_comes_back_exactly_once_with_a_real_audio_thread() {
        let (mut control, mut player) = channel();
        let pool = pool();
        let mut rng = Rng(42);
        let stop = Arc::new(AtomicBool::new(false));

        let audio = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    player.render(128);
                    thread::sleep(Duration::from_micros(20));
                }
                player
            })
        };

        for round in 0..30_000_u32 {
            act(&mut control, &mut rng, &pool);
            assert!(control.outstanding() <= SE_RETURN_CAPACITY);
            if round % 64 == 0 {
                thread::yield_now();
            }
        }

        stop.store(true, Ordering::Relaxed);
        let mut player = audio.join().unwrap();
        let _ = control.stop_all();
        for _ in 0..20 {
            run(&mut player, 2000);
        }
        control.collect();

        let stats = control.stats();
        assert_eq!(player.active_voices(), 0);
        assert_eq!(control.outstanding(), 0);
        assert_eq!(stats.handles_sent, stats.handles_returned);
        assert_eq!(stats.leaked, 0);
        assert!(stats.handles_sent > 100, "the test played something");
        for slot in 0..SE_SLOTS {
            control.clear(slot).unwrap();
        }
        for sound in &pool {
            assert_eq!(Arc::strong_count(sound), 1);
        }
    }
}
