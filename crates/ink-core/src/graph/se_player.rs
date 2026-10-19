//! The audio side of the sound effects (SE) player (ADR-0009).
//!
//! [`SePlayer`] runs on the audio thread. It takes [`SeCommand`]s from an
//! `rtrb` queue at the start of a block, plays them with a fixed number of
//! voices, and gives every `Arc<SoundData>` it was sent back through a
//! second queue, so the last reference to a sound is never dropped here.
//!
//! Nothing in [`SePlayer::render`] allocates, locks, logs, or does I/O
//! (AGENTS.md §5): the voices and the mix buffer are made in [`SePlayer::new`].
//!
//! # Voices
//!
//! There are [`SE_VOICES`] voices: up to [`SE_PLAYING_VOICES`] playing and
//! up to [`SE_FADING_VOICES`] fading out. A voice fades out (for
//! [`SE_FADE_MS`]) when it is stopped or restarted, so no click is heard.
//!
//! - A play that finds all playing voices in use is dropped (and counted).
//!   Playing voices are never taken over, because that would click.
//! - A voice that has to start fading when all fading voices are in use
//!   makes room by cutting the fading voice that has the least left of its
//!   fade (the oldest one). It is nearly silent already, so the cut is the
//!   smallest click there can be, and it keeps the fading voices at a fixed
//!   number. Its `Arc` is given back like any other.
//!
//! # Closing
//!
//! When the stream is closed, the player is dropped by the audio backend,
//! on its worker thread. Dropping it gives back the `Arc`s of the voices and
//! of the commands still in the queue, so the sounds are freed later on the
//! control side, not there.

use std::mem;
use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError};

use super::{
    AudioBuffer, SE_COMMANDS_PER_BLOCK, SE_FADE_MS, SE_FADING_VOICES, SE_PLAYING_VOICES, SE_VOICES,
    SeCommand, SePolicy, SeStats, SoundData,
};

/// What a voice is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceState {
    /// Not used.
    Idle,
    /// Playing at full level until the end of the sound.
    Playing,
    /// Fading out.
    Fading,
}

/// One voice. It holds the `Arc` of the sound it plays until it ends.
#[derive(Debug)]
struct Voice {
    state: VoiceState,
    sound: Option<Arc<SoundData>>,
    slot: u16,
    gain: f32,
    /// The next frame of the sound to play.
    pos: usize,
    /// The frames the fade has left (only while fading).
    fade_left: usize,
}

impl Voice {
    fn idle() -> Self {
        Self {
            state: VoiceState::Idle,
            sound: None,
            slot: 0,
            gain: 0.0,
            pos: 0,
            fade_left: 0,
        }
    }
}

/// The audio side of the SE player: commands in, a stereo mix out.
#[derive(Debug)]
pub struct SePlayer {
    commands: Consumer<SeCommand>,
    returns: Producer<Arc<SoundData>>,
    stats: Arc<SeStats>,
    voices: [Voice; SE_VOICES],
    mix: AudioBuffer,
    /// How many frames a fade lasts.
    fade_frames: usize,
    /// `1 / fade_frames`.
    fade_step: f32,
}

impl SePlayer {
    /// Creates a player that plays sounds at `sample_rate` Hz and renders at
    /// most `max_frames` frames at a time.
    ///
    /// Everything is allocated here. The sounds sent to the player must
    /// already be at `sample_rate` (the control side checks this).
    pub fn new(
        commands: Consumer<SeCommand>,
        returns: Producer<Arc<SoundData>>,
        stats: Arc<SeStats>,
        sample_rate: u32,
        max_frames: usize,
    ) -> Self {
        let fade_frames = ((u64::from(sample_rate) * u64::from(SE_FADE_MS)) / 1000).max(1) as usize;
        Self {
            commands,
            returns,
            stats,
            voices: std::array::from_fn(|_| Voice::idle()),
            mix: AudioBuffer::new(2, max_frames),
            fade_frames,
            fade_step: 1.0 / fade_frames as f32,
        }
    }

    /// Returns the most frames [`SePlayer::render`] can render at a time (the
    /// `max_frames` given to [`SePlayer::new`]).
    pub fn max_frames(&self) -> usize {
        self.mix.max_frames()
    }

    /// Returns how many voices are in use (playing or fading).
    pub fn active_voices(&self) -> usize {
        self.voices
            .iter()
            .filter(|voice| voice.state != VoiceState::Idle)
            .count()
    }

    /// Takes the waiting commands, then mixes all voices into a stereo
    /// buffer of `frames` frames and returns it.
    ///
    /// # Panics
    ///
    /// Panics if `frames` is more than the `max_frames` given to
    /// [`SePlayer::new`].
    pub fn render(&mut self, frames: usize) -> &AudioBuffer {
        self.take_commands();
        self.mix.set_frames(frames);
        for channel in 0..2 {
            self.mix.channel_mut(channel).fill(0.0);
        }
        for index in 0..SE_VOICES {
            self.mix_voice(index, frames);
        }
        &self.mix
    }

    /// Takes up to [`SE_COMMANDS_PER_BLOCK`] commands. The rest wait for the
    /// next block, so a burst of commands cannot make one block too long.
    fn take_commands(&mut self) {
        for _ in 0..SE_COMMANDS_PER_BLOCK {
            let Ok(command) = self.commands.pop() else {
                break;
            };
            self.handle(command);
        }
    }

    fn handle(&mut self, command: SeCommand) {
        match command {
            SeCommand::Play {
                slot,
                sound,
                gain,
                policy,
            } => self.play(slot, sound, gain, policy),
            SeCommand::Stop { slot } => self.fade_playing(Some(slot)),
            SeCommand::StopAll => self.fade_playing(None),
        }
    }

    fn play(&mut self, slot: u16, sound: Arc<SoundData>, gain: f32, policy: SePolicy) {
        // A sound with no frames has nothing to play.
        if sound.is_empty() {
            self.stats.record_play_ignored();
            self.give_back(sound);
            return;
        }
        match policy {
            SePolicy::Overlap => {}
            SePolicy::Restart => self.fade_playing(Some(slot)),
            SePolicy::Ignore => {
                if self
                    .voices
                    .iter()
                    .any(|voice| voice.state == VoiceState::Playing && voice.slot == slot)
                {
                    self.stats.record_play_ignored();
                    self.give_back(sound);
                    return;
                }
            }
        }
        let playing = self
            .voices
            .iter()
            .filter(|voice| voice.state == VoiceState::Playing)
            .count();
        let free = self
            .voices
            .iter()
            .position(|voice| voice.state == VoiceState::Idle);
        // With at most SE_PLAYING_VOICES playing and SE_FADING_VOICES
        // fading, a free voice exists whenever fewer than SE_PLAYING_VOICES
        // are playing. If that were ever wrong, the play is dropped.
        let Some(index) = free.filter(|_| playing < SE_PLAYING_VOICES) else {
            self.stats.record_play_dropped();
            self.give_back(sound);
            return;
        };
        // NaN and negative gains become silence.
        let gain = gain.max(0.0);
        self.voices[index] = Voice {
            state: VoiceState::Playing,
            sound: Some(sound),
            slot,
            gain,
            pos: 0,
            fade_left: 0,
        };
        self.stats.record_play_started();
    }

    /// Starts the fade of the playing voices of `slot` (of all playing
    /// voices if `slot` is `None`).
    fn fade_playing(&mut self, slot: Option<u16>) {
        for index in 0..SE_VOICES {
            let voice = &self.voices[index];
            if voice.state == VoiceState::Playing && slot.is_none_or(|slot| slot == voice.slot) {
                self.begin_fade(index);
            }
        }
    }

    /// Makes voice `index` (which is playing) fade out. If all fading voices
    /// are in use, the one with the least fade left is cut first.
    fn begin_fade(&mut self, index: usize) {
        let fading = self
            .voices
            .iter()
            .filter(|voice| voice.state == VoiceState::Fading)
            .count();
        if fading >= SE_FADING_VOICES {
            let oldest = self
                .voices
                .iter()
                .enumerate()
                .filter(|(_, voice)| voice.state == VoiceState::Fading)
                .min_by_key(|(_, voice)| voice.fade_left)
                .map(|(oldest, _)| oldest);
            if let Some(oldest) = oldest {
                self.finish(oldest);
            }
        }
        let voice = &mut self.voices[index];
        voice.state = VoiceState::Fading;
        voice.fade_left = self.fade_frames;
    }

    /// Adds the next `frames` frames of voice `index` to the mix.
    fn mix_voice(&mut self, index: usize, frames: usize) {
        let voice = &mut self.voices[index];
        let Some(sound) = voice.sound.as_ref() else {
            return;
        };
        let mut count = frames.min(sound.frames() - voice.pos);
        let ended = match voice.state {
            VoiceState::Idle => return,
            VoiceState::Playing => {
                for channel in 0..2 {
                    let out = &mut self.mix.channel_mut(channel)[..count];
                    let source = &sound.channel(channel)[voice.pos..voice.pos + count];
                    for (out, sample) in out.iter_mut().zip(source) {
                        *out += sample * voice.gain;
                    }
                }
                voice.pos += count;
                voice.pos >= sound.frames()
            }
            VoiceState::Fading => {
                count = count.min(voice.fade_left);
                for channel in 0..2 {
                    let out = &mut self.mix.channel_mut(channel)[..count];
                    let source = &sound.channel(channel)[voice.pos..voice.pos + count];
                    for (frame, (out, sample)) in out.iter_mut().zip(source).enumerate() {
                        let level = (voice.fade_left - frame) as f32 * self.fade_step;
                        *out += sample * voice.gain * level;
                    }
                }
                voice.pos += count;
                voice.fade_left -= count;
                voice.fade_left == 0 || voice.pos >= sound.frames()
            }
        };
        if ended {
            self.finish(index);
        }
    }

    /// Frees voice `index` and gives its `Arc` back.
    fn finish(&mut self, index: usize) {
        let voice = &mut self.voices[index];
        voice.state = VoiceState::Idle;
        if let Some(sound) = voice.sound.take() {
            self.give_back(sound);
        }
    }

    /// Gives an `Arc` back to the control side.
    ///
    /// The return queue cannot be full if the control side keeps its count
    /// of the `Arc`s that are out below the capacity (ADR-0009). If it is
    /// full anyway, the `Arc` is forgotten and counted, because dropping it
    /// here could free the sound on the audio thread. That is a leak, so
    /// `leaked` must stay 0.
    fn give_back(&mut self, sound: Arc<SoundData>) {
        if let Err(PushError::Full(sound)) = self.returns.push(sound) {
            self.stats.record_leaked();
            mem::forget(sound);
        }
    }
}

/// Gives back every `Arc` the player still holds: those of the voices (also
/// the ones that are playing) and those of the `Play` commands the audio side
/// has not taken yet. This runs when the stream is closed, on the thread of
/// the audio backend, and keeps the last reference of a sound from being
/// dropped there.
impl Drop for SePlayer {
    fn drop(&mut self) {
        for index in 0..SE_VOICES {
            self.finish(index);
        }
        while let Ok(command) = self.commands.pop() {
            if let SeCommand::Play { sound, .. } = command {
                self.give_back(sound);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rtrb::RingBuffer;

    use super::*;
    use crate::graph::{SE_COMMAND_CAPACITY, SE_RETURN_CAPACITY};

    const RATE: u32 = 48_000;
    const MAX_FRAMES: usize = 1024;
    /// The frames a fade lasts at `RATE`.
    const FADE: usize = 480;

    struct Rig {
        commands: Producer<SeCommand>,
        returned: Consumer<Arc<SoundData>>,
        player: SePlayer,
        stats: Arc<SeStats>,
    }

    fn rig() -> Rig {
        rig_with_return_capacity(SE_RETURN_CAPACITY)
    }

    fn rig_with_return_capacity(return_capacity: usize) -> Rig {
        let (commands, command_consumer) = RingBuffer::new(SE_COMMAND_CAPACITY);
        let (return_producer, returned) = RingBuffer::new(return_capacity);
        let stats = Arc::new(SeStats::new());
        let player = SePlayer::new(
            command_consumer,
            return_producer,
            Arc::clone(&stats),
            RATE,
            MAX_FRAMES,
        );
        Rig {
            commands,
            returned,
            player,
            stats,
        }
    }

    /// A sound whose left channel is `level` and right channel is `-level`
    /// all the way, so the output shows what was played.
    fn constant(frames: usize, level: f32) -> Arc<SoundData> {
        Arc::new(SoundData::new(
            RATE,
            vec![level; frames],
            vec![-level; frames],
        ))
    }

    fn send_play(rig: &mut Rig, slot: u16, sound: &Arc<SoundData>, gain: f32, policy: SePolicy) {
        rig.commands
            .push(SeCommand::Play {
                slot,
                sound: Arc::clone(sound),
                gain,
                policy,
            })
            .unwrap();
    }

    fn send(rig: &mut Rig, command: SeCommand) {
        rig.commands.push(command).unwrap();
    }

    fn render(rig: &mut Rig, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let out = rig.player.render(frames);
        (out.channel(0).to_vec(), out.channel(1).to_vec())
    }

    /// Renders `total` frames in blocks that fit the player.
    fn run(rig: &mut Rig, total: usize) {
        let mut left = total;
        while left > 0 {
            let frames = left.min(256);
            rig.player.render(frames);
            left -= frames;
        }
    }

    /// Takes everything out of the return queue.
    fn collect(rig: &mut Rig) -> Vec<Arc<SoundData>> {
        let mut all = Vec::new();
        while let Ok(sound) = rig.returned.pop() {
            all.push(sound);
        }
        all
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-5,
            "{actual} is not {expected}"
        );
    }

    #[test]
    fn it_reports_the_max_frames_it_was_made_with() {
        let rig = rig();
        assert_eq!(rig.player.max_frames(), MAX_FRAMES);
    }

    #[test]
    fn without_voices_it_renders_silence() {
        let mut rig = rig();
        let (left, right) = render(&mut rig, 480);
        assert_eq!(left.len(), 480);
        assert_eq!(right.len(), 480);
        assert!(left.iter().chain(&right).all(|&sample| sample == 0.0));
        assert_eq!(rig.player.active_voices(), 0);
    }

    #[test]
    fn a_sound_plays_to_its_end_and_is_given_back_once() {
        let mut rig = rig();
        let sound = constant(100, 0.5);
        send_play(&mut rig, 3, &sound, 0.5, SePolicy::Overlap);

        let (left, right) = render(&mut rig, 64);
        assert!(left.iter().all(|&sample| sample == 0.25));
        assert!(right.iter().all(|&sample| sample == -0.25));
        assert!(collect(&mut rig).is_empty(), "it is still playing");

        let (left, _) = render(&mut rig, 64);
        assert!(left[..36].iter().all(|&sample| sample == 0.25));
        assert!(left[36..].iter().all(|&sample| sample == 0.0));
        assert_eq!(rig.player.active_voices(), 0);

        let back = collect(&mut rig);
        assert_eq!(back.len(), 1);
        assert!(Arc::ptr_eq(&back[0], &sound));
        let (left, _) = render(&mut rig, 64);
        assert!(left.iter().all(|&sample| sample == 0.0));
        assert!(collect(&mut rig).is_empty(), "only once");
        assert_eq!(rig.stats.snapshot().plays_started, 1);
    }

    #[test]
    fn sounds_play_on_top_of_each_other() {
        let mut rig = rig();
        let a = constant(1000, 0.25);
        let b = constant(1000, 0.125);
        send_play(&mut rig, 0, &a, 1.0, SePolicy::Overlap);
        send_play(&mut rig, 1, &b, 1.0, SePolicy::Overlap);
        send_play(&mut rig, 0, &a, 1.0, SePolicy::Overlap);

        let (left, right) = render(&mut rig, 32);
        assert!(left.iter().all(|&sample| sample == 0.625));
        assert!(right.iter().all(|&sample| sample == -0.625));
        assert_eq!(rig.player.active_voices(), 3);
    }

    #[test]
    fn a_bad_gain_is_silence() {
        let mut rig = rig();
        let sound = constant(100, 1.0);
        send_play(&mut rig, 0, &sound, f32::NAN, SePolicy::Overlap);
        send_play(&mut rig, 1, &sound, -1.0, SePolicy::Overlap);
        let (left, _) = render(&mut rig, 64);
        assert!(left.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn a_sound_with_no_frames_is_given_back_at_once() {
        let mut rig = rig();
        let empty = Arc::new(SoundData::new(RATE, Vec::new(), Vec::new()));
        send_play(&mut rig, 0, &empty, 1.0, SePolicy::Overlap);
        render(&mut rig, 64);
        assert_eq!(rig.player.active_voices(), 0);
        let back = collect(&mut rig);
        assert_eq!(back.len(), 1);
        assert!(Arc::ptr_eq(&back[0], &empty));
        assert_eq!(rig.stats.snapshot().plays_ignored, 1);
    }

    #[test]
    fn ignore_leaves_the_playing_sound_alone_and_gives_the_new_one_back() {
        let mut rig = rig();
        let first = constant(1000, 0.5);
        let second = constant(1000, 0.25);
        send_play(&mut rig, 7, &first, 1.0, SePolicy::Overlap);
        render(&mut rig, 64);
        send_play(&mut rig, 7, &second, 1.0, SePolicy::Ignore);
        let (left, _) = render(&mut rig, 64);

        assert!(left.iter().all(|&sample| sample == 0.5));
        let back = collect(&mut rig);
        assert_eq!(back.len(), 1);
        assert!(Arc::ptr_eq(&back[0], &second));
        let stats = rig.stats.snapshot();
        assert_eq!((stats.plays_started, stats.plays_ignored), (1, 1));
    }

    #[test]
    fn ignore_plays_when_the_slot_is_not_playing() {
        let mut rig = rig();
        let a = constant(1000, 0.5);
        let b = constant(1000, 0.25);
        send_play(&mut rig, 1, &a, 1.0, SePolicy::Overlap);
        send_play(&mut rig, 2, &b, 1.0, SePolicy::Ignore);
        let (left, _) = render(&mut rig, 32);
        assert!(left.iter().all(|&sample| sample == 0.75));
        assert_eq!(rig.stats.snapshot().plays_ignored, 0);
    }

    #[test]
    fn restart_fades_the_old_one_out_and_starts_the_new_one() {
        let mut rig = rig();
        let old = constant(10_000, 1.0);
        let new = constant(10_000, 1.0);
        send_play(&mut rig, 4, &old, 1.0, SePolicy::Overlap);
        render(&mut rig, 64);
        send_play(&mut rig, 4, &new, 1.0, SePolicy::Restart);

        // The new one is at full level; the old one goes from 1 to 1/480.
        let (left, _) = render(&mut rig, FADE);
        assert_close(left[0], 2.0);
        assert_close(left[240], 1.0 + 240.0 / 480.0);
        assert_close(left[FADE - 1], 1.0 + 1.0 / 480.0);
        let back = collect(&mut rig);
        assert_eq!(back.len(), 1, "the old one is given back after its fade");
        assert!(Arc::ptr_eq(&back[0], &old));

        let (left, _) = render(&mut rig, 16);
        assert!(left.iter().all(|&sample| sample == 1.0));
        let stats = rig.stats.snapshot();
        assert_eq!(stats.plays_started, 2);
        assert_eq!(rig.player.active_voices(), 1);
    }

    #[test]
    fn restart_on_an_idle_slot_just_plays() {
        let mut rig = rig();
        let sound = constant(1000, 0.5);
        send_play(&mut rig, 4, &sound, 1.0, SePolicy::Restart);
        let (left, _) = render(&mut rig, 32);
        assert!(left.iter().all(|&sample| sample == 0.5));
        assert!(collect(&mut rig).is_empty());
    }

    #[test]
    fn stop_fades_the_slot_out_and_gives_the_sound_back() {
        let mut rig = rig();
        let stopped = constant(10_000, 1.0);
        let other = constant(10_000, 0.5);
        send_play(&mut rig, 1, &stopped, 1.0, SePolicy::Overlap);
        send_play(&mut rig, 2, &other, 1.0, SePolicy::Overlap);
        render(&mut rig, 64);
        send(&mut rig, SeCommand::Stop { slot: 1 });

        let (left, _) = render(&mut rig, FADE);
        assert_close(left[0], 0.5 + 1.0);
        assert_close(left[240], 0.5 + 240.0 / 480.0);
        assert_close(left[FADE - 1], 0.5 + 1.0 / 480.0);
        let back = collect(&mut rig);
        assert_eq!(back.len(), 1);
        assert!(Arc::ptr_eq(&back[0], &stopped));

        let (left, _) = render(&mut rig, 16);
        assert!(left.iter().all(|&sample| sample == 0.5));
    }

    #[test]
    fn stopping_a_slot_that_is_not_playing_does_nothing() {
        let mut rig = rig();
        let sound = constant(1000, 0.5);
        send_play(&mut rig, 1, &sound, 1.0, SePolicy::Overlap);
        send(&mut rig, SeCommand::Stop { slot: 9 });
        let (left, _) = render(&mut rig, 64);
        assert!(left.iter().all(|&sample| sample == 0.5));
        assert!(collect(&mut rig).is_empty());
    }

    #[test]
    fn a_fade_can_be_split_over_blocks() {
        let mut rig = rig();
        let sound = constant(10_000, 1.0);
        send_play(&mut rig, 0, &sound, 1.0, SePolicy::Overlap);
        render(&mut rig, 64);
        send(&mut rig, SeCommand::Stop { slot: 0 });

        let mut all = Vec::new();
        for _ in 0..FADE / 100 + 1 {
            all.extend(render(&mut rig, 100).0);
        }
        for (frame, &sample) in all.iter().take(FADE).enumerate() {
            assert_close(sample, (FADE - frame) as f32 / FADE as f32);
        }
        assert!(all[FADE..].iter().all(|&sample| sample == 0.0));
        assert_eq!(collect(&mut rig).len(), 1);
    }

    #[test]
    fn stop_all_fades_every_voice_out() {
        let mut rig = rig();
        let sounds: Vec<_> = (0..5).map(|_| constant(10_000, 0.1)).collect();
        for (slot, sound) in sounds.iter().enumerate() {
            send_play(&mut rig, slot as u16, sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 64);
        send(&mut rig, SeCommand::StopAll);
        render(&mut rig, FADE);
        let (left, _) = render(&mut rig, 64);
        assert!(left.iter().all(|&sample| sample == 0.0));
        assert_eq!(rig.player.active_voices(), 0);
        assert_eq!(collect(&mut rig).len(), 5);
    }

    #[test]
    fn a_sound_that_ends_during_its_fade_is_given_back_once() {
        let mut rig = rig();
        let sound = constant(100, 1.0);
        send_play(&mut rig, 0, &sound, 1.0, SePolicy::Overlap);
        render(&mut rig, 64);
        send(&mut rig, SeCommand::Stop { slot: 0 });
        render(&mut rig, 64);
        assert_eq!(rig.player.active_voices(), 0);
        assert_eq!(collect(&mut rig).len(), 1);
        render(&mut rig, FADE);
        assert!(collect(&mut rig).is_empty());
    }

    #[test]
    fn the_seventeenth_sound_is_dropped_and_given_back() {
        let mut rig = rig();
        let sounds: Vec<_> = (0..=SE_PLAYING_VOICES)
            .map(|_| constant(10_000, 0.01))
            .collect();
        for sound in &sounds {
            send_play(&mut rig, 0, sound, 1.0, SePolicy::Overlap);
        }
        let (left, _) = render(&mut rig, 32);
        assert_close(left[0], 0.16);
        assert_eq!(rig.player.active_voices(), SE_PLAYING_VOICES);

        let back = collect(&mut rig);
        assert_eq!(back.len(), 1);
        assert!(Arc::ptr_eq(&back[0], &sounds[SE_PLAYING_VOICES]));
        let stats = rig.stats.snapshot();
        assert_eq!((stats.plays_started, stats.plays_dropped), (16, 1));
        assert_eq!(stats.leaked, 0);
    }

    #[test]
    fn a_fading_voice_does_not_count_as_playing() {
        let mut rig = rig();
        let sounds: Vec<_> = (0..SE_PLAYING_VOICES)
            .map(|_| constant(10_000, 0.01))
            .collect();
        for sound in &sounds {
            send_play(&mut rig, 0, sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 32);
        send(&mut rig, SeCommand::StopAll);
        let extra = constant(10_000, 0.5);
        send_play(&mut rig, 1, &extra, 1.0, SePolicy::Overlap);
        render(&mut rig, 32);

        // 16 fading and the new one playing: 17 voices in use.
        assert_eq!(rig.player.active_voices(), SE_PLAYING_VOICES + 1);
        assert_eq!(rig.stats.snapshot().plays_dropped, 0);
    }

    /// All fading voices are in use and more have to start fading. The
    /// fading voices that have the least left are cut, and every `Arc` is
    /// given back exactly once.
    #[test]
    fn when_the_fading_voices_are_full_stop_all_cuts_the_oldest_fading_ones() {
        let mut rig = rig();
        let first: Vec<_> = (0..SE_PLAYING_VOICES)
            .map(|_| constant(100_000, 0.01))
            .collect();
        let second: Vec<_> = (0..SE_PLAYING_VOICES)
            .map(|_| constant(100_000, 0.01))
            .collect();

        for (slot, sound) in first.iter().enumerate() {
            send_play(&mut rig, slot as u16, sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 64);
        send(&mut rig, SeCommand::StopAll);
        render(&mut rig, 64);
        // 16 fading, 416 frames left each.
        assert_eq!(rig.player.active_voices(), SE_FADING_VOICES);
        assert!(collect(&mut rig).is_empty());

        for (slot, sound) in second.iter().enumerate() {
            send_play(&mut rig, slot as u16, sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 64);
        send(&mut rig, SeCommand::StopAll);
        let (left, _) = render(&mut rig, 64);
        assert!(left.iter().all(|sample| sample.is_finite()));

        // All 16 of the first group were cut for the second group.
        let back = collect(&mut rig);
        assert_eq!(back.len(), SE_PLAYING_VOICES);
        for sound in &back {
            assert!(first.iter().any(|first| Arc::ptr_eq(first, sound)));
        }
        drop(back);
        assert_eq!(rig.player.active_voices(), SE_FADING_VOICES);

        run(&mut rig, 4 * FADE);
        let back = collect(&mut rig);
        assert_eq!(back.len(), SE_PLAYING_VOICES);
        for sound in &back {
            assert!(second.iter().any(|second| Arc::ptr_eq(second, sound)));
        }
        drop(back);
        assert_eq!(rig.player.active_voices(), 0);
        let stats = rig.stats.snapshot();
        assert_eq!((stats.plays_started, stats.leaked), (32, 0));
        for sound in first.iter().chain(&second) {
            assert_eq!(Arc::strong_count(sound), 1, "every Arc came back");
        }
    }

    #[test]
    fn when_the_fading_voices_are_full_restart_cuts_the_oldest_fading_one() {
        let mut rig = rig();
        let fading: Vec<_> = (0..SE_FADING_VOICES)
            .map(|_| constant(100_000, 0.01))
            .collect();
        let playing: Vec<_> = (0..SE_PLAYING_VOICES)
            .map(|_| constant(100_000, 0.01))
            .collect();
        for (slot, sound) in fading.iter().enumerate() {
            send_play(&mut rig, slot as u16, sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 64);
        send(&mut rig, SeCommand::StopAll);
        render(&mut rig, 64);
        for (slot, sound) in playing.iter().enumerate() {
            send_play(&mut rig, 100 + slot as u16, sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 64);
        assert_eq!(rig.player.active_voices(), SE_VOICES);
        assert!(collect(&mut rig).is_empty());

        // Restart on a playing slot: its voice starts to fade, one fading
        // voice is cut for it, and the new sound takes the playing place.
        let new = constant(100_000, 0.01);
        send_play(&mut rig, 100, &new, 1.0, SePolicy::Restart);
        render(&mut rig, 64);
        let back = collect(&mut rig);
        assert_eq!(back.len(), 1);
        assert!(fading.iter().any(|sound| Arc::ptr_eq(sound, &back[0])));
        assert_eq!(rig.player.active_voices(), SE_VOICES);
        assert_eq!(rig.stats.snapshot().plays_dropped, 0);
    }

    #[test]
    fn when_the_fading_voices_are_full_stop_cuts_the_oldest_fading_one() {
        let mut rig = rig();
        let fading: Vec<_> = (0..SE_FADING_VOICES)
            .map(|_| constant(100_000, 0.01))
            .collect();
        let playing = constant(100_000, 0.01);
        for (slot, sound) in fading.iter().enumerate() {
            send_play(&mut rig, slot as u16, sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 64);
        send(&mut rig, SeCommand::StopAll);
        render(&mut rig, 64);
        send_play(&mut rig, 100, &playing, 1.0, SePolicy::Overlap);
        render(&mut rig, 64);

        send(&mut rig, SeCommand::Stop { slot: 100 });
        render(&mut rig, 64);
        let back = collect(&mut rig);
        assert_eq!(back.len(), 1);
        assert!(fading.iter().any(|sound| Arc::ptr_eq(sound, &back[0])));
        drop(back);
        assert_eq!(rig.player.active_voices(), SE_FADING_VOICES);

        run(&mut rig, 4 * FADE);
        assert_eq!(collect(&mut rig).len(), SE_FADING_VOICES);
        assert_eq!(rig.stats.snapshot().leaked, 0);
        for sound in fading.iter().chain([&playing]) {
            assert_eq!(Arc::strong_count(sound), 1);
        }
    }

    #[test]
    fn a_full_return_queue_leaks_and_counts_instead_of_dropping() {
        // A return queue for one `Arc`; the second one cannot be given back.
        let mut rig = rig_with_return_capacity(1);
        let first = constant(10, 0.5);
        let second = constant(10, 0.5);
        send_play(&mut rig, 0, &first, 1.0, SePolicy::Overlap);
        send_play(&mut rig, 1, &second, 1.0, SePolicy::Overlap);
        render(&mut rig, 64);

        assert_eq!(rig.stats.snapshot().leaked, 1);
        let back = collect(&mut rig);
        assert_eq!(back.len(), 1);
        assert!(Arc::ptr_eq(&back[0], &first));
        drop(back);
        assert_eq!(Arc::strong_count(&first), 1);
        // The player forgot its reference to the second sound (this test
        // leaks it on purpose), so the audio thread did not free it.
        assert_eq!(Arc::strong_count(&second), 2);
    }

    #[test]
    fn dropping_the_player_gives_back_playing_fading_and_waiting_sounds() {
        let Rig {
            mut commands,
            mut returned,
            mut player,
            stats,
        } = rig();
        let sounds: Vec<_> = (0..6).map(|_| constant(100_000, 0.01)).collect();
        let push_play = |commands: &mut Producer<SeCommand>, slot: u16, sound: &Arc<SoundData>| {
            commands
                .push(SeCommand::Play {
                    slot,
                    sound: Arc::clone(sound),
                    gain: 1.0,
                    policy: SePolicy::Overlap,
                })
                .unwrap();
        };

        // Two playing, one fading, and three waiting in the command queue.
        for (slot, sound) in sounds.iter().take(3).enumerate() {
            push_play(&mut commands, slot as u16, sound);
        }
        player.render(64);
        commands.push(SeCommand::Stop { slot: 2 }).unwrap();
        player.render(64);
        for (slot, sound) in sounds.iter().enumerate().skip(3) {
            push_play(&mut commands, slot as u16, sound);
        }
        assert_eq!(player.active_voices(), 3);
        assert!(returned.pop().is_err());
        for sound in &sounds {
            assert_eq!(Arc::strong_count(sound), 2, "test and the player");
        }

        drop(player);

        let mut back = Vec::new();
        while let Ok(sound) = returned.pop() {
            back.push(sound);
        }
        assert_eq!(back.len(), 6);
        for sound in &sounds {
            assert!(back.iter().any(|back| Arc::ptr_eq(back, sound)));
        }
        drop(back);
        for sound in &sounds {
            assert_eq!(Arc::strong_count(sound), 1);
        }
        assert_eq!(stats.snapshot().leaked, 0);
    }

    #[test]
    fn dropping_an_empty_player_gives_back_nothing() {
        let Rig {
            mut returned,
            player,
            stats,
            ..
        } = rig();
        drop(player);
        assert!(returned.pop().is_err());
        assert_eq!(stats.snapshot().leaked, 0);
    }

    #[test]
    fn at_most_one_block_of_commands_is_taken_per_render() {
        let mut rig = rig();
        let sound = constant(10_000, 0.001);
        for _ in 0..100 {
            send_play(&mut rig, 0, &sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 32);
        let stats = rig.stats.snapshot();
        assert_eq!(stats.plays_started + stats.plays_dropped, 64);
        render(&mut rig, 32);
        let stats = rig.stats.snapshot();
        assert_eq!(stats.plays_started + stats.plays_dropped, 100);
    }

    /// Nothing is collected while 256 commands (the whole command queue)
    /// are played with all voices busy. The return queue is not overrun.
    #[test]
    fn the_worst_case_fills_the_return_queue_without_overflow() {
        let mut rig = rig();
        let sound = constant(1_000_000, 0.0001);
        // The voices fill with 16 plays; fading voices take 16 more.
        for slot in 0..SE_PLAYING_VOICES {
            send_play(&mut rig, slot as u16, &sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 32);
        send(&mut rig, SeCommand::StopAll);
        render(&mut rig, 32);
        for slot in 0..SE_PLAYING_VOICES {
            send_play(&mut rig, slot as u16, &sound, 1.0, SePolicy::Overlap);
        }
        render(&mut rig, 32);
        assert_eq!(rig.player.active_voices(), SE_VOICES);

        // The control side may have 288 `Arc`s out: 32 in the voices and 256
        // in the command queue.
        for _ in 0..SE_COMMAND_CAPACITY {
            send_play(&mut rig, 0, &sound, 1.0, SePolicy::Overlap);
        }
        for _ in 0..SE_COMMAND_CAPACITY / SE_COMMANDS_PER_BLOCK {
            render(&mut rig, 32);
        }
        // All 256 were dropped and given back; they all fit.
        assert_eq!(rig.stats.snapshot().leaked, 0);

        send(&mut rig, SeCommand::StopAll);
        render(&mut rig, 32);
        run(&mut rig, 4 * FADE);
        assert_eq!(rig.player.active_voices(), 0);
        assert_eq!(rig.stats.snapshot().leaked, 0);

        let mut received = 0;
        while let Ok(sound) = rig.returned.pop() {
            received += 1;
            drop(sound);
        }
        assert_eq!(received, 32 + SE_COMMAND_CAPACITY);
        assert_eq!(Arc::strong_count(&sound), 1);
    }

    /// A small random generator, so the test below is the same every time.
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

    /// Random commands for thousands of blocks, with the control side
    /// keeping the number of `Arc`s that are out at or below the return
    /// queue capacity (as `SeController` will). Every `Arc` comes back
    /// exactly once and nothing leaks.
    #[test]
    fn every_arc_comes_back_exactly_once_with_random_commands() {
        for seed in [1_u64, 7, 12_345, 987_654_321] {
            let mut rng = Rng(seed);
            let mut rig = rig();
            let pool: Vec<_> = [1, 50, 300, 480, 481, 1000, 3000, 20_000]
                .into_iter()
                .map(|frames| constant(frames, 0.01))
                .collect();
            let policies = [SePolicy::Overlap, SePolicy::Restart, SePolicy::Ignore];
            let mut sent = 0_usize;
            let mut received = 0_usize;
            let mut longest_outstanding = 0;

            for block in 0..4000 {
                for _ in 0..rng.below(4) {
                    let outstanding = sent - received;
                    match rng.below(10) {
                        0..=6 => {
                            if outstanding < SE_RETURN_CAPACITY && !rig.commands.is_full() {
                                let sound = &pool[rng.below(pool.len() as u64) as usize];
                                let policy = policies[rng.below(3) as usize];
                                send_play(&mut rig, rng.below(6) as u16, sound, 1.0, policy);
                                sent += 1;
                            }
                        }
                        7..=8 => {
                            if !rig.commands.is_full() {
                                send(
                                    &mut rig,
                                    SeCommand::Stop {
                                        slot: rng.below(6) as u16,
                                    },
                                );
                            }
                        }
                        _ => {
                            if !rig.commands.is_full() {
                                send(&mut rig, SeCommand::StopAll);
                            }
                        }
                    }
                }
                render(&mut rig, 1 + rng.below(256) as usize);
                // Sometimes the control side does not collect for a long
                // time.
                if block % 50 == 0 || rng.below(20) == 0 {
                    received += collect(&mut rig).len();
                }
                longest_outstanding = longest_outstanding.max(sent - received);
                assert!(sent - received <= SE_RETURN_CAPACITY);
            }

            send(&mut rig, SeCommand::StopAll);
            for _ in 0..200 {
                render(&mut rig, 256);
            }
            received += collect(&mut rig).len();

            assert_eq!(rig.player.active_voices(), 0, "seed {seed}");
            assert_eq!(sent, received, "seed {seed}");
            let stats = rig.stats.snapshot();
            assert_eq!(stats.leaked, 0, "seed {seed}");
            assert_eq!(
                stats.plays_started + stats.plays_ignored + stats.plays_dropped,
                sent as u64,
                "seed {seed}"
            );
            for sound in &pool {
                assert_eq!(Arc::strong_count(sound), 1, "seed {seed}");
            }
            assert!(longest_outstanding > 0);
        }
    }
}
