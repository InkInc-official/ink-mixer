//! Saving and restoring the settings of the window (#42, #64): the chosen
//! devices, the levels and mutes of the four channels, the chosen song and
//! the Sound Pad.
//!
//! The file is read and written by `ink_core::Config`. This module decides
//! *what* to restore at startup and *when* to save:
//!
//! - At startup the master volume is the saved value or [`STARTUP_MASTER_DB`],
//!   whichever is lower. The microphone is heard the moment the window opens,
//!   so a loud value saved last time must not start loud. Capping the value
//!   does not change the file: the saved master stays until the user moves
//!   the master fader.
//! - A change is saved when it has settled: the state differs from the saved
//!   one, the mouse button is up (a slider is not being dragged), and the
//!   state has not changed for [`SETTLE`]. [`SaveTracker`] makes this
//!   decision from the times it is given, so it can be tested without a clock.
//! - The BGM and SE levels are restored as they were saved (within their
//!   range). Nothing sounds at the start: a song is not played by itself and a
//!   sound effect is heard only when a pad is pressed.
//! - The devices are saved only when the user chose them. When a saved device
//!   is missing and the default is used instead, the file keeps the saved
//!   device.
//! - The song and the pads are saved as Asset IDs and numbers, never as paths
//!   (AGENTS.md §4, 6). An ID the library does not know, or of a file that is
//!   missing or cannot be read, is kept: it is replaced only when the user
//!   chooses again ([`SoundChoices`]).
//! - When the file could not be read, nothing is saved ([`Settings::disabled`]),
//!   so a broken (or newer) file is never overwritten.
//!
//! Everything here runs on the UI thread. The audio thread never saves
//! (AGENTS.md §5); the UI reads the values from the atomics of `MixerControl`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ink_core::graph::{MixerControl, SePolicy};
use ink_core::{AssetId, BgmConfig, Config, DeviceId, MixerConfig, PadSetting, SoundPadConfig};
use tracing::{info, warn};

use crate::sound_state::{BgmPanel, PAD_COUNT, PadBank};

/// The master volume used when nothing is saved, and the highest master
/// volume the window starts with.
pub const STARTUP_MASTER_DB: f32 = -12.0;
/// The MIC gain used when nothing is saved.
pub const DEFAULT_MIC_GAIN_DB: f32 = 0.0;
/// The BGM volume used when nothing is saved.
pub const DEFAULT_BGM_DB: f32 = 0.0;
/// The SE volume used when nothing is saved.
pub const DEFAULT_SE_DB: f32 = 0.0;
/// How long the state must stay unchanged before it is saved.
pub const SETTLE: Duration = Duration::from_millis(300);
/// The shortest wait before the window looks at a pending save again, so a
/// held mouse button does not make it repaint at once, again and again.
const RECHECK: Duration = Duration::from_millis(50);

/// Returns the master volume to start with: the saved value or
/// [`STARTUP_MASTER_DB`], whichever is lower.
pub fn startup_master_db(saved: Option<f32>) -> f32 {
    saved.map_or(STARTUP_MASTER_DB, |db| db.min(STARTUP_MASTER_DB))
}

/// Sets the controls to the saved values, or to the defaults where nothing
/// is saved. Call this before the audio starts.
pub fn restore(control: &MixerControl, mixer: &MixerConfig) {
    for name in mixer.invalid_values() {
        warn!("saved {name} is not a number; using the default");
    }
    control
        .mic
        .set_gain_db(mixer.mic_gain_db().unwrap_or(DEFAULT_MIC_GAIN_DB));
    control.mic.set_muted(mixer.mic_muted().unwrap_or(false));
    // No cap for BGM and SE: nothing sounds at the start (see the top).
    control
        .bgm
        .set_gain_db(mixer.bgm_db().unwrap_or(DEFAULT_BGM_DB));
    control.bgm.set_muted(mixer.bgm_muted().unwrap_or(false));
    control
        .se
        .set_gain_db(mixer.se_db().unwrap_or(DEFAULT_SE_DB));
    control.se.set_muted(mixer.se_muted().unwrap_or(false));

    let master = startup_master_db(mixer.master_db());
    if let Some(saved) = mixer.master_db()
        && saved > master
    {
        info!("saved master volume {saved:.1} dB is lowered to {master:.1} dB for the start");
    }
    control.master.set_gain_db(master);
}

/// The song and the Sound Pad as they are saved: Asset IDs and numbers only.
///
/// The IDs are the ones the user chose, whether or not the library has them
/// now: [`BgmPanel::selected`] and [`PadBank::saved`] keep an ID that is
/// unknown, whose file is missing, or whose file cannot be read, so that a
/// library that cannot be opened (or a file that is moved back later) does not
/// make the choice disappear.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SoundChoices {
    /// The chosen song.
    pub bgm_asset: Option<AssetId>,
    /// Whether the song loops.
    pub bgm_loop: bool,
    /// The rule for a sound effect that is played again.
    pub pad_policy: SePolicy,
    /// The pads that have a sound effect, by slot.
    pub pads: Vec<PadSetting>,
}

impl SoundChoices {
    /// Reads the choices from the configuration. What is missing is the
    /// default; what cannot be used (a text that is not an ID, an ID of the
    /// other kind, a slot of [`PAD_COUNT`] or more, a repeated slot, an unknown
    /// rule) is left out with a warning, and is gone from the file at the next
    /// save.
    pub fn from_config(config: &Config) -> Self {
        for name in config.bgm.invalid_values() {
            warn!("saved {name} cannot be used; it is left out");
        }
        for name in config.sound_pad.invalid_values(PAD_COUNT) {
            warn!("saved {name} cannot be used; it is left out");
        }
        let mut pads = config.sound_pad.pads(PAD_COUNT);
        // In the order of the slots, as they are saved, so that reading and
        // saving again do not look like a change.
        pads.sort_by_key(|pad| pad.slot);
        Self {
            bgm_asset: config.bgm.asset(),
            bgm_loop: config.bgm.looping().unwrap_or(false),
            pad_policy: config.sound_pad.policy().unwrap_or_default(),
            pads,
        }
    }

    /// Takes the choices of the window.
    pub fn of(panel: &BgmPanel, bank: &PadBank) -> Self {
        Self {
            bgm_asset: panel.selected().copied(),
            bgm_loop: panel.looping(),
            pad_policy: bank.policy(),
            pads: bank.saved(),
        }
    }
}

/// What would be saved: compared with the saved state to find out whether
/// there is anything to save.
#[derive(Debug, Clone, PartialEq)]
pub struct MixerState {
    pub input_device: Option<DeviceId>,
    pub output_device: Option<DeviceId>,
    pub mic_gain_db: f32,
    pub mic_muted: bool,
    pub bgm_db: f32,
    pub bgm_muted: bool,
    pub se_db: f32,
    pub se_muted: bool,
    pub master_db: f32,
    pub sounds: SoundChoices,
}

/// Decides when a changed state is saved. It holds the saved state and the
/// last state it saw, and takes the current time as an argument.
#[derive(Debug)]
pub struct SaveTracker {
    saved: MixerState,
    seen: MixerState,
    changed_at: Instant,
}

impl SaveTracker {
    /// Starts with `state` as both the saved and the current state.
    pub fn new(state: MixerState, now: Instant) -> Self {
        Self {
            saved: state.clone(),
            seen: state,
            changed_at: now,
        }
    }

    /// Looks at the state of this frame and returns whether to save it now:
    /// it differs from the saved state, no mouse button is down, and it has
    /// not changed for [`SETTLE`].
    pub fn update(&mut self, current: &MixerState, now: Instant, pointer_down: bool) -> bool {
        if *current != self.seen {
            self.seen = current.clone();
            self.changed_at = now;
        }
        *current != self.saved
            && !pointer_down
            && now.saturating_duration_since(self.changed_at) >= SETTLE
    }

    /// Records that `state` was saved, or that saving it failed. Either way
    /// it is not tried again until the state changes.
    pub fn mark_saved(&mut self, state: MixerState) {
        self.saved = state;
    }

    /// Returns whether the last state seen differs from the saved one.
    pub fn pending(&self) -> bool {
        self.seen != self.saved
    }

    /// Returns how long to wait before looking again, if a save is pending.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.pending().then(|| {
            let elapsed = now.saturating_duration_since(self.changed_at);
            SETTLE.saturating_sub(elapsed).max(RECHECK)
        })
    }

    fn saved(&self) -> &MixerState {
        &self.saved
    }
}

/// Where the settings go.
#[derive(Debug)]
enum Target {
    File(PathBuf),
    /// Saving is off, and why.
    Disabled(String),
}

/// The settings of this run: what was loaded, what the user chose since, and
/// when to save.
#[derive(Debug)]
pub struct Settings {
    target: Target,
    /// The configuration as last read or written.
    config: Config,
    /// The devices to save: the saved ones until the user chooses others.
    input_device: Option<DeviceId>,
    output_device: Option<DeviceId>,
    /// The song and the pads to save: the saved ones until the window tells
    /// the choices.
    sounds: SoundChoices,
    /// The master volume the window started with.
    startup_master_db: f32,
    /// Set once the master volume differs from the start value; from then
    /// on the current value is saved instead of the one in the file.
    master_touched: bool,
    tracker: SaveTracker,
}

impl Settings {
    /// Creates settings that are saved to `path`. Call this after
    /// [`restore`], with the controls as they were restored.
    pub fn new(config: Config, path: PathBuf, control: &MixerControl, now: Instant) -> Self {
        Self::with_target(config, Target::File(path), control, now)
    }

    /// Creates settings that are never saved (the file could not be read, or
    /// there is no place for it), with the reason to show.
    pub fn disabled(
        config: Config,
        reason: impl Into<String>,
        control: &MixerControl,
        now: Instant,
    ) -> Self {
        Self::with_target(config, Target::Disabled(reason.into()), control, now)
    }

    fn with_target(config: Config, target: Target, control: &MixerControl, now: Instant) -> Self {
        let mut settings = Self {
            input_device: config.audio.input_device.clone(),
            output_device: config.audio.output_device.clone(),
            sounds: SoundChoices::from_config(&config),
            startup_master_db: control.master.gain_db(),
            master_touched: false,
            tracker: SaveTracker::new(
                MixerState {
                    input_device: None,
                    output_device: None,
                    mic_gain_db: 0.0,
                    mic_muted: false,
                    bgm_db: 0.0,
                    bgm_muted: false,
                    se_db: 0.0,
                    se_muted: false,
                    master_db: 0.0,
                    sounds: SoundChoices::default(),
                },
                now,
            ),
            config,
            target,
        };
        let state = settings.state(control);
        settings.tracker = SaveTracker::new(state, now);
        settings
    }

    /// Returns why saving is off, or `None` when settings are saved.
    pub fn disabled_reason(&self) -> Option<&str> {
        match &self.target {
            Target::Disabled(reason) => Some(reason),
            Target::File(_) => None,
        }
    }

    /// Records the devices the user chose. Call this only when the user
    /// chose them, not when the default was used instead of a missing one.
    pub fn set_chosen_devices(&mut self, input: Option<DeviceId>, output: Option<DeviceId>) {
        self.input_device = input;
        self.output_device = output;
    }

    /// Returns the song and the pads that are saved (or to be saved), for the
    /// window to start with.
    pub fn sound_choices(&self) -> &SoundChoices {
        &self.sounds
    }

    /// Records the song and the pads as the window has them now. Unlike the
    /// devices this may be called every frame: only a change is saved.
    pub fn set_sound_choices(&mut self, choices: SoundChoices) {
        self.sounds = choices;
    }

    /// Reads the state to save from the controls.
    fn state(&mut self, control: &MixerControl) -> MixerState {
        let master = control.master.gain_db();
        if master != self.startup_master_db {
            self.master_touched = true;
        }
        // Until the user moves the master fader, the file keeps its value
        // (the start value may have been lowered from it).
        let master_db = if self.master_touched {
            master
        } else {
            self.config.mixer.master_db().unwrap_or(master)
        };
        MixerState {
            input_device: self.input_device.clone(),
            output_device: self.output_device.clone(),
            mic_gain_db: control.mic.gain_db(),
            mic_muted: control.mic.is_muted(),
            bgm_db: control.bgm.gain_db(),
            bgm_muted: control.bgm.is_muted(),
            se_db: control.se.gain_db(),
            se_muted: control.se.is_muted(),
            master_db,
            sounds: self.sounds.clone(),
        }
    }

    /// Call once per frame. Saves when the changes have settled and returns
    /// the result of the save, or `None` when nothing was saved.
    pub fn tick(
        &mut self,
        control: &MixerControl,
        now: Instant,
        pointer_down: bool,
    ) -> Option<Result<(), String>> {
        if self.disabled_reason().is_some() {
            return None;
        }
        let state = self.state(control);
        self.tracker
            .update(&state, now, pointer_down)
            .then(|| self.write(state))
    }

    /// Saves any unsaved change at once, without waiting. Call this when
    /// the window closes.
    pub fn flush(&mut self, control: &MixerControl) -> Option<Result<(), String>> {
        if self.disabled_reason().is_some() {
            return None;
        }
        let state = self.state(control);
        (state != *self.tracker.saved()).then(|| self.write(state))
    }

    /// Returns how long to wait before the next [`tick`](Self::tick) if a
    /// save is pending, so that the window repaints in time.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        match self.target {
            Target::File(_) => self.tracker.wait(now),
            Target::Disabled(_) => None,
        }
    }

    fn write(&mut self, state: MixerState) -> Result<(), String> {
        let Target::File(path) = &self.target else {
            return Ok(());
        };
        let mut config = self.config.clone();
        config.audio.input_device = state.input_device.clone();
        config.audio.output_device = state.output_device.clone();
        config.mixer = MixerConfig::new(state.mic_gain_db, state.mic_muted, state.master_db)
            .with_bgm(state.bgm_db, state.bgm_muted)
            .with_se(state.se_db, state.se_muted);
        config.bgm = BgmConfig::new(state.sounds.bgm_asset.as_ref(), state.sounds.bgm_loop);
        config.sound_pad = SoundPadConfig::new(state.sounds.pad_policy, &state.sounds.pads);

        let result = config.save(path).map_err(|e| e.to_string());
        // A failed save is not tried again until the state changes.
        self.tracker.mark_saved(state);
        if result.is_ok() {
            self.config = config;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_view::{LibraryView, Resolved};
    use crate::se_loader::SeLoader;
    use ink_core::AssetKind;
    use std::fs;
    use std::path::Path;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn state() -> MixerState {
        MixerState {
            input_device: Some(DeviceId::new("a:in")),
            output_device: Some(DeviceId::new("a:out")),
            mic_gain_db: 0.0,
            mic_muted: false,
            bgm_db: 0.0,
            bgm_muted: false,
            se_db: 0.0,
            se_muted: false,
            master_db: -12.0,
            sounds: SoundChoices::default(),
        }
    }

    /// A fresh directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ink-mixer-settings-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// The mixer as it is saved when BGM and SE were not touched.
    fn with_default_sounds(mixer: MixerConfig) -> MixerConfig {
        mixer
            .with_bgm(DEFAULT_BGM_DB, false)
            .with_se(DEFAULT_SE_DB, false)
    }

    fn config_with_master(db: f32) -> Config {
        Config {
            mixer: MixerConfig::new(3.5, false, db),
            ..Config::default()
        }
    }

    /// Restores `config` into new controls and creates settings saved to
    /// `path`, as the window does at startup.
    fn start(config: Config, path: &Path, now: Instant) -> (MixerControl, Settings) {
        let control = MixerControl::new();
        restore(&control, &config.mixer);
        let settings = Settings::new(config, path.to_path_buf(), &control, now);
        (control, settings)
    }

    // -- startup values ----------------------------------------------------

    #[test]
    fn the_master_starts_at_the_lower_of_the_saved_value_and_minus_12() {
        assert_eq!(startup_master_db(None), -12.0);
        assert_eq!(startup_master_db(Some(6.0)), -12.0);
        assert_eq!(startup_master_db(Some(3.0)), -12.0);
        assert_eq!(startup_master_db(Some(0.0)), -12.0);
        assert_eq!(startup_master_db(Some(-12.0)), -12.0);
        assert_eq!(startup_master_db(Some(-20.0)), -20.0);
        assert_eq!(startup_master_db(Some(-60.0)), -60.0);
    }

    #[test]
    fn restore_sets_the_saved_values_and_caps_the_master() {
        let control = MixerControl::new();
        restore(&control, &MixerConfig::new(3.5, true, 6.0));
        assert_eq!(control.mic.gain_db(), 3.5);
        assert!(control.mic.is_muted());
        assert_eq!(control.master.gain_db(), -12.0, "+6 starts at -12");

        restore(&control, &MixerConfig::new(-4.0, false, -20.0));
        assert_eq!(control.mic.gain_db(), -4.0);
        assert!(!control.mic.is_muted());
        assert_eq!(control.master.gain_db(), -20.0, "-20 starts at -20");
    }

    #[test]
    fn restore_uses_the_defaults_where_nothing_is_saved() {
        let control = MixerControl::new();
        control.mic.set_gain_db(10.0);
        control.mic.set_muted(true);
        restore(&control, &MixerConfig::default());
        assert_eq!(control.mic.gain_db(), 0.0);
        assert!(!control.mic.is_muted());
        assert_eq!(control.master.gain_db(), -12.0);
    }

    #[test]
    fn restore_does_not_start_silent_when_a_saved_value_is_not_a_number() {
        let dir = test_dir("nan");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "version = 1\n[mixer]\nmic_gain_db = nan\nmaster_db = nan\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        let control = MixerControl::new();
        restore(&control, &config.mixer);
        assert_eq!(control.mic.gain_db(), 0.0, "NaN would have been -60 dB");
        assert_eq!(control.master.gain_db(), -12.0);
    }

    // -- SaveTracker -------------------------------------------------------

    #[test]
    fn a_change_is_saved_only_after_it_has_settled() {
        let t0 = Instant::now();
        let mut tracker = SaveTracker::new(state(), t0);
        assert!(!tracker.update(&state(), at(t0, 10), false), "no change");
        assert_eq!(tracker.wait(at(t0, 10)), None);

        let mut changed = state();
        changed.mic_gain_db = 3.0;
        assert!(
            !tracker.update(&changed, at(t0, 100), false),
            "just changed"
        );
        assert!(tracker.pending());
        assert_eq!(tracker.wait(at(t0, 100)), Some(Duration::from_millis(300)));
        assert!(!tracker.update(&changed, at(t0, 399), false));
        assert!(tracker.update(&changed, at(t0, 400), false), "300 ms later");
    }

    #[test]
    fn nothing_is_saved_while_the_mouse_button_is_down() {
        let t0 = Instant::now();
        let mut tracker = SaveTracker::new(state(), t0);
        let mut changed = state();
        changed.master_db = -20.0;
        // A slider is being dragged for two seconds, in steps.
        for k in 1..=20u32 {
            changed.master_db = -20.0 + k as f32;
            assert!(!tracker.update(&changed, at(t0, 100 * u64::from(k)), true));
        }
        // The button is released: the last move is only just old enough.
        assert!(!tracker.update(&changed, at(t0, 2100), false));
        assert!(tracker.update(&changed, at(t0, 2300), false));
        // While the button is held, the wait never drops below a short time.
        assert!(tracker.wait(at(t0, 9000)).unwrap() >= RECHECK);
    }

    #[test]
    fn every_move_restarts_the_wait() {
        let t0 = Instant::now();
        let mut tracker = SaveTracker::new(state(), t0);
        let mut changed = state();
        for k in 1..=5u32 {
            changed.mic_gain_db = k as f32;
            // Changing every 200 ms never lets 300 ms pass.
            assert!(!tracker.update(&changed, at(t0, 200 * u64::from(k)), false));
        }
        assert!(tracker.update(&changed, at(t0, 1300), false));
    }

    #[test]
    fn a_saved_state_is_not_saved_again_and_a_failure_waits_for_the_next_change() {
        let t0 = Instant::now();
        let mut tracker = SaveTracker::new(state(), t0);
        let mut changed = state();
        changed.mic_muted = true;
        assert!(!tracker.update(&changed, t0, false));
        assert!(tracker.update(&changed, at(t0, 300), false));

        // Saved (or the save failed): the same state does not come again.
        tracker.mark_saved(changed.clone());
        assert!(!tracker.update(&changed, at(t0, 400), false));
        assert!(!tracker.pending());

        // The next change is tried again.
        let original = state();
        assert!(!tracker.update(&original, at(t0, 500), false));
        assert!(tracker.update(&original, at(t0, 800), false));
    }

    // -- Settings ----------------------------------------------------------

    #[test]
    fn nothing_is_written_at_startup_or_while_nothing_changes() {
        let dir = test_dir("quiet");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(-6.0), &path, t0);

        for k in 0..20u64 {
            assert!(settings.tick(&control, at(t0, 100 * k), false).is_none());
        }
        assert!(settings.flush(&control).is_none());
        assert!(!path.exists());
        assert_eq!(settings.wait(at(t0, 3000)), None);
    }

    #[test]
    fn a_settled_change_is_written_with_all_values() {
        let dir = test_dir("write");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let mut config = config_with_master(-6.0);
        config.audio.input_device = Some(DeviceId::new("a:in"));
        let (control, mut settings) = start(config, &path, t0);

        control.mic.set_gain_db(7.0);
        control.mic.set_muted(true);
        assert!(settings.tick(&control, at(t0, 100), false).is_none());
        assert!(settings.wait(at(t0, 100)).is_some());
        assert_eq!(settings.tick(&control, at(t0, 400), false), Some(Ok(())));

        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.mixer,
            with_default_sounds(MixerConfig::new(7.0, true, -6.0))
        );
        assert_eq!(saved.audio.input_device, Some(DeviceId::new("a:in")));
        assert_eq!(settings.wait(at(t0, 500)), None);
        assert!(settings.tick(&control, at(t0, 600), false).is_none());
    }

    #[test]
    fn a_loud_saved_master_is_lowered_at_start_but_stays_in_the_file() {
        let dir = test_dir("capped");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(3.0), &path, t0);
        assert_eq!(control.master.gain_db(), -12.0);

        // Nothing is saved just because the master was lowered.
        for k in 0..10u64 {
            assert!(settings.tick(&control, at(t0, 100 * k), false).is_none());
        }
        assert!(!path.exists());

        // Changing something else saves it, with the master as in the file.
        control.mic.set_gain_db(-3.0);
        assert_eq!(settings.tick(&control, at(t0, 1500), false), None);
        assert_eq!(settings.tick(&control, at(t0, 1800), false), Some(Ok(())));
        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.mixer,
            with_default_sounds(MixerConfig::new(-3.0, false, 3.0))
        );
        assert_eq!(control.master.gain_db(), -12.0);
    }

    #[test]
    fn once_the_master_is_moved_its_value_is_saved_even_back_at_minus_12() {
        let dir = test_dir("touched");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(3.0), &path, t0);

        control.master.set_gain_db(-30.0);
        assert!(settings.tick(&control, at(t0, 100), false).is_none());
        control.master.set_gain_db(-12.0);
        assert!(settings.tick(&control, at(t0, 200), false).is_none());
        assert_eq!(settings.tick(&control, at(t0, 500), false), Some(Ok(())));
        assert_eq!(Config::load(&path).unwrap().mixer.master_db(), Some(-12.0));
    }

    #[test]
    fn a_master_below_minus_12_is_kept_as_saved() {
        let dir = test_dir("quiet-master");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(-20.0), &path, t0);
        assert_eq!(control.master.gain_db(), -20.0, "-20 starts at -20");
        control.mic.set_muted(true);
        assert_eq!(settings.flush(&control), Some(Ok(())));
        assert_eq!(Config::load(&path).unwrap().mixer.master_db(), Some(-20.0));
    }

    #[test]
    fn devices_are_saved_only_when_the_user_chose_them() {
        let dir = test_dir("devices");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let mut config = Config::default();
        config.audio.input_device = Some(DeviceId::new("usb:headset"));
        config.audio.output_device = Some(DeviceId::new("usb:headset"));
        // The headset is unplugged: the window starts with the defaults, but
        // does not tell the settings, so the file keeps the headset.
        let (control, mut settings) = start(config, &path, t0);
        control.mic.set_gain_db(2.0);
        assert_eq!(settings.flush(&control), Some(Ok(())));
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.audio.input_device, Some(DeviceId::new("usb:headset")));
        assert_eq!(
            saved.audio.output_device,
            Some(DeviceId::new("usb:headset"))
        );

        // The user chooses another device: now it is saved.
        settings.set_chosen_devices(Some(DeviceId::new("alsa:default")), None);
        assert!(settings.tick(&control, at(t0, 0), false).is_none());
        assert_eq!(settings.flush(&control), Some(Ok(())));
        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.audio.input_device,
            Some(DeviceId::new("alsa:default"))
        );
        assert_eq!(saved.audio.output_device, None);
    }

    #[test]
    fn a_change_at_exit_is_saved_without_waiting() {
        let dir = test_dir("exit");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(-6.0), &path, t0);
        control.mic.set_gain_db(9.0);
        // The change is only 10 ms old, but the window is closing.
        assert!(settings.tick(&control, at(t0, 10), false).is_none());
        assert_eq!(settings.flush(&control), Some(Ok(())));
        assert_eq!(Config::load(&path).unwrap().mixer.mic_gain_db(), Some(9.0));
        assert!(settings.flush(&control).is_none(), "nothing left to save");
    }

    #[test]
    fn a_failed_save_is_reported_and_tried_again_only_after_the_next_change() {
        let dir = test_dir("failure");
        fs::create_dir_all(&dir).unwrap();
        // The "directory" of the config file is a regular file.
        let blocker = dir.join("blocker");
        fs::write(&blocker, "x").unwrap();
        let path = blocker.join("config.toml");

        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(-6.0), &path, t0);
        control.mic.set_gain_db(4.0);
        assert!(settings.tick(&control, at(t0, 0), false).is_none());
        let result = settings.tick(&control, at(t0, 400), false);
        assert!(matches!(result, Some(Err(_))), "{result:?}");

        // The same state is not tried every frame.
        assert!(settings.tick(&control, at(t0, 500), false).is_none());
        assert!(settings.flush(&control).is_none());

        // The place becomes usable; the next change is saved.
        fs::remove_file(&blocker).unwrap();
        control.mic.set_gain_db(5.0);
        assert!(settings.tick(&control, at(t0, 600), false).is_none());
        assert_eq!(settings.tick(&control, at(t0, 900), false), Some(Ok(())));
        assert_eq!(Config::load(&path).unwrap().mixer.mic_gain_db(), Some(5.0));
    }

    #[test]
    fn disabled_settings_never_save_and_say_why() {
        let t0 = Instant::now();
        let control = MixerControl::new();
        let mut settings = Settings::disabled(Config::default(), "cannot read", &control, t0);
        assert_eq!(settings.disabled_reason(), Some("cannot read"));

        control.mic.set_gain_db(8.0);
        assert!(settings.tick(&control, at(t0, 0), false).is_none());
        assert!(settings.tick(&control, at(t0, 1000), false).is_none());
        assert!(settings.flush(&control).is_none());
        assert_eq!(settings.wait(at(t0, 1000)), None);
    }

    // -- BGM and SE levels --------------------------------------------------

    #[test]
    fn restore_sets_the_bgm_and_se_levels_as_saved_and_only_the_master_is_capped() {
        let control = MixerControl::new();
        let mixer = MixerConfig::new(0.0, false, 6.0)
            .with_bgm(6.0, true)
            .with_se(6.0, true);
        restore(&control, &mixer);
        assert_eq!(control.bgm.gain_db(), 6.0, "no cap for BGM");
        assert!(control.bgm.is_muted());
        assert_eq!(control.se.gain_db(), 6.0, "no cap for SE");
        assert!(control.se.is_muted());
        assert_eq!(control.master.gain_db(), -12.0);

        restore(
            &control,
            &MixerConfig::new(0.0, false, -20.0)
                .with_bgm(-30.0, false)
                .with_se(-1.5, false),
        );
        assert_eq!(control.bgm.gain_db(), -30.0);
        assert!(!control.bgm.is_muted());
        assert_eq!(control.se.gain_db(), -1.5);
        assert!(!control.se.is_muted());
    }

    #[test]
    fn restore_uses_the_default_bgm_and_se_levels_where_nothing_is_saved() {
        let control = MixerControl::new();
        control.bgm.set_gain_db(-20.0);
        control.bgm.set_muted(true);
        control.se.set_gain_db(-20.0);
        control.se.set_muted(true);
        // An old file: only the MIC and the master are in it.
        restore(&control, &MixerConfig::new(1.0, false, -6.0));
        assert_eq!(control.bgm.gain_db(), DEFAULT_BGM_DB);
        assert!(!control.bgm.is_muted());
        assert_eq!(control.se.gain_db(), DEFAULT_SE_DB);
        assert!(!control.se.is_muted());
    }

    #[test]
    fn restore_does_not_start_silent_when_a_saved_bgm_or_se_level_is_not_a_number() {
        let dir = test_dir("nan-sounds");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "version = 1\n[mixer]\nbgm_db = nan\nse_db = nan\n").unwrap();
        let config = Config::load(&path).unwrap();
        let control = MixerControl::new();
        restore(&control, &config.mixer);
        assert_eq!(control.bgm.gain_db(), DEFAULT_BGM_DB, "NaN would be -60 dB");
        assert_eq!(control.se.gain_db(), DEFAULT_SE_DB);
    }

    #[test]
    fn the_master_is_at_most_minus_12_at_the_start_whatever_else_is_saved() {
        let dir = test_dir("master-cap");
        let t0 = Instant::now();
        for master in [
            None,
            Some(6.0),
            Some(0.0),
            Some(-11.9),
            Some(-12.0),
            Some(-30.0),
        ] {
            for sounds in [-60.0, 0.0, 6.0] {
                let mut mixer = MixerConfig::default()
                    .with_bgm(sounds, false)
                    .with_se(sounds, false);
                if let Some(db) = master {
                    mixer = MixerConfig::new(6.0, false, db)
                        .with_bgm(sounds, false)
                        .with_se(sounds, false);
                }
                let config = Config {
                    mixer,
                    ..Config::default()
                };
                let (control, _settings) = start(config, &dir.join("config.toml"), t0);
                assert!(
                    control.master.gain_db() <= STARTUP_MASTER_DB,
                    "{master:?} {sounds}: {}",
                    control.master.gain_db()
                );
                assert_eq!(control.bgm.gain_db(), sounds, "BGM is as saved");
                assert_eq!(control.se.gain_db(), sounds, "SE is as saved");
            }
        }
        // A saved master that is not a number is not a loud start either.
        let nan = MixerConfig::new(0.0, false, f32::NAN);
        let (control, _settings) = start(
            Config {
                mixer: nan,
                ..Config::default()
            },
            &dir.join("config.toml"),
            t0,
        );
        assert!(control.master.gain_db() <= STARTUP_MASTER_DB);
    }

    #[test]
    fn the_bgm_and_se_levels_are_saved_even_at_plus_6_and_come_back() {
        let dir = test_dir("sound-levels");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(-6.0), &path, t0);
        control.bgm.set_gain_db(6.0);
        control.bgm.set_muted(true);
        control.se.set_gain_db(-9.5);
        assert!(settings.tick(&control, at(t0, 100), false).is_none());
        assert_eq!(settings.tick(&control, at(t0, 400), false), Some(Ok(())));

        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.mixer.bgm_db(), Some(6.0));
        assert_eq!(saved.mixer.bgm_muted(), Some(true));
        assert_eq!(saved.mixer.se_db(), Some(-9.5));
        assert_eq!(saved.mixer.se_muted(), Some(false));

        let restored = MixerControl::new();
        restore(&restored, &saved.mixer);
        assert_eq!(restored.bgm.gain_db(), 6.0);
        assert!(restored.bgm.is_muted());
        assert_eq!(restored.se.gain_db(), -9.5);
        assert!(!restored.se.is_muted());
        assert_eq!(restored.master.gain_db(), -12.0, "the master is lowered");
    }

    // -- the song and the Sound Pad -------------------------------------------

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    fn pad(slot: usize, asset: &str, gain_db: f32) -> PadSetting {
        PadSetting {
            slot,
            asset: id(asset),
            gain_db,
        }
    }

    fn write_config(name: &str, text: &str) -> (PathBuf, Config) {
        let dir = test_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, text).unwrap();
        let config = Config::load(&path).unwrap();
        (path, config)
    }

    fn index_entry(id: &str, kind: &str, file: &str, read: bool) -> String {
        let info = if read {
            "\n[asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 1\nframes = 100\n"
        } else {
            ""
        };
        format!("[[asset]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{file}\"\n{info}\n")
    }

    /// A library with, for songs and for sound effects, `-0001` that can be
    /// used, `-0002` whose file is gone, and `-0003` that was never read
    /// (the library cannot read it). `-0099` is not in it.
    fn library(name: &str) -> (PathBuf, LibraryView) {
        let root = test_dir(name);
        let mut index = String::from("version = 1\nnext_id = 100\n\n");
        for (kind, prefix) in [("bgm", "bgm"), ("se", "se")] {
            let folder = root.join("sounds").join(kind);
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("here.wav"), "x").unwrap();
            fs::write(folder.join("unread.wav"), "x").unwrap();
            index += &index_entry(&format!("{prefix}-0001"), kind, "here.wav", true);
            index += &index_entry(&format!("{prefix}-0002"), kind, "gone.wav", true);
            index += &index_entry(&format!("{prefix}-0003"), kind, "unread.wav", false);
        }
        fs::write(root.join("library.toml"), index).unwrap();
        let view = LibraryView::open(Some(&root));
        (root, view)
    }

    #[test]
    fn the_library_of_the_tests_has_every_state() {
        let (_root, view) = library("states");
        let song = |text: &str| view.resolve(&id(text), AssetKind::Bgm);
        assert!(matches!(song("bgm-0001"), Resolved::Found { .. }));
        assert!(matches!(song("bgm-0002"), Resolved::Missing { .. }));
        assert!(matches!(song("bgm-0003"), Resolved::Unreadable { .. }));
        assert!(matches!(song("bgm-0099"), Resolved::Unknown));
        let sound = |text: &str| view.resolve(&id(text), AssetKind::Se);
        assert!(matches!(sound("se-0001"), Resolved::Found { .. }));
        assert!(matches!(sound("se-0002"), Resolved::Missing { .. }));
        assert!(matches!(sound("se-0003"), Resolved::Unreadable { .. }));
        assert!(matches!(sound("se-0099"), Resolved::Unknown));
    }

    #[test]
    fn what_is_not_in_the_file_is_the_default_choice() {
        assert_eq!(
            SoundChoices::from_config(&Config::default()),
            SoundChoices {
                bgm_asset: None,
                bgm_loop: false,
                pad_policy: SePolicy::Overlap,
                pads: Vec::new(),
            }
        );
        // An old file that has only the mixer and the devices.
        let (_path, config) = write_config("old-file", "version = 1\n[mixer]\nmaster_db = -6.0\n");
        assert_eq!(SoundChoices::from_config(&config), SoundChoices::default());

        // Only the loop is saved: the song and the pads are still the default.
        let (_path, config) = write_config("loop-only", "version = 1\n[bgm]\nloop = true\n");
        let choices = SoundChoices::from_config(&config);
        assert_eq!(choices.bgm_asset, None);
        assert!(choices.bgm_loop);
        assert_eq!(choices.pad_policy, SePolicy::Overlap);
        assert!(choices.pads.is_empty());
    }

    #[test]
    fn the_choices_are_read_in_the_order_of_the_slots_and_what_cannot_be_used_is_left_out() {
        let (_path, config) = write_config(
            "unusable",
            "version = 1\n\
             [bgm]\nasset = \"se-0001\"\nloop = true\n\
             [sound_pad]\npolicy = \"bogus\"\n\
             [[sound_pad.pad]]\nslot = 3\nasset = \"se-0003\"\ngain_db = -3.0\n\
             [[sound_pad.pad]]\nslot = 0\nasset = \"se-0001\"\ngain_db = 2.0\n\
             [[sound_pad.pad]]\nslot = 8\nasset = \"se-0002\"\ngain_db = 0.0\n\
             [[sound_pad.pad]]\nslot = 0\nasset = \"se-0002\"\ngain_db = 0.0\n\
             [[sound_pad.pad]]\nslot = 5\nasset = \"bgm-0001\"\ngain_db = 0.0\n",
        );
        let choices = SoundChoices::from_config(&config);
        assert_eq!(choices.bgm_asset, None, "the ID of a sound effect");
        assert!(choices.bgm_loop);
        assert_eq!(choices.pad_policy, SePolicy::Overlap, "an unknown rule");
        assert_eq!(
            choices.pads,
            vec![pad(0, "se-0001", 2.0), pad(3, "se-0003", -3.0)],
            "by slot; the slot 8, the repeated slot 0 and the song are left out"
        );
    }

    #[test]
    fn a_pad_beyond_the_last_slot_is_left_out_and_is_gone_from_the_file_at_the_next_save() {
        let (path, config) = write_config(
            "beyond",
            "version = 1\n\
             [[sound_pad.pad]]\nslot = 1\nasset = \"se-0001\"\ngain_db = 0.0\n\
             [[sound_pad.pad]]\nslot = 8\nasset = \"se-0002\"\ngain_db = 0.0\n\
             [[sound_pad.pad]]\nslot = 12\nasset = \"se-0003\"\ngain_db = 0.0\n",
        );
        assert_eq!(PAD_COUNT, 8, "the test is written for 8 pads");
        let t0 = Instant::now();
        let (control, mut settings) = start(config, &path, t0);
        assert_eq!(settings.sound_choices().pads, vec![pad(1, "se-0001", 0.0)]);

        control.mic.set_gain_db(2.0);
        assert_eq!(settings.flush(&control), Some(Ok(())));
        let reloaded = SoundChoices::from_config(&Config::load(&path).unwrap());
        assert_eq!(reloaded.pads, vec![pad(1, "se-0001", 0.0)]);
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("se-0002") && !text.contains("se-0003"),
            "{text}"
        );
    }

    #[test]
    fn the_choices_of_the_window_keep_an_id_that_cannot_be_used() {
        let (_root, view) = library("window-choices");
        let loader = SeLoader::start(Box::new(|| {})).unwrap();
        for song in ["bgm-0099", "bgm-0002", "bgm-0003"] {
            let mut panel = BgmPanel::new(None, true);
            panel.select(Some(id(song)), &view, None);
            let mut bank = PadBank::new(&[], SePolicy::Restart);
            bank.assign(0, Some(id("se-0099")), &view, None, &loader);
            bank.assign(1, Some(id("se-0002")), &view, None, &loader);
            bank.assign(2, Some(id("se-0003")), &view, None, &loader);
            bank.assign(3, Some(id("se-0001")), &view, None, &loader);
            bank.set_gain_db(3, -4.5);

            let choices = SoundChoices::of(&panel, &bank);
            assert_eq!(choices.bgm_asset, Some(id(song)), "{song}");
            assert!(choices.bgm_loop);
            assert_eq!(choices.pad_policy, SePolicy::Restart);
            assert_eq!(
                choices.pads,
                vec![
                    pad(0, "se-0099", 0.0),
                    pad(1, "se-0002", 0.0),
                    pad(2, "se-0003", 0.0),
                    pad(3, "se-0001", -4.5),
                ],
                "{song}"
            );
        }
        // Nothing chosen: nothing is saved for the song.
        let panel = BgmPanel::new(None, false);
        let bank = PadBank::new(&[], SePolicy::Overlap);
        assert_eq!(SoundChoices::of(&panel, &bank), SoundChoices::default());
    }

    #[test]
    fn an_id_that_cannot_be_used_stays_after_saving_and_reading_again() {
        let (_root, view) = library("survive");
        let loader = SeLoader::start(Box::new(|| {})).unwrap();
        for song in ["bgm-0099", "bgm-0002", "bgm-0003"] {
            let dir = test_dir(&format!("survive-{song}"));
            let path = dir.join("config.toml");
            let t0 = Instant::now();
            let (control, mut settings) = start(Config::default(), &path, t0);

            let mut panel = BgmPanel::new(None, true);
            panel.select(Some(id(song)), &view, None);
            let mut bank = PadBank::new(&[], SePolicy::Ignore);
            for (slot, sound) in ["se-0099", "se-0002", "se-0003", "se-0001"]
                .iter()
                .enumerate()
            {
                bank.assign(slot, Some(id(sound)), &view, None, &loader);
            }
            let chosen = SoundChoices::of(&panel, &bank);
            settings.set_sound_choices(chosen.clone());
            assert_eq!(settings.flush(&control), Some(Ok(())), "{song}");

            // The next run reads the same choices, and its window shows them.
            let reloaded = Config::load(&path).unwrap();
            assert_eq!(SoundChoices::from_config(&reloaded), chosen, "{song}");
            let panel = BgmPanel::new(chosen.bgm_asset, chosen.bgm_loop);
            let bank = PadBank::new(&chosen.pads, chosen.pad_policy);
            assert_eq!(panel.selected(), Some(&id(song)));
            assert_eq!(bank.saved(), chosen.pads);

            // That run changes something else only: the IDs stay in the file.
            let (control, mut settings) = start(reloaded, &path, t0);
            control.mic.set_gain_db(5.0);
            assert_eq!(settings.flush(&control), Some(Ok(())), "{song}");
            let again = SoundChoices::from_config(&Config::load(&path).unwrap());
            assert_eq!(again, chosen, "{song}");
        }
    }

    #[test]
    fn the_saved_text_has_ids_and_numbers_and_no_path() {
        let (root, view) = library("no-path");
        let loader = SeLoader::start(Box::new(|| {})).unwrap();
        let dir = test_dir("no-path-config");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(-6.0), &path, t0);
        control.bgm.set_gain_db(-3.0);
        control.se.set_muted(true);

        let mut panel = BgmPanel::new(None, true);
        panel.select(Some(id("bgm-0001")), &view, None);
        let mut bank = PadBank::new(&[], SePolicy::Restart);
        bank.assign(0, Some(id("se-0001")), &view, None, &loader);
        bank.assign(4, Some(id("se-0099")), &view, None, &loader);
        settings.set_sound_choices(SoundChoices::of(&panel, &bank));
        assert_eq!(settings.flush(&control), Some(Ok(())));

        let text = fs::read_to_string(&path).unwrap();
        for forbidden in [
            "/",
            "\\",
            "sounds",
            "here.wav",
            "unread.wav",
            ".wav",
            "library.toml",
            "ink-mixer-settings-test",
            &*root.to_string_lossy(),
            &*dir.to_string_lossy(),
        ] {
            assert!(!text.contains(forbidden), "{forbidden:?} is in\n{text}");
        }
        for wanted in [
            "bgm-0001",
            "se-0001",
            "se-0099",
            "policy = \"restart\"",
            "loop = true",
        ] {
            assert!(text.contains(wanted), "{wanted:?} is not in\n{text}");
        }
    }

    #[test]
    fn a_new_choice_is_saved_once_it_has_settled_and_replaces_the_old_one() {
        let dir = test_dir("choose-again");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let (control, mut settings) = start(config_with_master(-6.0), &path, t0);
        assert_eq!(settings.sound_choices(), &SoundChoices::default());

        // The same choices every frame are not a change.
        for k in 0..10u64 {
            settings.set_sound_choices(SoundChoices::default());
            assert!(settings.tick(&control, at(t0, 100 * k), false).is_none());
        }
        assert!(!path.exists());

        let first = SoundChoices {
            bgm_asset: Some(id("bgm-0001")),
            bgm_loop: false,
            pad_policy: SePolicy::Overlap,
            pads: vec![pad(0, "se-0001", 0.0), pad(1, "se-0002", -3.0)],
        };
        settings.set_sound_choices(first.clone());
        assert!(settings.tick(&control, at(t0, 1000), false).is_none());
        assert!(settings.wait(at(t0, 1000)).is_some());
        assert_eq!(settings.tick(&control, at(t0, 1300), false), Some(Ok(())));
        assert_eq!(
            SoundChoices::from_config(&Config::load(&path).unwrap()),
            first
        );

        // The user chooses another song, loops, drops a pad, and changes the rule.
        let second = SoundChoices {
            bgm_asset: Some(id("bgm-0002")),
            bgm_loop: true,
            pad_policy: SePolicy::Ignore,
            pads: vec![pad(1, "se-0003", -3.0)],
        };
        settings.set_sound_choices(second.clone());
        assert!(settings.tick(&control, at(t0, 2000), false).is_none());
        assert_eq!(settings.tick(&control, at(t0, 2300), false), Some(Ok(())));
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            SoundChoices::from_config(&Config::load(&path).unwrap()),
            second
        );
        assert!(
            !text.contains("bgm-0001") && !text.contains("se-0001"),
            "{text}"
        );

        // Choosing no song and no pad at all.
        settings.set_sound_choices(SoundChoices::default());
        assert_eq!(settings.flush(&control), Some(Ok(())));
        let cleared = SoundChoices::from_config(&Config::load(&path).unwrap());
        assert_eq!(cleared, SoundChoices::default());
    }

    #[test]
    fn enabled_settings_have_no_disabled_reason() {
        let t0 = Instant::now();
        let control = MixerControl::new();
        let settings = Settings::new(Config::default(), PathBuf::from("x.toml"), &control, t0);
        assert_eq!(settings.disabled_reason(), None);
    }
}
