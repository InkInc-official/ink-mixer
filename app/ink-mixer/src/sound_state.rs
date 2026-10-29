//! What the window knows about the BGM and the Sound Pad, and what it does
//! with them (#64). No egui here: the drawing code (`gui/bgm_strip.rs`,
//! `gui/sound_pad.rs`) only shows this state and calls these functions, so
//! the decisions can be tested without a window.
//!
//! # What the window owns, and what it only borrows
//!
//! The window owns a [`BgmPanel`] and a [`PadBank`]. They hold IDs, flags and
//! numbers, never the BGM player or the SE controller: those belong to the
//! audio session (`session.rs`), which the window drops in the order that
//! ADR-0009 needs. The panel gets `&Bgm` and the bank gets
//! `&mut dyn SoundSlots` for the time of one call.
//!
//! # The saved ID is kept
//!
//! A song or a sound effect that cannot be used now (the library has no such
//! ID, its file is missing, the library cannot be opened) stays chosen: the
//! panel and the pad show why, in Japanese, and the ID is still what is saved.
//! Only the user's own choice replaces it (ARCHITECTURE.md §9, #64).
//!
//! # Nothing here waits
//!
//! [`Bgm::load`] only sends the path to the worker: the file is opened later,
//! and a failure reaches [`BgmPanel::poll`] (through [`Bgm::take_error`]) some
//! frames after. The sound effects are read by [`SeLoader`] on its own thread;
//! [`PadBank::poll`] takes the results.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ink_core::graph::{SE_VOLUME_RANGE, SeController, SeError, SePolicy, SoundData, db_to_gain};
use ink_core::{AssetId, AssetKind, AssetState, PadSetting, SE_MAX_DURATION};
use tracing::{debug, warn};

use crate::bgm_worker::Bgm;
use crate::library_view::{LibraryView, Resolved};
use crate::se_loader::{Job, LoadError, LoadErrorKind, SeLoader};

/// How many pads the window has (4 × 2). The SE player has more slots
/// (`SE_SLOTS`); a larger number here is all it takes to show more pads.
pub const PAD_COUNT: usize = 8;

// -- the text ------------------------------------------------------------------------

/// What to say about an Asset whose file cannot be used, in Japanese: the
/// file is gone (`Missing`) or cannot be read (`Unreadable`). `None` for one
/// that can be used. The words are here only; the choice that is saved
/// ([`problem_ja`]) and the list of songs and sound effects use them.
pub fn unusable_hint_ja(name: &str, state: AssetState) -> Option<String> {
    match state {
        AssetState::Available => None,
        AssetState::Missing => Some(format!(
            "「{name}」のファイルが見つかりません（フォルダから消えた可能性があります）"
        )),
        AssetState::Unreadable => Some(format!(
            "「{name}」のファイルを読めません（読めない音源は、取り込みページの更新（再スキャン）で直ることがあります）"
        )),
    }
}

/// Why a saved ID cannot be used now, in Japanese. `None` if it can be.
/// `library_ready` is whether the library could be opened.
fn problem_ja(
    resolved: &Resolved,
    id: &AssetId,
    kind: AssetKind,
    library_ready: bool,
) -> Option<String> {
    match resolved {
        Resolved::Found { .. } => None,
        Resolved::Unknown if !library_ready => Some(format!(
            "ライブラリが使えないため、開けません（ID {id} は設定に残っています）"
        )),
        Resolved::Unknown => Some(format!("未選択（ID {id} が見つかりません）")),
        Resolved::WrongKind => Some(match kind {
            AssetKind::Bgm => format!("未選択（ID {id} は曲の ID ではありません）"),
            AssetKind::Se => format!("未選択（ID {id} は SE の ID ではありません）"),
        }),
        Resolved::Missing { name } => unusable_hint_ja(name, AssetState::Missing),
        Resolved::Unreadable { name } => unusable_hint_ja(name, AssetState::Unreadable),
    }
}

/// Formats a position or a length as `3:05`, or `1:02:03` from an hour on.
/// The seconds are rounded down. `None` (the length is not known) is `--:--`.
pub fn format_clock(duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return "--:--".to_string();
    };
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

// -- the BGM -------------------------------------------------------------------------

/// What the BGM is doing, for the buttons and the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// No song is open (none chosen, one that cannot be used, or no audio).
    NoSong,
    /// The song is at its start, not playing.
    Stopped,
    /// The song is to be heard (it may still be filling its buffer).
    Playing,
    /// The song is paused in the middle.
    Paused,
    /// A song that does not loop has played to its end.
    Finished,
    /// The song could not be opened, or broke. Stop (or Play) tries again.
    Failed,
}

/// Decides the [`Transport`]. A failure comes first (the player counts a
/// failed song as finished); a song that ended comes before "playing" (the
/// play flag stays on at the end of a song).
///
/// `flush_pending` is a Stop (or another song) that the audio thread has not
/// done yet: it fades out for a moment and the position keeps its old value
/// until then. Without the play flag that is a stop, not a pause.
pub fn transport_of(
    has_song: bool,
    failed: bool,
    wants_play: bool,
    finished: bool,
    flush_pending: bool,
    position_frames: u64,
) -> Transport {
    if !has_song {
        Transport::NoSong
    } else if failed {
        Transport::Failed
    } else if finished {
        Transport::Finished
    } else if wants_play {
        Transport::Playing
    } else if flush_pending {
        Transport::Stopped
    } else if position_frames > 0 {
        Transport::Paused
    } else {
        Transport::Stopped
    }
}

/// The song that was sent to the worker in this session.
#[derive(Debug)]
struct Song {
    name: String,
    /// The length the library knows (from the file's header), for the time
    /// before the player says it.
    length: Option<Duration>,
    /// The file. A failure of another file is not this song's.
    path: PathBuf,
}

/// A failure of the song, for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureView {
    /// One line in Japanese.
    pub summary_ja: String,
    /// The English text of the error.
    pub detail: String,
}

/// The note that is shown when the audio was made again while a song played.
const RESTART_NOTICE: &str = "デバイスを切り替えたため、BGM は止まりました。▶ で頭から再生できます";

/// The state of the BGM strip.
#[derive(Debug)]
pub struct BgmPanel {
    /// The chosen song. Kept when it cannot be used (see the module docs).
    selected: Option<AssetId>,
    looping: bool,
    /// The song that was opened (sent to the worker) in this session.
    song: Option<Song>,
    /// Why the chosen song is not open, in Japanese.
    problem: Option<String>,
    failure: Option<FailureView>,
    notice: Option<String>,
}

impl BgmPanel {
    /// Creates the panel with the saved song and loop.
    pub fn new(saved: Option<AssetId>, looping: bool) -> Self {
        Self {
            selected: saved,
            looping,
            song: None,
            problem: None,
            failure: None,
            notice: None,
        }
    }

    /// The chosen song (what is saved).
    pub fn selected(&self) -> Option<&AssetId> {
        self.selected.as_ref()
    }

    /// Whether the song loops.
    pub fn looping(&self) -> bool {
        self.looping
    }

    /// The display name of the open song.
    #[cfg(test)]
    pub fn song_name(&self) -> Option<&str> {
        self.song.as_ref().map(|song| song.name.as_str())
    }

    /// Why the chosen song is not open, in Japanese.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// The failure of the song, if there is one.
    pub fn failure(&self) -> Option<&FailureView> {
        self.failure.as_ref()
    }

    /// The note about the restart of the audio, until the user does something.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Whether a song is playing now. Ask this before the session is dropped,
    /// and tell the answer to [`on_session_started`](Self::on_session_started).
    pub fn was_playing(&self, bgm: Option<&Bgm>) -> bool {
        self.transport(bgm) == Transport::Playing
    }

    /// Chooses `id` (or no song). With a session, the song is opened at once:
    /// **the file is opened later by the worker**, so a song that cannot be
    /// played shows up in [`poll`](Self::poll) after some frames. Without a
    /// session, the song is opened when the next session starts.
    ///
    /// A failure and the note go away. When the new choice cannot be used
    /// and a song was open, that song is stopped.
    pub fn select(&mut self, id: Option<AssetId>, view: &LibraryView, bgm: Option<&Bgm>) {
        let had_song = self.song.is_some();
        self.selected = id;
        self.failure = None;
        self.notice = None;
        match bgm {
            Some(bgm) => {
                self.open_selected(view, bgm);
                if had_song && self.song.is_none() {
                    bgm.stop();
                }
            }
            None => {
                self.song = None;
                self.problem = self.problem_of(view);
            }
        }
    }

    /// The library was opened again (the "read again" button): a chosen song
    /// that could not be used is tried again. A song that is open is left
    /// alone, so it does not start over.
    pub fn on_library_reloaded(&mut self, view: &LibraryView, bgm: Option<&Bgm>) {
        if self.song.is_some() {
            return;
        }
        match bgm {
            Some(bgm) => self.open_selected(view, bgm),
            None => self.problem = self.problem_of(view),
        }
    }

    /// The audio was started (again). `bgm` is the new player: stopped, with
    /// no song and no loop. The chosen song is opened and the loop is set;
    /// the position and the play state of the old session are not carried
    /// over. If the song was playing (`was_playing`), the note tells that it
    /// stopped.
    pub fn on_session_started(&mut self, view: &LibraryView, bgm: &Bgm, was_playing: bool) {
        self.failure = None;
        self.notice = was_playing.then(|| RESTART_NOTICE.to_string());
        self.open_selected(view, bgm);
    }

    /// Play when stopped or paused, pause when playing. A song that ended is
    /// stopped first and then played from its start. After a failure, the
    /// song is tried again the same way.
    pub fn toggle_play(&mut self, bgm: &Bgm) {
        self.notice = None;
        let controller = bgm.controller();
        match self.transport(Some(bgm)) {
            Transport::NoSong => {}
            Transport::Playing => controller.pause(),
            Transport::Paused | Transport::Stopped => controller.play(),
            Transport::Finished => {
                // A song that ended plays again only after a Stop.
                bgm.stop();
                controller.play();
            }
            Transport::Failed => {
                self.failure = None;
                bgm.stop();
                controller.play();
            }
        }
    }

    /// Stops the song: it goes back to its start. A failure goes away; the
    /// worker opens the file again.
    pub fn stop(&mut self, bgm: &Bgm) {
        self.notice = None;
        self.failure = None;
        if self.song.is_some() {
            bgm.stop();
        }
    }

    /// Sets the loop. With a player, it is set there too.
    pub fn set_looping(&mut self, on: bool, bgm: Option<&Bgm>) {
        self.looping = on;
        if let Some(bgm) = bgm {
            bgm.controller().set_looping(on);
        }
    }

    /// Takes the failure of the song from the worker, if there is one. Call it
    /// every frame: **nobody sees the failure if it is not taken**
    /// (ADR-0009). A failure of a file that is not the open song (an older
    /// choice) is left out.
    pub fn poll(&mut self, bgm: &Bgm) {
        let Some(failure) = bgm.take_error() else {
            return;
        };
        let Some(song) = &self.song else {
            return;
        };
        if failure.path.as_ref().is_some_and(|path| *path != song.path) {
            debug!("a failure of an older song is left out: {failure}");
            return;
        }
        self.failure = Some(FailureView {
            summary_ja: failure.message_ja(&song.name),
            detail: failure.message,
        });
    }

    /// What the song is doing.
    pub fn transport(&self, bgm: Option<&Bgm>) -> Transport {
        let Some(bgm) = bgm else {
            return Transport::NoSong;
        };
        let controller = bgm.controller();
        transport_of(
            self.song.is_some(),
            self.failure.is_some(),
            controller.wants_play(),
            controller.is_finished(),
            !controller.is_flushed(),
            controller.position_frames(),
        )
    }

    /// The position and the length of the song. The length is the player's
    /// when it knows it, or else the library's, or else unknown. The position
    /// never goes past the length.
    pub fn position(&self, bgm: &Bgm) -> (Duration, Option<Duration>) {
        let controller = bgm.controller();
        let rate = f64::from(controller.sample_rate().max(1));
        let seconds = |frames: u64| Duration::from_secs_f64(frames as f64 / rate);
        let length = controller
            .total_frames()
            .map(seconds)
            .or_else(|| self.song.as_ref().and_then(|song| song.length));
        let position = seconds(controller.position_frames());
        (
            length.map_or(position, |length| position.min(length)),
            length,
        )
    }

    fn problem_of(&self, view: &LibraryView) -> Option<String> {
        let id = self.selected.as_ref()?;
        problem_ja(
            &view.resolve(id, AssetKind::Bgm),
            id,
            AssetKind::Bgm,
            view.library().is_some(),
        )
    }

    /// Opens the chosen song in `bgm`, or finds out why it cannot be. The path
    /// goes to the worker with [`Bgm::load`] (which does not wait), and the
    /// loop is set **after** it.
    fn open_selected(&mut self, view: &LibraryView, bgm: &Bgm) {
        self.song = None;
        self.problem = None;
        if let Some(id) = self.selected.as_ref() {
            match view.resolve(id, AssetKind::Bgm) {
                Resolved::Found { path, name, length } => {
                    bgm.load(&path);
                    self.song = Some(Song { name, length, path });
                }
                other => {
                    self.problem = problem_ja(&other, id, AssetKind::Bgm, view.library().is_some());
                }
            }
        }
        bgm.controller().set_looping(self.looping);
    }
}

// -- the Sound Pad -------------------------------------------------------------------

/// What [`PadBank`] needs of the SE player. [`SeController`] is one; the tests
/// have their own, which can refuse things the real one refuses only in rare
/// cases (a full queue, too much memory).
pub trait SoundSlots {
    /// The sample rate of the player: the sounds have to be at this rate.
    fn sample_rate(&self) -> u32;
    fn set_sound(&mut self, slot: usize, sound: Arc<SoundData>) -> Result<(), SeError>;
    fn clear(&mut self, slot: usize) -> Result<(), SeError>;
    fn play(&mut self, slot: usize, gain: f32, policy: SePolicy) -> Result<(), SeError>;
    fn stop_all(&mut self) -> Result<(), SeError>;
    /// Drops the sounds that the player gave back (on the control thread).
    fn collect(&mut self) -> usize;
}

impl SoundSlots for SeController {
    fn sample_rate(&self) -> u32 {
        SeController::sample_rate(self)
    }

    fn set_sound(&mut self, slot: usize, sound: Arc<SoundData>) -> Result<(), SeError> {
        SeController::set_sound(self, slot, sound)
    }

    fn clear(&mut self, slot: usize) -> Result<(), SeError> {
        SeController::clear(self, slot)
    }

    fn play(&mut self, slot: usize, gain: f32, policy: SePolicy) -> Result<(), SeError> {
        SeController::play(self, slot, gain, policy)
    }

    fn stop_all(&mut self) -> Result<(), SeError> {
        SeController::stop_all(self)
    }

    fn collect(&mut self) -> usize {
        SeController::collect(self)
    }
}

/// What one pad is doing.
#[derive(Debug, Clone, PartialEq)]
pub enum PadState {
    /// Nothing is in the player for this pad: no sound is chosen, or the
    /// audio is not running (the sound is read when it starts).
    Empty,
    /// The sound is being read. It cannot be played yet.
    Loading,
    /// The sound is in the player.
    Ready { length: Option<Duration> },
    /// The sound cannot be used; the line says why, in Japanese. The ID is
    /// still the pad's.
    Failed(String),
}

#[derive(Debug)]
struct Pad {
    asset: Option<AssetId>,
    gain_db: f32,
    state: PadState,
    /// The job whose result is wanted. 0: none, so every result is old.
    ticket: u64,
    /// The display name, for the messages.
    name: String,
    /// The English text of the failure.
    detail: Option<String>,
}

impl Pad {
    fn empty() -> Self {
        Self {
            asset: None,
            gain_db: 0.0,
            state: PadState::Empty,
            ticket: 0,
            name: String::new(),
            detail: None,
        }
    }
}

/// What came of pressing a pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// The sound was sent to the player.
    Played,
    /// The pad has no sound that can be played (empty, loading, failed).
    NotReady,
    /// The player has too much to do (too many sounds, or a full queue).
    Busy,
    /// The player refused for another reason.
    Rejected,
}

impl Press {
    /// A short note for the status line, if the press needs one.
    pub fn notice_ja(self) -> Option<&'static str> {
        match self {
            Self::Played | Self::NotReady => None,
            Self::Busy => Some("SE が混み合っています。少し待ってから、もう一度押してください"),
            Self::Rejected => Some("この SE は鳴らせませんでした"),
        }
    }
}

/// The pads of the Sound Pad.
#[derive(Debug)]
pub struct PadBank {
    pads: Vec<Pad>,
    policy: SePolicy,
    next_ticket: u64,
}

impl PadBank {
    /// Creates the bank from the saved pads. Nothing is read yet: the sounds
    /// are read when a session starts ([`on_session_started`](Self::on_session_started)).
    pub fn new(saved: &[PadSetting], policy: SePolicy) -> Self {
        let mut pads: Vec<Pad> = (0..PAD_COUNT).map(|_| Pad::empty()).collect();
        for setting in saved {
            if let Some(pad) = pads.get_mut(setting.slot) {
                pad.asset = Some(setting.asset);
                pad.gain_db = SE_VOLUME_RANGE.clamp(setting.gain_db);
            }
        }
        Self {
            pads,
            policy,
            next_ticket: 0,
        }
    }

    pub fn policy(&self) -> SePolicy {
        self.policy
    }

    pub fn set_policy(&mut self, policy: SePolicy) {
        self.policy = policy;
    }

    pub fn state(&self, slot: usize) -> &PadState {
        self.pads
            .get(slot)
            .map_or(&PadState::Empty, |pad| &pad.state)
    }

    /// The sound chosen for the pad (what is saved).
    pub fn asset(&self, slot: usize) -> Option<&AssetId> {
        self.pads.get(slot).and_then(|pad| pad.asset.as_ref())
    }

    /// The display name of the sound, once it is known.
    pub fn name(&self, slot: usize) -> Option<&str> {
        self.pads
            .get(slot)
            .map(|pad| pad.name.as_str())
            .filter(|name| !name.is_empty())
    }

    /// The English text of the failure of the pad.
    pub fn detail(&self, slot: usize) -> Option<&str> {
        self.pads.get(slot).and_then(|pad| pad.detail.as_deref())
    }

    pub fn gain_db(&self, slot: usize) -> f32 {
        self.pads.get(slot).map_or(0.0, |pad| pad.gain_db)
    }

    /// Sets the volume of the pad, within the SE range.
    pub fn set_gain_db(&mut self, slot: usize, db: f32) {
        if let Some(pad) = self.pads.get_mut(slot) {
            pad.gain_db = SE_VOLUME_RANGE.clamp(db);
        }
    }

    /// Whether a sound is being read.
    #[cfg(test)]
    pub fn loading(&self) -> bool {
        self.pads
            .iter()
            .any(|pad| matches!(pad.state, PadState::Loading))
    }

    /// The pads that have a sound chosen, for the settings. Includes those
    /// that failed or whose ID the library does not know.
    pub fn saved(&self) -> Vec<PadSetting> {
        self.pads
            .iter()
            .enumerate()
            .filter_map(|(slot, pad)| {
                pad.asset.map(|asset| PadSetting {
                    slot,
                    asset,
                    gain_db: pad.gain_db,
                })
            })
            .collect()
    }

    /// The user chose `id` (or no sound) for the pad. The old sound leaves the
    /// player at once, so it cannot be played any more. With a session, the
    /// new sound is queued for reading; without one, it is read when a
    /// session starts.
    pub fn assign(
        &mut self,
        slot: usize,
        id: Option<AssetId>,
        view: &LibraryView,
        se: Option<&mut dyn SoundSlots>,
        loader: &SeLoader,
    ) {
        let Some(pad) = self.pads.get_mut(slot) else {
            return;
        };
        pad.asset = id;
        let rate = se.map(|se| {
            // The old sound goes; a sound that is playing plays to its end.
            let _ = se.clear(slot);
            se.sample_rate()
        });
        self.begin(slot, view, rate.map(|rate| (rate, loader)));
    }

    /// A session started: its player is empty and may run at another rate.
    /// Every chosen sound is queued for reading again, in the order of the
    /// pads, and the loader reads them one at a time. Results of the jobs of
    /// the old session are thrown away.
    pub fn on_session_started(
        &mut self,
        view: &LibraryView,
        se: &mut dyn SoundSlots,
        loader: &SeLoader,
    ) {
        let rate = se.sample_rate();
        for slot in 0..self.pads.len() {
            self.begin(slot, view, Some((rate, loader)));
        }
    }

    /// Takes the results of the loader. Call it every frame. A result that is
    /// not of the newest job of its pad is thrown away. Without a session
    /// (`se` is `None`), the sounds are thrown away too.
    pub fn poll(&mut self, loader: &SeLoader, mut se: Option<&mut dyn SoundSlots>) {
        while let Some(done) = loader.try_recv() {
            let Some(pad) = self.pads.get_mut(done.slot) else {
                continue;
            };
            if pad.ticket == 0 || pad.ticket != done.ticket {
                debug!(
                    "the result of an older job of pad {} is thrown away",
                    done.slot
                );
                continue;
            }
            pad.ticket = 0;
            match done.result {
                Err(error) => {
                    pad.state = PadState::Failed(error.message_ja(&pad.name));
                    pad.detail = Some(error.detail);
                }
                Ok(loaded) => match se.as_deref_mut() {
                    // The session is gone: nobody to play it.
                    None => pad.state = PadState::Empty,
                    Some(se) => {
                        let length = loaded.sound.duration();
                        debug!(
                            "pad {}: read in {:?}, {} packets skipped",
                            done.slot, loaded.took, loaded.skipped_packets
                        );
                        if loaded.skipped_packets > 0 {
                            warn!(
                                "pad {}: {} packets of the sound could not be decoded and were skipped",
                                done.slot, loaded.skipped_packets
                            );
                        }
                        match se.set_sound(done.slot, loaded.sound) {
                            Ok(()) => {
                                pad.state = PadState::Ready {
                                    length: Some(length),
                                }
                            }
                            Err(error) => {
                                pad.state = PadState::Failed(set_sound_error_ja(&error, &pad.name));
                                pad.detail = Some(error.to_string());
                            }
                        }
                    }
                },
            }
        }
        if let Some(se) = se {
            se.collect();
        }
    }

    /// Plays the sound of the pad, at the volume of the pad. Only a pad that
    /// is ready plays.
    pub fn press(&self, slot: usize, se: &mut dyn SoundSlots) -> Press {
        let Some(pad) = self.pads.get(slot) else {
            return Press::NotReady;
        };
        if !matches!(pad.state, PadState::Ready { .. }) {
            return Press::NotReady;
        }
        match se.play(slot, db_to_gain(pad.gain_db), self.policy) {
            Ok(()) => Press::Played,
            Err(SeError::Busy | SeError::QueueFull) => Press::Busy,
            Err(error) => {
                debug!("the player refused pad {slot}: {error}");
                Press::Rejected
            }
        }
    }

    /// Fades out every sound. Returns whether the player took the command.
    pub fn stop_all(&self, se: &mut dyn SoundSlots) -> bool {
        se.stop_all().is_ok()
    }

    /// Finds out what the pad is now, and queues its sound for reading when
    /// there is a session (`target` is its rate and the loader). The jobs of
    /// the pad that are out become old.
    fn begin(&mut self, slot: usize, view: &LibraryView, target: Option<(u32, &SeLoader)>) {
        let ticket = self.next_ticket + 1;
        let Some(pad) = self.pads.get_mut(slot) else {
            return;
        };
        pad.ticket = 0;
        pad.detail = None;
        pad.state = PadState::Empty;
        let Some(id) = pad.asset else {
            pad.name.clear();
            return;
        };
        match view.resolve(&id, AssetKind::Se) {
            Resolved::Found { path, name, length } => {
                pad.name = name;
                // A length that the library knows is checked before anything
                // is read.
                if length.is_some_and(|length| length > SE_MAX_DURATION) {
                    let error = LoadError {
                        kind: LoadErrorKind::TooLong,
                        detail: "the library says the sound is longer than the limit".to_string(),
                    };
                    pad.state = PadState::Failed(error.message_ja(&pad.name));
                    pad.detail = Some(error.detail);
                    return;
                }
                let Some((rate, loader)) = target else {
                    return;
                };
                let job = Job {
                    ticket,
                    slot,
                    path,
                    rate,
                };
                if loader.submit(job) {
                    self.next_ticket = ticket;
                    pad.ticket = ticket;
                    pad.state = PadState::Loading;
                } else {
                    pad.state = PadState::Failed(
                        "SE を読み込む仕組みが止まっています。Ink Mixer を開き直してください"
                            .to_string(),
                    );
                    pad.detail = Some("the SE loader thread is gone".to_string());
                }
            }
            other => {
                pad.name.clear();
                if let Some(text) = problem_ja(&other, &id, AssetKind::Se, view.library().is_some())
                {
                    pad.state = PadState::Failed(text);
                }
            }
        }
    }
}

/// The text for a sound that the player does not take.
fn set_sound_error_ja(error: &SeError, name: &str) -> String {
    match error {
        SeError::TotalTooLarge { limit_bytes, .. } => format!(
            "SE の合計が上限（{} MiB）を超えるため、「{name}」は読み込めません。使っていない SE を外してください",
            limit_bytes / (1024 * 1024)
        ),
        _ => format!("「{name}」を鳴らせる形にできませんでした"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Instant;

    use ink_core::graph::{LinkWriter, MicGraph, MicrophoneSource, MixerControl};
    use ink_core::graph::{audio_link, bgm_channel};

    use super::*;

    /// A small rate keeps the tests short.
    const RATE: u32 = 8_000;
    const BLOCK: usize = 64;
    /// Long enough for anything that works; a test that waits this long has
    /// failed.
    const TIMEOUT: Duration = Duration::from_secs(10);
    const QUARTER: i16 = 8_192;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    // -- format_clock and transport_of ------------------------------------------------

    #[test]
    fn the_hint_for_an_asset_that_cannot_be_used_is_one_text_for_a_gone_file_and_an_unreadable_one()
    {
        assert_eq!(unusable_hint_ja("曲 テスト", AssetState::Available), None);
        assert_eq!(
            unusable_hint_ja("曲 テスト", AssetState::Missing).as_deref(),
            Some("「曲 テスト」のファイルが見つかりません（フォルダから消えた可能性があります）")
        );
        assert_eq!(
            unusable_hint_ja("曲 テスト", AssetState::Unreadable).as_deref(),
            Some(
                "「曲 テスト」のファイルを読めません（読めない音源は、取り込みページの更新（再スキャン）で直ることがあります）"
            )
        );
        // The text of the chosen song is this one text.
        let id: AssetId = "bgm-0001".parse().unwrap();
        for (resolved, state) in [
            (
                Resolved::Missing {
                    name: "曲 テスト".to_string(),
                },
                AssetState::Missing,
            ),
            (
                Resolved::Unreadable {
                    name: "曲 テスト".to_string(),
                },
                AssetState::Unreadable,
            ),
        ] {
            assert_eq!(
                problem_ja(&resolved, &id, AssetKind::Bgm, true),
                unusable_hint_ja("曲 テスト", state)
            );
        }
    }

    #[test]
    fn a_clock_shows_minutes_and_seconds_and_hours_when_there_are_some() {
        let secs = |s: f64| Some(Duration::from_secs_f64(s));
        assert_eq!(format_clock(secs(0.0)), "0:00");
        assert_eq!(format_clock(secs(9.0)), "0:09");
        assert_eq!(format_clock(secs(59.9)), "0:59", "rounded down");
        assert_eq!(format_clock(secs(60.0)), "1:00");
        assert_eq!(format_clock(secs(185.0)), "3:05");
        assert_eq!(format_clock(secs(3599.0)), "59:59");
        assert_eq!(format_clock(secs(3723.0)), "1:02:03");
        assert_eq!(format_clock(None), "--:--");
    }

    #[test]
    fn the_transport_follows_the_failure_the_end_the_play_flag_and_the_position() {
        use Transport::*;
        // (has_song, failed, wants_play, finished, flush_pending, position)
        // -> transport
        let cases = [
            ((false, false, false, false, false, 0), NoSong),
            ((false, true, true, true, false, 99), NoSong),
            ((false, false, false, false, true, 99), NoSong),
            ((true, false, false, false, false, 0), Stopped),
            ((true, false, true, false, false, 0), Playing),
            ((true, false, true, false, false, 500), Playing),
            ((true, false, false, false, false, 500), Paused),
            ((true, false, false, true, false, 500), Finished),
            ((true, false, true, true, false, 500), Finished),
            ((true, true, false, false, false, 0), Failed),
            ((true, true, true, true, false, 500), Failed),
            // A Stop the audio thread has not done yet: the position is
            // still the old one, but it is a stop, not a pause.
            ((true, false, false, false, true, 500), Stopped),
            ((true, false, false, false, true, 0), Stopped),
            // Play right after Stop is playing.
            ((true, false, true, false, true, 500), Playing),
            // A failure and an end come before the pending stop.
            ((true, true, false, false, true, 500), Failed),
            ((true, false, false, true, true, 500), Finished),
        ];
        for ((has_song, failed, wants_play, finished, flush_pending, position), expected) in cases {
            assert_eq!(
                transport_of(
                    has_song,
                    failed,
                    wants_play,
                    finished,
                    flush_pending,
                    position
                ),
                expected,
                "{has_song} {failed} {wants_play} {finished} {flush_pending} {position}"
            );
        }
    }

    // -- the files for the tests ------------------------------------------------------

    /// A fresh directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ink-mixer-sound-state-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A mono 16-bit WAV file at `RATE` whose header says `claimed` frames,
    /// with `samples` in it.
    fn write_wav(path: &Path, samples: usize, claimed: usize) {
        let data_len = (claimed * 2) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1_u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&RATE.to_le_bytes());
        bytes.extend_from_slice(&(RATE * 2).to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for _ in 0..samples {
            bytes.extend_from_slice(&QUARTER.to_le_bytes());
        }
        fs::write(path, bytes).unwrap();
    }

    /// An index entry that was read once, with `frames` frames at `RATE`.
    fn entry(id: &str, kind: &str, path: &str, frames: u64) -> String {
        format!(
            "[[asset]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{path}\"\n\n\
             [asset.info]\nformat = \"wav\"\nsample_rate = {RATE}\nchannels = 1\nframes = {frames}\n\n"
        )
    }

    /// An index entry that was never read.
    fn entry_unread(id: &str, kind: &str, path: &str) -> String {
        format!("[[asset]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{path}\"\n\n")
    }

    /// A library with songs and sound effects in every state:
    ///
    /// - `bgm-0001` 曲 テスト (2,000 frames), `bgm-0002` other (4,000 frames),
    ///   `bgm-0003` gone (no file), `bgm-0004` unread (never read),
    ///   `bgm-0005` vanishing (the file is deleted after the library opened)
    /// - `se-0001` 拍手 テスト (1,000 frames), `se-0002` bell (2,000 frames),
    ///   `se-0003` gone, `se-0004` unread, `se-0005` the file says 61 s but the
    ///   library says 1,000 frames, `se-0006` the library says 61 s,
    ///   `se-0007` vanishing
    struct Fixture {
        view: LibraryView,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = test_dir(name);
            let folder = |kind: &str| {
                let folder = root.join("sounds").join(kind);
                fs::create_dir_all(&folder).unwrap();
                folder
            };
            let (bgm, se) = (folder("bgm"), folder("se"));
            write_wav(&bgm.join("曲 テスト.wav"), 2_000, 2_000);
            write_wav(&bgm.join("other.wav"), 4_000, 4_000);
            write_wav(&bgm.join("unread.wav"), 100, 100);
            write_wav(&bgm.join("vanishing.wav"), 2_000, 2_000);
            write_wav(&se.join("拍手 テスト.wav"), 1_000, 1_000);
            write_wav(&se.join("bell.wav"), 2_000, 2_000);
            write_wav(&se.join("unread.wav"), 100, 100);
            write_wav(&se.join("claims long.wav"), 0, 61 * RATE as usize);
            write_wav(&se.join("long.wav"), 100, 100);
            write_wav(&se.join("vanishing.wav"), 1_000, 1_000);
            let entries = [
                entry("bgm-0001", "bgm", "曲 テスト.wav", 2_000),
                entry("bgm-0002", "bgm", "other.wav", 4_000),
                entry("bgm-0003", "bgm", "gone.wav", 2_000),
                entry_unread("bgm-0004", "bgm", "unread.wav"),
                entry("bgm-0005", "bgm", "vanishing.wav", 2_000),
                entry("se-0001", "se", "拍手 テスト.wav", 1_000),
                entry("se-0002", "se", "bell.wav", 2_000),
                entry("se-0003", "se", "gone.wav", 1_000),
                entry_unread("se-0004", "se", "unread.wav"),
                entry("se-0005", "se", "claims long.wav", 1_000),
                entry("se-0006", "se", "long.wav", 61 * u64::from(RATE)),
                entry("se-0007", "se", "vanishing.wav", 1_000),
            ];
            fs::write(
                root.join("library.toml"),
                format!("version = 1\nnext_id = 100\n\n{}", entries.concat()),
            )
            .unwrap();
            let view = LibraryView::open(Some(&root));
            assert!(view.library().is_some());
            // The library knows the files now; these two go away after that.
            fs::remove_file(bgm.join("vanishing.wav")).unwrap();
            fs::remove_file(se.join("vanishing.wav")).unwrap();
            Self { view }
        }

        /// A library that cannot be opened.
        fn broken(name: &str) -> LibraryView {
            let root = test_dir(name);
            fs::write(root.join("library.toml"), "garbage").unwrap();
            LibraryView::open(Some(&root))
        }
    }

    // -- the BGM, with a real player and a graph that is rendered by the test --------

    /// The BGM player with the worker, and a graph that the test renders in
    /// place of the audio thread. `graph` is declared first, so it drops first
    /// (the order of `AudioSession`).
    struct Rig {
        graph: MicGraph,
        bgm: Bgm,
        _writer: LinkWriter,
    }

    impl Rig {
        fn new() -> Self {
            let control = Arc::new(MixerControl::new());
            let (writer, reader, _stats) = audio_link(RATE, BLOCK);
            let (controller, feeder, player) = bgm_channel(RATE, BLOCK);
            let graph =
                MicGraph::new(MicrophoneSource::new(reader), control, RATE, BLOCK).with_bgm(player);
            let bgm = Bgm::start(controller, feeder).unwrap();
            Self {
                graph,
                bgm,
                _writer: writer,
            }
        }

        /// Renders blocks, as the audio thread would, until `condition`.
        fn pump_until(&mut self, what: &str, mut condition: impl FnMut(&Bgm) -> bool) {
            let start = Instant::now();
            let mut out = vec![0.0_f32; BLOCK * 2];
            loop {
                self.graph.render(&mut out, 2);
                if condition(&self.bgm) {
                    return;
                }
                assert!(start.elapsed() < TIMEOUT, "waited too long for {what}");
                thread::sleep(Duration::from_millis(1));
            }
        }

        /// Renders blocks until the flush that was asked for is done.
        fn settle(&mut self) {
            self.pump_until("the flush", |bgm| bgm.controller().is_flushed());
        }
    }

    fn panel_with_song(fixture: &Fixture, rig: &mut Rig, song: &str) -> BgmPanel {
        let mut panel = BgmPanel::new(None, false);
        panel.select(Some(id(song)), &fixture.view, Some(&rig.bgm));
        rig.settle();
        panel
    }

    #[test]
    fn choosing_a_song_opens_it_stopped_with_the_loop_and_the_length() {
        let fixture = Fixture::new("bgm-select");
        let mut rig = Rig::new();
        let mut panel = BgmPanel::new(None, true);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::NoSong);

        panel.select(Some(id("bgm-0001")), &fixture.view, Some(&rig.bgm));
        rig.settle();
        assert_eq!(panel.selected(), Some(&id("bgm-0001")));
        assert_eq!(panel.song_name(), Some("曲 テスト"));
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);
        assert!(rig.bgm.controller().is_looping(), "the loop is set");
        assert!(
            !rig.bgm.controller().wants_play(),
            "a chosen song is not played"
        );
        assert_eq!(panel.problem(), None);
        let (position, length) = panel.position(&rig.bgm);
        assert_eq!(position, Duration::ZERO);
        assert_eq!(length, Some(Duration::from_millis(250)));
    }

    #[test]
    fn a_player_that_has_done_nothing_is_flushed_and_a_pause_does_not_flush() {
        let fixture = Fixture::new("bgm-flushed");
        let mut rig = Rig::new();
        assert!(rig.bgm.controller().is_flushed(), "just started");
        let mut panel = panel_with_song(&fixture, &mut rig, "bgm-0002");
        assert!(
            rig.bgm.controller().is_flushed(),
            "a song opened and waiting"
        );

        panel.toggle_play(&rig.bgm);
        rig.pump_until("the position to move", |bgm| {
            bgm.controller().position_frames() > 0
        });
        panel.toggle_play(&rig.bgm);
        assert!(rig.bgm.controller().is_flushed(), "a pause is not a flush");
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Paused);
    }

    #[test]
    fn a_stop_is_a_stop_at_once_even_while_the_old_position_is_still_there() {
        let fixture = Fixture::new("bgm-stop-at-once");
        let mut rig = Rig::new();
        let mut panel = panel_with_song(&fixture, &mut rig, "bgm-0002");
        panel.toggle_play(&rig.bgm);
        rig.pump_until("the position to move", |bgm| {
            bgm.controller().position_frames() > 0
        });

        panel.stop(&rig.bgm);
        // The audio thread has not rendered yet: the flush is pending and
        // the position is the old one.
        assert!(!rig.bgm.controller().is_flushed());
        assert!(rig.bgm.controller().position_frames() > 0);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);

        rig.settle();
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);
        assert_eq!(panel.position(&rig.bgm).0, Duration::ZERO);
    }

    #[test]
    fn choosing_another_song_while_playing_stops_and_opens_the_new_song_from_the_start() {
        let fixture = Fixture::new("bgm-select-while-playing");
        let mut rig = Rig::new();
        let mut panel = panel_with_song(&fixture, &mut rig, "bgm-0002");
        panel.toggle_play(&rig.bgm);
        rig.pump_until("the position to move", |bgm| {
            bgm.controller().position_frames() > 0
        });
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Playing);

        panel.select(Some(id("bgm-0001")), &fixture.view, Some(&rig.bgm));
        // Right after the choice, before the audio thread has done the
        // flush: not a pause, though the old position is still there.
        assert!(!rig.bgm.controller().is_flushed());
        assert!(rig.bgm.controller().position_frames() > 0);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);

        rig.settle();
        rig.pump_until("the new song to be open", |bgm| {
            bgm.controller().total_frames() == Some(2_000)
        });
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);
        assert_eq!(panel.position(&rig.bgm).0, Duration::ZERO);
        assert_eq!(panel.song_name(), Some("曲 テスト"));
        assert_eq!(panel.position(&rig.bgm).1, Some(Duration::from_millis(250)));
        assert!(
            !rig.bgm.controller().wants_play(),
            "it does not play by itself"
        );
    }

    #[test]
    fn a_song_plays_pauses_and_plays_again_and_stop_goes_back_to_the_start() {
        let fixture = Fixture::new("bgm-transport");
        let mut rig = Rig::new();
        let mut panel = panel_with_song(&fixture, &mut rig, "bgm-0002");

        panel.toggle_play(&rig.bgm);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Playing);
        rig.pump_until("the position to move", |bgm| {
            bgm.controller().position_frames() > 0
        });
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Playing);

        panel.toggle_play(&rig.bgm);
        assert!(!rig.bgm.controller().wants_play());
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Paused);

        panel.toggle_play(&rig.bgm);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Playing);

        panel.stop(&rig.bgm);
        rig.settle();
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);
        assert_eq!(panel.position(&rig.bgm).0, Duration::ZERO);
    }

    #[test]
    fn a_song_that_ended_is_stopped_before_it_plays_from_the_start() {
        let fixture = Fixture::new("bgm-finished");
        let mut rig = Rig::new();
        let mut panel = panel_with_song(&fixture, &mut rig, "bgm-0001");

        panel.toggle_play(&rig.bgm);
        rig.pump_until("the song to end", |bgm| bgm.controller().is_finished());
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Finished);
        // At its end the position is the length, not more.
        let (position, length) = panel.position(&rig.bgm);
        assert_eq!(Some(position), length);

        panel.toggle_play(&rig.bgm);
        assert!(
            !rig.bgm.controller().is_flushed(),
            "a Stop was asked for before the song played again"
        );
        assert!(rig.bgm.controller().wants_play());
        rig.pump_until("the song to play again", |bgm| {
            bgm.controller().stats().played_frames > 0 && !bgm.controller().is_finished()
        });
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Playing);
    }

    #[test]
    fn the_failure_of_a_song_that_cannot_be_opened_comes_late_through_poll_in_japanese() {
        let fixture = Fixture::new("bgm-failure");
        let mut rig = Rig::new();
        let mut panel = BgmPanel::new(None, false);

        // The library saw the file; it is gone when the worker opens it.
        panel.select(Some(id("bgm-0005")), &fixture.view, Some(&rig.bgm));
        assert_eq!(panel.failure(), None, "load does not wait for the file");
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);

        let mut tries = 0;
        rig.pump_until("the failure", |bgm| {
            panel.poll(bgm);
            tries += 1;
            panel.failure().is_some()
        });
        assert!(tries >= 1);
        let failure = panel.failure().unwrap();
        assert_eq!(
            failure.summary_ja,
            "「vanishing」を再生できません：ファイルが見つかりません"
        );
        assert!(!failure.detail.is_empty());
        assert!(
            !failure.summary_ja.contains("sounds"),
            "no path in the line"
        );
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Failed);
    }

    #[test]
    fn stop_and_a_new_choice_clear_a_failure_and_play_tries_the_song_again() {
        let fixture = Fixture::new("bgm-failure-clear");
        let mut rig = Rig::new();
        let fail = |panel: &mut BgmPanel, rig: &mut Rig| {
            panel.select(Some(id("bgm-0005")), &fixture.view, Some(&rig.bgm));
            rig.pump_until("the failure", |bgm| {
                panel.poll(bgm);
                panel.failure().is_some()
            });
        };
        let mut panel = BgmPanel::new(None, false);

        fail(&mut panel, &mut rig);
        panel.stop(&rig.bgm);
        assert_eq!(panel.failure(), None);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);

        // The worker opens the file again at the Stop and fails again.
        rig.pump_until("the second failure", |bgm| {
            panel.poll(bgm);
            panel.failure().is_some()
        });

        panel.select(Some(id("bgm-0001")), &fixture.view, Some(&rig.bgm));
        assert_eq!(panel.failure(), None, "a new choice clears it");
        rig.settle();

        // Play after a failure: the failure goes and the song is tried again.
        fail(&mut panel, &mut rig);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Failed);
        panel.toggle_play(&rig.bgm);
        assert_eq!(panel.failure(), None);
        assert!(rig.bgm.controller().wants_play());
        assert!(!rig.bgm.controller().is_flushed(), "a Stop was asked for");
    }

    #[test]
    fn a_failure_of_an_older_song_is_not_shown_for_the_new_one() {
        let fixture = Fixture::new("bgm-stale-failure");
        let mut rig = Rig::new();
        // The old song fails in the worker, and the failure is not taken yet.
        let mut panel = BgmPanel::new(None, false);
        panel.select(Some(id("bgm-0005")), &fixture.view, Some(&rig.bgm));
        // A song that cannot be opened is told to the player as an empty song.
        rig.pump_until("the worker to fail", |bgm| {
            bgm.controller().total_frames() == Some(0)
        });
        thread::sleep(Duration::from_millis(30));

        // The user chooses another song before the failure was taken.
        panel.select(Some(id("bgm-0001")), &fixture.view, Some(&rig.bgm));
        panel.poll(&rig.bgm);
        assert_eq!(panel.failure(), None);
        rig.settle();
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);
    }

    #[test]
    fn a_chosen_song_that_cannot_be_used_stays_chosen_and_says_why() {
        let fixture = Fixture::new("bgm-problems");
        let mut rig = Rig::new();
        let cases = [
            (
                "bgm-0003",
                "「gone」のファイルが見つかりません（フォルダから消えた可能性があります）",
            ),
            (
                "bgm-0004",
                "「unread」のファイルを読めません（読めない音源は、取り込みページの更新（再スキャン）で直ることがあります）",
            ),
            ("bgm-0077", "未選択（ID bgm-0077 が見つかりません）"),
            ("se-0001", "未選択（ID se-0001 は曲の ID ではありません）"),
        ];
        for (song, text) in cases {
            let mut panel = BgmPanel::new(None, false);
            panel.select(Some(id(song)), &fixture.view, Some(&rig.bgm));
            assert_eq!(panel.selected(), Some(&id(song)), "{song}");
            assert_eq!(panel.problem(), Some(text), "{song}");
            assert_eq!(panel.song_name(), None, "{song}");
            assert_eq!(panel.transport(Some(&rig.bgm)), Transport::NoSong, "{song}");
        }

        // The library cannot be opened: the ID is kept and the line says so.
        let broken = Fixture::broken("bgm-broken-library");
        let mut panel = BgmPanel::new(Some(id("bgm-0001")), false);
        panel.on_session_started(&broken, &rig.bgm, false);
        assert_eq!(panel.selected(), Some(&id("bgm-0001")));
        assert_eq!(
            panel.problem(),
            Some("ライブラリが使えないため、開けません（ID bgm-0001 は設定に残っています）")
        );
        rig.settle();
    }

    #[test]
    fn choosing_nothing_or_a_song_that_cannot_be_used_stops_the_song_that_was_open() {
        let fixture = Fixture::new("bgm-unselect");
        let mut rig = Rig::new();
        let mut panel = panel_with_song(&fixture, &mut rig, "bgm-0002");
        panel.toggle_play(&rig.bgm);
        assert!(rig.bgm.controller().wants_play());

        panel.select(Some(id("bgm-0003")), &fixture.view, Some(&rig.bgm));
        assert!(!rig.bgm.controller().wants_play(), "the old song stopped");
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::NoSong);
        assert!(panel.problem().is_some());

        let mut panel = panel_with_song(&fixture, &mut rig, "bgm-0002");
        panel.toggle_play(&rig.bgm);
        panel.select(None, &fixture.view, Some(&rig.bgm));
        assert!(!rig.bgm.controller().wants_play());
        assert_eq!(panel.selected(), None);
        assert_eq!(panel.problem(), None);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::NoSong);
    }

    #[test]
    fn a_new_session_gets_the_song_and_the_loop_but_not_the_play_state() {
        let fixture = Fixture::new("bgm-restart");
        let mut old = Rig::new();
        let mut panel = panel_with_song(&fixture, &mut old, "bgm-0002");
        panel.set_looping(true, Some(&old.bgm));
        panel.toggle_play(&old.bgm);
        old.pump_until("the position to move", |bgm| {
            bgm.controller().position_frames() > 0
        });
        let was_playing = panel.was_playing(Some(&old.bgm));
        assert!(was_playing);
        drop(old);

        // The new player has no song and no loop.
        let mut new = Rig::new();
        assert!(!new.bgm.controller().is_looping());
        panel.on_session_started(&fixture.view, &new.bgm, was_playing);
        new.settle();
        assert!(new.bgm.controller().is_looping(), "the loop is set again");
        assert!(!new.bgm.controller().wants_play(), "not played again");
        assert_eq!(panel.transport(Some(&new.bgm)), Transport::Stopped);
        assert_eq!(panel.position(&new.bgm).0, Duration::ZERO);
        assert_eq!(panel.song_name(), Some("other"));
        assert_eq!(
            panel.notice(),
            Some("デバイスを切り替えたため、BGM は止まりました。▶ で頭から再生できます")
        );
        panel.toggle_play(&new.bgm);
        assert_eq!(panel.notice(), None, "the note goes when the user acts");
    }

    #[test]
    fn the_note_about_a_restart_is_only_for_a_song_that_was_playing() {
        let fixture = Fixture::new("bgm-notice");
        let rig = Rig::new();
        let mut panel = BgmPanel::new(Some(id("bgm-0001")), false);
        assert!(!panel.was_playing(Some(&rig.bgm)));
        assert!(!panel.was_playing(None));
        panel.on_session_started(&fixture.view, &rig.bgm, false);
        assert_eq!(panel.notice(), None);
        panel.on_session_started(&fixture.view, &rig.bgm, true);
        assert!(panel.notice().is_some());
        panel.on_session_started(&fixture.view, &rig.bgm, false);
        assert_eq!(panel.notice(), None);
    }

    #[test]
    fn without_a_session_a_choice_is_kept_and_opened_when_one_starts() {
        let fixture = Fixture::new("bgm-no-session");
        let mut panel = BgmPanel::new(None, false);
        panel.select(Some(id("bgm-0001")), &fixture.view, None);
        assert_eq!(panel.selected(), Some(&id("bgm-0001")));
        assert_eq!(panel.problem(), None);
        assert_eq!(panel.transport(None), Transport::NoSong);

        panel.select(Some(id("bgm-0003")), &fixture.view, None);
        assert!(panel.problem().is_some());

        let mut rig = Rig::new();
        panel.select(Some(id("bgm-0001")), &fixture.view, None);
        panel.on_session_started(&fixture.view, &rig.bgm, false);
        rig.settle();
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);
    }

    #[test]
    fn reading_the_library_again_opens_a_chosen_song_that_was_not_usable() {
        let fixture = Fixture::new("bgm-reload");
        let broken = Fixture::broken("bgm-reload-broken");
        let mut rig = Rig::new();
        let mut panel = BgmPanel::new(Some(id("bgm-0001")), false);
        panel.on_session_started(&broken, &rig.bgm, false);
        assert!(panel.problem().is_some());
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::NoSong);

        panel.on_library_reloaded(&fixture.view, Some(&rig.bgm));
        rig.settle();
        assert_eq!(panel.problem(), None);
        assert_eq!(panel.transport(Some(&rig.bgm)), Transport::Stopped);

        // A song that is open is left alone: it does not start over.
        panel.toggle_play(&rig.bgm);
        rig.pump_until("the position to move", |bgm| {
            bgm.controller().position_frames() > 0
        });
        panel.on_library_reloaded(&fixture.view, Some(&rig.bgm));
        assert!(rig.bgm.controller().wants_play());
    }

    #[test]
    fn the_loop_is_set_in_the_player_and_kept_for_the_next_one() {
        let rig = Rig::new();
        let mut panel = BgmPanel::new(None, false);
        panel.set_looping(true, Some(&rig.bgm));
        assert!(panel.looping() && rig.bgm.controller().is_looping());
        panel.set_looping(false, Some(&rig.bgm));
        assert!(!panel.looping() && !rig.bgm.controller().is_looping());
        panel.set_looping(true, None);
        assert!(panel.looping(), "without a player it is kept");
    }

    #[test]
    fn the_panel_can_be_dropped_before_or_after_the_player() {
        let fixture = Fixture::new("bgm-drop");
        let mut rig = Rig::new();
        let panel = panel_with_song(&fixture, &mut rig, "bgm-0001");
        drop(panel);
        let mut rig2 = Rig::new();
        let panel2 = panel_with_song(&fixture, &mut rig2, "bgm-0001");
        drop(rig2);
        drop(panel2);
    }

    // -- the Sound Pad -----------------------------------------------------------------

    /// An SE player that can be told to refuse, and that remembers what it
    /// was asked.
    struct FakeSlots {
        rate: u32,
        limit_bytes: usize,
        sounds: Vec<Option<Arc<SoundData>>>,
        set_calls: Vec<usize>,
        cleared: Vec<usize>,
        plays: Vec<(usize, f32, SePolicy)>,
        play_error: Option<SeError>,
        stop_all_error: Option<SeError>,
        stop_alls: usize,
        collects: usize,
    }

    impl FakeSlots {
        fn new(rate: u32) -> Self {
            Self {
                rate,
                limit_bytes: usize::MAX,
                sounds: vec![None; PAD_COUNT],
                set_calls: Vec::new(),
                cleared: Vec::new(),
                plays: Vec::new(),
                play_error: None,
                stop_all_error: None,
                stop_alls: 0,
                collects: 0,
            }
        }
    }

    impl SoundSlots for FakeSlots {
        fn sample_rate(&self) -> u32 {
            self.rate
        }

        fn set_sound(&mut self, slot: usize, sound: Arc<SoundData>) -> Result<(), SeError> {
            self.set_calls.push(slot);
            if sound.sample_rate() != self.rate {
                return Err(SeError::RateMismatch {
                    expected: self.rate,
                    actual: sound.sample_rate(),
                });
            }
            let others: usize = self
                .sounds
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != slot)
                .filter_map(|(_, sound)| sound.as_ref())
                .map(|sound| sound.memory_bytes())
                .sum();
            let total_bytes = others + sound.memory_bytes();
            if total_bytes > self.limit_bytes {
                return Err(SeError::TotalTooLarge {
                    total_bytes,
                    limit_bytes: self.limit_bytes,
                });
            }
            self.sounds[slot] = Some(sound);
            Ok(())
        }

        fn clear(&mut self, slot: usize) -> Result<(), SeError> {
            self.cleared.push(slot);
            self.sounds[slot] = None;
            Ok(())
        }

        fn play(&mut self, slot: usize, gain: f32, policy: SePolicy) -> Result<(), SeError> {
            if let Some(error) = self.play_error {
                return Err(error);
            }
            self.plays.push((slot, gain, policy));
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), SeError> {
            self.stop_alls += 1;
            self.stop_all_error.map_or(Ok(()), Err)
        }

        fn collect(&mut self) -> usize {
            self.collects += 1;
            0
        }
    }

    fn loader() -> SeLoader {
        SeLoader::start(Box::new(|| {})).unwrap()
    }

    /// Polls until no pad is loading.
    fn settle(bank: &mut PadBank, loader: &SeLoader, se: &mut FakeSlots) {
        let start = Instant::now();
        loop {
            bank.poll(loader, Some(se));
            if !bank.loading() {
                return;
            }
            assert!(start.elapsed() < TIMEOUT, "waited too long for the sounds");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn setting(slot: usize, asset: &str, gain_db: f32) -> PadSetting {
        PadSetting {
            slot,
            asset: id(asset),
            gain_db,
        }
    }

    fn failed(bank: &PadBank, slot: usize) -> String {
        match bank.state(slot) {
            PadState::Failed(text) => text.clone(),
            other => panic!("pad {slot} is {other:?}"),
        }
    }

    #[test]
    fn the_bank_starts_with_the_saved_pads_unread_and_saves_them_as_they_are() {
        let saved = [
            setting(0, "se-0001", -3.0),
            setting(5, "se-0099", 100.0),
            setting(PAD_COUNT, "se-0002", 0.0),
        ];
        let bank = PadBank::new(&saved, SePolicy::Restart);
        assert_eq!(bank.policy(), SePolicy::Restart);
        assert_eq!(bank.asset(0), Some(&id("se-0001")));
        assert_eq!(bank.asset(1), None);
        assert_eq!(bank.state(0), &PadState::Empty);
        assert!(!bank.loading());
        // The volume is kept within the range; a pad past the end is not.
        assert_eq!(
            bank.saved(),
            [setting(0, "se-0001", -3.0), setting(5, "se-0099", 6.0)]
        );
    }

    #[test]
    fn choosing_a_sound_empties_the_slot_then_reads_it_and_makes_it_ready() {
        let fixture = Fixture::new("pad-assign");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        let mut bank = PadBank::new(&[], SePolicy::Overlap);

        bank.assign(
            2,
            Some(id("se-0001")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        assert_eq!(se.cleared, [2], "the old sound leaves first");
        assert_eq!(bank.state(2), &PadState::Loading);
        assert_eq!(bank.name(2), Some("拍手 テスト"));
        assert!(bank.loading());

        settle(&mut bank, &loader, &mut se);
        assert_eq!(
            bank.state(2),
            &PadState::Ready {
                length: Some(Duration::from_millis(125))
            }
        );
        assert_eq!(se.sounds[2].as_ref().unwrap().frames(), 1_000);
        assert!(
            se.collects >= 1,
            "the sounds that came back are dropped here"
        );

        // No sound: the slot is emptied and the pad forgets it.
        bank.assign(2, None, &fixture.view, Some(&mut se), &loader);
        assert_eq!(bank.state(2), &PadState::Empty);
        assert_eq!(bank.asset(2), None);
        assert!(se.sounds[2].is_none());
        assert!(bank.saved().is_empty());
    }

    #[test]
    fn a_result_of_an_older_job_is_thrown_away() {
        let fixture = Fixture::new("pad-stale");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        let mut bank = PadBank::new(&[], SePolicy::Overlap);

        // Two choices for the same pad, one after the other.
        bank.assign(
            0,
            Some(id("se-0001")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        bank.assign(
            0,
            Some(id("se-0002")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        settle(&mut bank, &loader, &mut se);

        assert_eq!(
            se.set_calls,
            [0],
            "the first result was not put in the player"
        );
        assert_eq!(se.sounds[0].as_ref().unwrap().frames(), 2_000);
        assert_eq!(bank.name(0), Some("bell"));
        assert_eq!(bank.asset(0), Some(&id("se-0002")));
    }

    #[test]
    fn a_new_session_reads_every_sound_again_at_its_rate_and_drops_the_old_results() {
        let fixture = Fixture::new("pad-session");
        let loader = loader();
        let mut old = FakeSlots::new(RATE);
        let saved = [setting(0, "se-0001", 0.0), setting(1, "se-0002", 0.0)];
        let mut bank = PadBank::new(&saved, SePolicy::Overlap);

        // Nothing is read before a session; the pads wait.
        assert_eq!(bank.state(0), &PadState::Empty);
        bank.on_session_started(&fixture.view, &mut old, &loader);
        assert_eq!(bank.state(0), &PadState::Loading);
        assert_eq!(bank.state(1), &PadState::Loading);

        // The session is made again at once, at another rate.
        let mut new = FakeSlots::new(16_000);
        bank.on_session_started(&fixture.view, &mut new, &loader);
        settle(&mut bank, &loader, &mut new);

        assert!(old.set_calls.is_empty(), "the old session got nothing");
        assert_eq!(
            new.set_calls,
            [0, 1],
            "one at a time, in the order of the pads"
        );
        for slot in [0, 1] {
            assert_eq!(new.sounds[slot].as_ref().unwrap().sample_rate(), 16_000);
            assert!(matches!(bank.state(slot), PadState::Ready { .. }));
        }
    }

    #[test]
    fn without_a_session_a_choice_is_kept_and_nothing_is_read() {
        let fixture = Fixture::new("pad-no-session");
        let loader = loader();
        let mut bank = PadBank::new(&[], SePolicy::Overlap);
        bank.assign(3, Some(id("se-0001")), &fixture.view, None, &loader);
        assert_eq!(bank.asset(3), Some(&id("se-0001")));
        assert_eq!(bank.state(3), &PadState::Empty);
        assert_eq!(bank.name(3), Some("拍手 テスト"));
        assert!(!bank.loading());
        assert_eq!(bank.saved(), [setting(3, "se-0001", 0.0)]);

        // The session starts: now it is read.
        let mut se = FakeSlots::new(RATE);
        bank.on_session_started(&fixture.view, &mut se, &loader);
        settle(&mut bank, &loader, &mut se);
        assert!(matches!(bank.state(3), PadState::Ready { .. }));
    }

    #[test]
    fn a_result_after_the_session_ended_is_thrown_away() {
        let fixture = Fixture::new("pad-ended");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        let mut bank = PadBank::new(&[setting(0, "se-0001", 0.0)], SePolicy::Overlap);
        bank.on_session_started(&fixture.view, &mut se, &loader);

        // The session is gone when the result comes.
        let start = Instant::now();
        while bank.loading() {
            bank.poll(&loader, None);
            assert!(start.elapsed() < TIMEOUT);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(bank.state(0), &PadState::Empty);
        assert!(se.set_calls.is_empty());
        assert_eq!(bank.asset(0), Some(&id("se-0001")), "the choice stays");
    }

    #[test]
    fn a_sound_that_cannot_be_used_fails_with_a_line_and_keeps_its_id() {
        let fixture = Fixture::new("pad-problems");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        let mut bank = PadBank::new(&[], SePolicy::Overlap);

        let cases = [
            (
                0,
                "se-0003",
                "「gone」のファイルが見つかりません（フォルダから消えた可能性があります）",
            ),
            (
                1,
                "se-0004",
                "「unread」のファイルを読めません（読めない音源は、取り込みページの更新（再スキャン）で直ることがあります）",
            ),
            (2, "se-0099", "未選択（ID se-0099 が見つかりません）"),
            (
                3,
                "bgm-0001",
                "未選択（ID bgm-0001 は SE の ID ではありません）",
            ),
        ];
        for (slot, asset, text) in cases {
            bank.assign(slot, Some(id(asset)), &fixture.view, Some(&mut se), &loader);
            assert_eq!(failed(&bank, slot), text, "{asset}");
            assert_eq!(bank.asset(slot), Some(&id(asset)), "{asset}");
        }
        assert!(!bank.loading(), "nothing was queued");
        assert_eq!(bank.saved().len(), 4, "the IDs are still saved");

        // The library cannot be opened.
        let broken = Fixture::broken("pad-broken-library");
        bank.on_session_started(&broken, &mut se, &loader);
        assert_eq!(
            failed(&bank, 0),
            "ライブラリが使えないため、開けません（ID se-0003 は設定に残っています）"
        );
        assert_eq!(bank.saved().len(), 4);
    }

    #[test]
    fn a_sound_longer_than_the_limit_is_refused_before_it_is_read_when_the_length_is_known() {
        let fixture = Fixture::new("pad-too-long-info");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        let mut bank = PadBank::new(&[], SePolicy::Overlap);

        bank.assign(
            0,
            Some(id("se-0006")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        assert_eq!(
            failed(&bank, 0),
            "「long」は 60 秒より長いため、SE にできません。BGM として登録してください"
        );
        assert!(!bank.loading(), "no job was queued");
        assert!(bank.detail(0).is_some());
        assert_eq!(bank.asset(0), Some(&id("se-0006")));
    }

    #[test]
    fn a_sound_that_turns_out_too_long_or_gone_when_it_is_read_fails_in_japanese() {
        let fixture = Fixture::new("pad-load-errors");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        let mut bank = PadBank::new(&[], SePolicy::Overlap);

        // The library says 1,000 frames; the file says 61 s.
        bank.assign(
            0,
            Some(id("se-0005")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        // The library saw the file, and it is gone when it is read.
        bank.assign(
            1,
            Some(id("se-0007")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        settle(&mut bank, &loader, &mut se);

        assert_eq!(
            failed(&bank, 0),
            "「claims long」は 60 秒より長いため、SE にできません。BGM として登録してください"
        );
        assert_eq!(failed(&bank, 1), "「vanishing」のファイルが見つかりません");
        assert!(bank.detail(1).is_some_and(|detail| !detail.is_empty()));
        assert!(se.set_calls.is_empty());
    }

    #[test]
    fn a_sound_that_is_over_the_total_limit_fails_in_japanese_and_nothing_is_changed() {
        let fixture = Fixture::new("pad-total");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        // 1,000 frames = 8,000 bytes (two f32 channels); room for one only.
        se.limit_bytes = 12_000;
        let mut bank = PadBank::new(&[], SePolicy::Overlap);

        bank.assign(
            0,
            Some(id("se-0001")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        settle(&mut bank, &loader, &mut se);
        assert!(matches!(bank.state(0), PadState::Ready { .. }));

        bank.assign(
            1,
            Some(id("se-0002")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        settle(&mut bank, &loader, &mut se);
        let text = failed(&bank, 1);
        assert!(text.contains("SE の合計が上限"), "{text}");
        assert!(text.contains("「bell」"), "{text}");
        assert!(text.contains("使っていない SE を外してください"), "{text}");
        assert!(se.sounds[1].is_none());
        assert!(
            matches!(bank.state(0), PadState::Ready { .. }),
            "the first is unharmed"
        );
        assert_eq!(bank.asset(1), Some(&id("se-0002")), "the choice stays");
        assert!(bank.detail(1).is_some());
    }

    #[test]
    fn only_a_ready_pad_plays_and_at_the_volume_of_the_pad_with_the_rule_of_the_bank() {
        let fixture = Fixture::new("pad-press");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        let mut bank = PadBank::new(&[], SePolicy::Restart);
        bank.assign(
            0,
            Some(id("se-0001")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );

        // Loading: nothing plays.
        assert_eq!(bank.press(0, &mut se), Press::NotReady);
        // Empty, failed, and a slot that is not there.
        assert_eq!(bank.press(1, &mut se), Press::NotReady);
        bank.assign(
            2,
            Some(id("se-0003")),
            &fixture.view,
            Some(&mut se),
            &loader,
        );
        assert_eq!(bank.press(2, &mut se), Press::NotReady);
        assert_eq!(bank.press(PAD_COUNT, &mut se), Press::NotReady);
        assert!(se.plays.is_empty());

        settle(&mut bank, &loader, &mut se);
        bank.set_gain_db(0, -6.0);
        assert_eq!(bank.press(0, &mut se), Press::Played);
        let (slot, gain, policy) = se.plays[0];
        assert_eq!((slot, policy), (0, SePolicy::Restart));
        assert!(
            (gain - db_to_gain(-6.0)).abs() < 1e-6 && (gain - 0.501).abs() < 1e-3,
            "{gain}"
        );

        bank.set_policy(SePolicy::Ignore);
        bank.set_gain_db(0, 100.0);
        assert_eq!(bank.gain_db(0), 6.0, "within the range of the SE channel");
        assert_eq!(bank.press(0, &mut se), Press::Played);
        assert_eq!(se.plays[1].2, SePolicy::Ignore);
    }

    #[test]
    fn a_player_that_is_busy_or_refuses_is_told_to_the_window() {
        let fixture = Fixture::new("pad-busy");
        let loader = loader();
        let mut se = FakeSlots::new(RATE);
        let mut bank = PadBank::new(&[setting(0, "se-0001", 0.0)], SePolicy::Overlap);
        bank.on_session_started(&fixture.view, &mut se, &loader);
        settle(&mut bank, &loader, &mut se);

        for (error, press) in [
            (SeError::Busy, Press::Busy),
            (SeError::QueueFull, Press::Busy),
            (SeError::NoSound { slot: 0 }, Press::Rejected),
        ] {
            se.play_error = Some(error);
            assert_eq!(bank.press(0, &mut se), press, "{error}");
        }
        assert!(Press::Busy.notice_ja().is_some());
        assert!(Press::Rejected.notice_ja().is_some());
        assert_eq!(Press::Played.notice_ja(), None);
        assert_eq!(Press::NotReady.notice_ja(), None);
        assert!(se.plays.is_empty());
    }

    #[test]
    fn stop_all_goes_to_the_player_and_says_whether_it_was_taken() {
        let mut se = FakeSlots::new(RATE);
        let bank = PadBank::new(&[], SePolicy::Overlap);
        assert!(bank.stop_all(&mut se));
        assert_eq!(se.stop_alls, 1);
        se.stop_all_error = Some(SeError::QueueFull);
        assert!(!bank.stop_all(&mut se));
    }

    #[test]
    fn the_real_controller_is_a_sound_slot() {
        // `SeController` is what the window passes; check that the trait
        // reaches it, with a sound at the rate of the player.
        let (mut controller, _player) = ink_core::graph::se_channel(RATE, BLOCK);
        let slots: &mut dyn SoundSlots = &mut controller;
        assert_eq!(slots.sample_rate(), RATE);
        let sound = Arc::new(SoundData::new(RATE, vec![0.1; 100], vec![0.1; 100]));
        assert!(slots.set_sound(1, Arc::clone(&sound)).is_ok());
        assert!(slots.play(1, 1.0, SePolicy::Overlap).is_ok());
        assert_eq!(
            slots.play(2, 1.0, SePolicy::Overlap),
            Err(SeError::NoSound { slot: 2 })
        );
        assert!(slots.clear(1).is_ok());
        assert!(slots.stop_all().is_ok());
        let _ = slots.collect();
    }
}
