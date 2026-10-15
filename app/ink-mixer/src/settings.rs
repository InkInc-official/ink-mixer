//! Saving and restoring the settings of the window (#42): the chosen
//! devices, the MIC gain and mute, and the master volume.
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
//! - The devices are saved only when the user chose them. When a saved device
//!   is missing and the default is used instead, the file keeps the saved
//!   device.
//! - When the file could not be read, nothing is saved ([`Settings::disabled`]),
//!   so a broken (or newer) file is never overwritten.
//!
//! Everything here runs on the UI thread. The audio thread never saves
//! (AGENTS.md §5); the UI reads the values from the atomics of `MixerControl`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ink_core::graph::MixerControl;
use ink_core::{Config, DeviceId, MixerConfig};
use tracing::{info, warn};

/// The master volume used when nothing is saved, and the highest master
/// volume the window starts with.
pub const STARTUP_MASTER_DB: f32 = -12.0;
/// The MIC gain used when nothing is saved.
pub const DEFAULT_MIC_GAIN_DB: f32 = 0.0;
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

    let master = startup_master_db(mixer.master_db());
    if let Some(saved) = mixer.master_db()
        && saved > master
    {
        info!("saved master volume {saved:.1} dB is lowered to {master:.1} dB for the start");
    }
    control.master.set_gain_db(master);
}

/// What would be saved: compared with the saved state to find out whether
/// there is anything to save.
#[derive(Debug, Clone, PartialEq)]
pub struct MixerState {
    pub input_device: Option<DeviceId>,
    pub output_device: Option<DeviceId>,
    pub mic_gain_db: f32,
    pub mic_muted: bool,
    pub master_db: f32,
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
            startup_master_db: control.master.gain_db(),
            master_touched: false,
            tracker: SaveTracker::new(
                MixerState {
                    input_device: None,
                    output_device: None,
                    mic_gain_db: 0.0,
                    mic_muted: false,
                    master_db: 0.0,
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
            master_db,
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
        config.mixer = MixerConfig::new(state.mic_gain_db, state.mic_muted, state.master_db);

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
            master_db: -12.0,
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
        assert_eq!(saved.mixer, MixerConfig::new(7.0, true, -6.0));
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
        assert_eq!(saved.mixer, MixerConfig::new(-3.0, false, 3.0));
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

    #[test]
    fn enabled_settings_have_no_disabled_reason() {
        let t0 = Instant::now();
        let control = MixerControl::new();
        let settings = Settings::new(Config::default(), PathBuf::from("x.toml"), &control, t0);
        assert_eq!(settings.disabled_reason(), None);
    }
}
