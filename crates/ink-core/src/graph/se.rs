//! What the two ends of the sound effects (SE) player share (ADR-0009): the
//! sizes, the commands, and the counters.
//!
//! The control side ([`SeController`](super::SeController)) sends commands
//! through an `rtrb` queue; the audio side ([`SePlayer`](super::SePlayer))
//! plays them and hands every `Arc` it was given back through a second
//! queue, so that the last reference to a sound is never dropped on the
//! audio thread (AGENTS.md §5).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::SoundData;

/// How many sounds can play at the same time. A play past this is dropped
/// and counted.
pub const SE_PLAYING_VOICES: usize = 16;

/// How many voices are fading out (after a stop or a restart) besides the
/// playing ones. A voice that fades out does not count as playing.
pub const SE_FADING_VOICES: usize = 16;

/// The number of voices there are: the playing ones and the fading ones.
pub const SE_VOICES: usize = SE_PLAYING_VOICES + SE_FADING_VOICES;

/// The number of places for sounds on the control side (the sound pad).
pub const SE_SLOTS: usize = 64;

/// The capacity of the command queue.
pub const SE_COMMAND_CAPACITY: usize = 256;

/// The most commands the audio side takes at the start of a block.
pub const SE_COMMANDS_PER_BLOCK: usize = 64;

/// The capacity of the return queue: every voice and every command in the
/// queue can hold one `Arc`. It is larger than the number of voices, and
/// the control side never lets more `Arc`s be out than this, so the audio
/// thread never finds the return queue full.
pub const SE_RETURN_CAPACITY: usize = SE_VOICES + SE_COMMAND_CAPACITY;

/// How long a voice takes to fade out, in milliseconds.
pub const SE_FADE_MS: u32 = 10;

/// The most memory the sounds in the slots may take together, in bytes.
pub const SE_TOTAL_BYTES_LIMIT: usize = 256 * 1024 * 1024;

// The sizes must agree with each other (ADR-0009).
const _: () = assert!(SE_RETURN_CAPACITY >= SE_VOICES + SE_COMMAND_CAPACITY);
const _: () = assert!(SE_RETURN_CAPACITY >= SE_PLAYING_VOICES);
const _: () = assert!(SE_COMMANDS_PER_BLOCK <= SE_COMMAND_CAPACITY);
const _: () = assert!(SE_SLOTS <= u16::MAX as usize);

/// What to do when a sound is played while the same slot is already playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SePolicy {
    /// Play it again on top of the one that is playing.
    #[default]
    Overlap,
    /// Fade the one that is playing out and start again from the beginning.
    Restart,
    /// Do nothing: the one that is playing goes on.
    Ignore,
}

/// A command for the audio side.
///
/// `Play` carries an `Arc` to the sound. The audio side gives every `Arc` it
/// receives back through the return queue, exactly once: when the sound has
/// played to its end, when it has faded out, or at once if it was not played.
#[derive(Debug)]
pub enum SeCommand {
    /// Play `sound` (taken from slot `slot`) at the linear gain `gain`.
    Play {
        slot: u16,
        sound: Arc<SoundData>,
        gain: f32,
        policy: SePolicy,
    },
    /// Fade out the voices that play slot `slot`.
    Stop { slot: u16 },
    /// Fade out all voices.
    StopAll,
}

/// Counters of the SE player, shared by its two ends. All are atomics with
/// `Ordering::Relaxed`: each counter stands on its own.
///
/// The `record_*` functions are for the two ends of the player; each counter
/// is written by one end only.
#[derive(Debug, Default)]
pub struct SeStats {
    // Written by the audio side.
    plays_started: AtomicU64,
    plays_ignored: AtomicU64,
    plays_dropped: AtomicU64,
    leaked: AtomicU64,
    // Written by the control side.
    commands_dropped: AtomicU64,
    plays_busy: AtomicU64,
    handles_sent: AtomicU64,
    handles_returned: AtomicU64,
}

/// The values of [`SeStats`] at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SeStatsSnapshot {
    /// Plays that started a voice.
    pub plays_started: u64,
    /// Plays not played because of the policy (`Ignore`).
    pub plays_ignored: u64,
    /// Plays not played because all playing voices were in use.
    pub plays_dropped: u64,
    /// `Arc`s the audio side could not hand back (the return queue was
    /// full) and kept instead of dropping. This must stay 0.
    pub leaked: u64,
    /// Commands the control side could not send because the command queue
    /// was full.
    pub commands_dropped: u64,
    /// Plays the control side refused because too many `Arc`s were out.
    pub plays_busy: u64,
    /// `Arc`s sent to the audio side.
    pub handles_sent: u64,
    /// `Arc`s received back from the audio side.
    pub handles_returned: u64,
}

impl SeStats {
    /// Creates counters that are all 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns all counters. They are read one after the other, so the
    /// numbers can be a block apart.
    pub fn snapshot(&self) -> SeStatsSnapshot {
        SeStatsSnapshot {
            plays_started: self.plays_started.load(Ordering::Relaxed),
            plays_ignored: self.plays_ignored.load(Ordering::Relaxed),
            plays_dropped: self.plays_dropped.load(Ordering::Relaxed),
            leaked: self.leaked.load(Ordering::Relaxed),
            commands_dropped: self.commands_dropped.load(Ordering::Relaxed),
            plays_busy: self.plays_busy.load(Ordering::Relaxed),
            handles_sent: self.handles_sent.load(Ordering::Relaxed),
            handles_returned: self.handles_returned.load(Ordering::Relaxed),
        }
    }

    /// Audio side: a play started a voice.
    pub fn record_play_started(&self) {
        self.plays_started.fetch_add(1, Ordering::Relaxed);
    }

    /// Audio side: a play was not played because of its policy.
    pub fn record_play_ignored(&self) {
        self.plays_ignored.fetch_add(1, Ordering::Relaxed);
    }

    /// Audio side: a play was not played because no voice was free.
    pub fn record_play_dropped(&self) {
        self.plays_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Audio side: an `Arc` could not be handed back and was kept.
    pub fn record_leaked(&self) {
        self.leaked.fetch_add(1, Ordering::Relaxed);
    }

    /// Control side: a command could not be sent (the queue was full).
    pub fn record_command_dropped(&self) {
        self.commands_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Control side: a play was refused (too many `Arc`s were out).
    pub fn record_play_busy(&self) {
        self.plays_busy.fetch_add(1, Ordering::Relaxed);
    }

    /// Control side: an `Arc` was sent.
    pub fn record_handle_sent(&self) {
        self.handles_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Control side: an `Arc` came back.
    pub fn record_handle_returned(&self) {
        self.handles_returned.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sizes_are_those_of_adr_0009() {
        assert_eq!(SE_PLAYING_VOICES, 16);
        assert_eq!(SE_VOICES, 32);
        assert_eq!(SE_SLOTS, 64);
        assert_eq!(SE_COMMAND_CAPACITY, 256);
        assert_eq!(SE_COMMANDS_PER_BLOCK, 64);
        assert_eq!(SE_FADE_MS, 10);
        assert_eq!(SE_TOTAL_BYTES_LIMIT, 268_435_456);
    }

    #[test]
    fn the_return_queue_holds_the_voices_and_the_commands() {
        // The relations between the sizes are checked at compile time above.
        assert_eq!(SE_RETURN_CAPACITY, 32 + 256);
        assert_eq!(SE_RETURN_CAPACITY, 288);
    }

    #[test]
    fn a_command_is_small() {
        // 256 of them are in the queue.
        assert!(size_of::<SeCommand>() <= 32, "{}", size_of::<SeCommand>());
    }

    #[test]
    fn commands_and_counters_can_cross_threads() {
        fn assert_send<T: Send>() {}
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send::<SeCommand>();
        assert_send_sync::<SeStats>();
    }

    #[test]
    fn the_default_policy_overlaps() {
        assert_eq!(SePolicy::default(), SePolicy::Overlap);
    }

    #[test]
    fn each_counter_counts_by_itself() {
        let stats = SeStats::new();
        assert_eq!(stats.snapshot(), SeStatsSnapshot::default());

        stats.record_play_started();
        stats.record_play_started();
        stats.record_play_ignored();
        stats.record_play_dropped();
        stats.record_leaked();
        stats.record_command_dropped();
        stats.record_play_busy();
        stats.record_handle_sent();
        stats.record_handle_sent();
        stats.record_handle_sent();
        stats.record_handle_returned();

        assert_eq!(
            stats.snapshot(),
            SeStatsSnapshot {
                plays_started: 2,
                plays_ignored: 1,
                plays_dropped: 1,
                leaked: 1,
                commands_dropped: 1,
                plays_busy: 1,
                handles_sent: 3,
                handles_returned: 1,
            }
        );
    }
}
