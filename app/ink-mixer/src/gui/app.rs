//! The main window: the device pickers, and the MIC and MASTER strips
//! (#40, #41).
//!
//! The window owns the audio session that plays MIC -> Gain -> MASTER
//! (`session.rs`) and shares a `MixerControl` with it. The strips read and
//! set the controls; the window never touches the audio engine itself.
//!
//! The window also owns the backend, so it can start and stop the session
//! and change the devices: it drops the old session (the input stops first),
//! gets the device lists again, and opens a new one.
//!
//! Every frame the window gives the settings (`settings.rs`) the chance to
//! save a change that has settled, and asks to be repainted in time for it.
//! The settings are also saved when the window closes (#42).

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use ink_backend::{AudioBackend, DeviceId};
use ink_core::Config;
use ink_core::graph::MixerControl;
use tracing::{error, info, warn};

use super::device_panel::{DevicePanel, PanelAction};
use super::frame_limiter::{FrameLimiter, FrameRate};
use super::strip::Strip;
use crate::session::{AudioSession, SessionError};
use crate::settings::Settings;

/// While audio runs but the meters are still, the window checks this often
/// for new sound and for a stream that stopped.
const QUIET_CHECK: Duration = Duration::from_millis(500);

/// The main window.
pub struct InkMixerApp {
    backend: Box<dyn AudioBackend>,
    control: Arc<MixerControl>,
    /// The running audio, or `None` when it is stopped.
    session: Option<AudioSession>,
    devices: DevicePanel,
    settings: Settings,
    /// The line at the bottom: the devices in use, stopped, or an error.
    status: Status,
    /// A note under the status line (a device was replaced by the default,
    /// or the device lists could not be read).
    notice: Option<String>,
    /// Why the last save of the settings failed; cleared by the next
    /// successful save.
    save_error: Option<String>,
    mic: Strip,
    master: Strip,
    limiter: FrameLimiter,
    fps: FpsCounter,
}

/// What the bottom line shows.
#[derive(Debug, PartialEq, Eq)]
enum Status {
    Running(String),
    Stopped,
    /// A one-line explanation in Japanese and, if there is one, the English
    /// detail from the audio code (useful in a bug report).
    Error {
        summary: String,
        detail: Option<String>,
    },
}

impl Status {
    fn error(summary: impl Into<String>, detail: Option<String>) -> Self {
        Self::Error {
            summary: summary.into(),
            detail,
        }
    }

    fn running(session: &AudioSession) -> Self {
        Self::Running(format!(
            "入力: {}  /  出力: {}  /  {} Hz",
            session.input_device().name,
            session.output_device().name,
            session.sample_rate()
        ))
    }

    /// The status for a session that had to stop.
    fn stopped_by(e: &SessionError) -> Self {
        Self::error(
            "音声が止まりました: ".to_string() + &e.message_ja(),
            Some(e.to_string()),
        )
    }
}

impl InkMixerApp {
    /// Creates the window for `control`, with the audio session that was
    /// started for it (or the error that kept it from starting).
    ///
    /// The device lists are read here, and the pickers start with the devices
    /// in use, or else the configured ones, or else the defaults.
    /// `settings` were created after the saved values were restored.
    pub fn new(
        backend: Box<dyn AudioBackend>,
        config: &Config,
        control: Arc<MixerControl>,
        session: Result<AudioSession, String>,
        settings: Settings,
    ) -> Self {
        let now = Instant::now();
        let mut notices = Vec::new();

        let mut devices = DevicePanel::new();
        if let Err(e) = devices.refresh(backend.as_ref()) {
            warn!("{e}");
            notices.push(format!("デバイスの一覧を取得できませんでした: {e}"));
        }
        let default_input = backend.default_input().ok().flatten().map(|d| d.id);
        let default_output = backend.default_output().ok().flatten().map(|d| d.id);
        let started_input = session.as_ref().ok().map(|s| s.input_device().id.clone());
        let started_output = session.as_ref().ok().map(|s| s.output_device().id.clone());
        devices.select_first_listed(
            &[
                started_input.as_ref(),
                config.audio.input_device.as_ref(),
                default_input.as_ref(),
            ],
            &[
                started_output.as_ref(),
                config.audio.output_device.as_ref(),
                default_output.as_ref(),
            ],
        );
        if let (Some(input), Some(output)) = (&started_input, &started_output) {
            notices.extend(fallback_notices(config, input, output));
        }

        let (session, status) = match session {
            Ok(session) => {
                let status = Status::running(&session);
                (Some(session), status)
            }
            Err(e) => (None, Status::error("音声を開始できませんでした", Some(e))),
        };
        Self {
            backend,
            control,
            session,
            devices,
            settings,
            status,
            notice: (!notices.is_empty()).then(|| notices.join("  ")),
            save_error: None,
            mic: Strip::new("マイク", true, now),
            master: Strip::new("マスター", false, now),
            limiter: FrameLimiter::new(FrameRate::default(), now),
            fps: FpsCounter::new(now),
        }
    }

    /// Checks the streams; on a failure, stops the audio and shows why.
    fn check_session(&mut self, now: Instant) {
        if let Some(session) = &mut self.session
            && let Err(e) = session.check(now)
        {
            error!("audio stopped: {e}");
            self.session = None;
            self.status = Status::stopped_by(&e);
        }
    }

    /// Passes the devices the user chose to the settings. Only a choice of
    /// the user counts: not the initial selection, and not a default used in
    /// place of a missing device.
    fn sync_chosen_devices(&mut self) {
        if self.devices.take_user_changed() {
            self.settings.set_chosen_devices(
                self.devices.selected_input().cloned(),
                self.devices.selected_output().cloned(),
            );
        }
    }

    /// Lets the settings save a change that has settled. `pointer_down` is
    /// true while a mouse button is held (a slider may be dragged).
    fn save_settings(&mut self, now: Instant, pointer_down: bool) {
        if let Some(result) = self.settings.tick(&self.control, now, pointer_down) {
            self.note_save_result(result);
        }
    }

    /// Saves any change at once, because the window is closing.
    fn save_on_exit(&mut self) {
        if let Some(result) = self.settings.flush(&self.control) {
            self.note_save_result(result);
        }
    }

    /// Shows a failed save as a note, and removes the note after a success.
    fn note_save_result(&mut self, result: Result<(), String>) {
        self.save_error = match result {
            Ok(()) => None,
            Err(e) => {
                warn!("failed to save the settings: {e}");
                Some(format!("設定を保存できませんでした: {e}"))
            }
        };
    }

    /// Does what the user asked for in the device panel.
    fn apply(&mut self, action: PanelAction) {
        match action {
            PanelAction::Start | PanelAction::Restart => self.start_selected(),
            PanelAction::Stop => self.stop(),
            PanelAction::Refresh => self.refresh_lists(),
        }
    }

    /// Stops the audio. Dropping the session stops the input first.
    fn stop(&mut self) {
        self.session = None;
        self.status = Status::Stopped;
    }

    /// Gets the device lists again and shows a failure as a note.
    fn refresh_lists(&mut self) {
        self.notice = match self.devices.refresh(self.backend.as_ref()) {
            Ok(()) => None,
            Err(e) => {
                warn!("{e}");
                Some(format!("デバイスの一覧を取得できませんでした: {e}"))
            }
        };
    }

    /// Stops the audio, gets the lists again (so a device that was missing
    /// for a moment is found), and starts with the selected devices. On a
    /// failure the audio stays stopped and the status says why.
    fn start_selected(&mut self) {
        self.session = None;
        self.notice = None;

        if let Err(e) = self.devices.refresh(self.backend.as_ref()) {
            warn!("{e}");
            self.status = Status::error("デバイスの一覧を取得できませんでした", Some(e));
            return;
        }
        let (input, output) = match self.devices.selected_devices() {
            Ok(devices) => devices,
            Err(e) => {
                warn!("cannot start: {e:?}");
                self.status = Status::error(e.message_ja(), None);
                return;
            }
        };
        match AudioSession::start(self.backend.as_ref(), input, output, &self.control) {
            Ok(session) => {
                info!(
                    "audio started: {} -> {}, {} Hz",
                    session.input_device().id,
                    session.output_device().id,
                    session.sample_rate()
                );
                self.status = Status::running(&session);
                self.session = Some(session);
            }
            Err(e) => {
                error!("audio could not start: {e}");
                self.status = Status::error("音声を開始できませんでした", Some(e));
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Ink Mixer");
            ui.separator();
            ui.label("描画の上限:");
            let mut rate = self.limiter.rate();
            for choice in FrameRate::ALL {
                ui.selectable_value(&mut rate, choice, choice.label());
            }
            self.limiter.set_rate(rate, Instant::now());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.monospace(format!("{} fps", self.fps.fps()));
            });
        });
    }

    fn status_line(&self, ui: &mut egui::Ui) {
        match &self.status {
            Status::Running(text) => {
                ui.label(text);
            }
            Status::Stopped => {
                ui.label("停止中");
            }
            Status::Error { summary, detail } => {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 90), summary);
                if let Some(detail) = detail {
                    ui.label(egui::RichText::new(detail).small().weak());
                }
            }
        }
        let warning = egui::Color32::from_rgb(230, 190, 60);
        if let Some(notice) = &self.notice {
            ui.colored_label(warning, notice);
        }
        if let Some(reason) = self.settings.disabled_reason() {
            ui.colored_label(warning, reason);
        }
        if let Some(error) = &self.save_error {
            ui.colored_label(warning, error);
        }
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        ui.colored_label(
            egui::Color32::from_rgb(230, 190, 60),
            "注意：マイクの音を出力しています。ヘッドホンかヘッドセットで使ってください",
        );
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            self.mic.show(ui, &self.control.mic);
            self.master.show(ui, &self.control.master);
        });
    }
}

impl eframe::App for InkMixerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        self.fps.count(frame_start);
        self.check_session(frame_start);
        self.mic.update_meter(&self.control.mic, frame_start);
        self.master.update_meter(&self.control.master, frame_start);

        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        let running = self.session.is_some();
        let mut action = None;
        egui::Panel::top("devices").show(ui, |ui| action = self.devices.show(ui, running));
        if let Some(action) = action {
            self.apply(action);
        }
        self.sync_chosen_devices();
        egui::Panel::bottom("status").show(ui, |ui| self.status_line(ui));
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));

        // After the panels, so that this frame's moves of the faders and the
        // mute are seen.
        let pointer_down = ui.input(|i| i.pointer.any_down());
        self.save_settings(frame_start, pointer_down);

        let meters_active = self.mic.active() || self.master.active();
        let predicted = Duration::from_secs_f32(ui.input(|i| i.predicted_dt));
        let now = Instant::now();
        let delay = next_repaint(
            repaint_delay(
                meters_active,
                self.session.is_some(),
                &mut self.limiter,
                frame_start,
                now,
                predicted,
            ),
            self.settings.wait(now),
            predicted,
        );
        if let Some(delay) = delay {
            ui.ctx().request_repaint_after(delay);
        }
    }

    /// Called when the window closes.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_on_exit();
    }
}

/// Combines the repaint the meters and the stream check need with the wait
/// for a pending save: the earlier one wins. The save wait gets the
/// predicted frame time added back, as in [`repaint_delay`], because egui
/// subtracts it.
fn next_repaint(
    repaint: Option<Duration>,
    save_wait: Option<Duration>,
    predicted_dt: Duration,
) -> Option<Duration> {
    let save_wait = save_wait.map(|wait| wait + predicted_dt);
    match (repaint, save_wait) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Notes for the devices that were used instead of the configured ones.
/// `choose_devices` falls back to the default when a configured device is
/// not listed, which would otherwise go unnoticed.
fn fallback_notices(config: &Config, input: &DeviceId, output: &DeviceId) -> Vec<String> {
    let mut notices = Vec::new();
    for (label, configured, used) in [
        ("入力", &config.audio.input_device, input),
        ("出力", &config.audio.output_device, output),
    ] {
        if let Some(configured) = configured
            && configured != used
        {
            notices.push(format!(
                "設定の{label}デバイス（{configured}）が見つからないため、既定を使っています"
            ));
        }
    }
    notices
}

/// Decides when to repaint by itself:
///
/// - the meters move: at the frame rate cap;
/// - audio runs but the meters are still: every [`QUIET_CHECK`], to notice
///   new sound and a stream that stopped;
/// - no audio: never (egui repaints on input only), so an idle window uses
///   almost no CPU.
fn repaint_delay(
    meters_active: bool,
    audio_running: bool,
    limiter: &mut FrameLimiter,
    frame_start: Instant,
    now: Instant,
    predicted_dt: Duration,
) -> Option<Duration> {
    if meters_active {
        Some(limiter.delay_after(frame_start, now, predicted_dt))
    } else if audio_running {
        Some(QUIET_CHECK + predicted_dt)
    } else {
        None
    }
}

/// Counts frames to show the frame rate actually drawn.
#[derive(Debug)]
struct FpsCounter {
    frames: u32,
    window_start: Instant,
    fps: u32,
}

impl FpsCounter {
    fn new(now: Instant) -> Self {
        Self {
            frames: 0,
            window_start: now,
            fps: 0,
        }
    }

    /// Counts a frame drawn at `now`; updates the rate once a second.
    fn count(&mut self, now: Instant) {
        self.frames += 1;
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed >= Duration::from_secs(1) {
            self.fps = (f64::from(self.frames) / elapsed.as_secs_f64()).round() as u32;
            self.frames = 0;
            self.window_start = now;
        }
    }

    fn fps(&self) -> u32 {
        self.fps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_meters_repaint_at_the_frame_rate_cap() {
        let t0 = Instant::now();
        let mut limiter = FrameLimiter::new(FrameRate::Fps30, t0);
        assert_eq!(
            repaint_delay(true, true, &mut limiter, t0, t0, Duration::ZERO),
            Some(FrameRate::Fps30.interval())
        );
    }

    #[test]
    fn still_meters_with_audio_check_every_half_second() {
        let t0 = Instant::now();
        let mut limiter = FrameLimiter::new(FrameRate::Fps60, t0);
        assert_eq!(
            repaint_delay(false, true, &mut limiter, t0, t0, Duration::ZERO),
            Some(QUIET_CHECK)
        );
        assert_eq!(QUIET_CHECK, Duration::from_millis(500));
        // egui subtracts the predicted frame time, so it is added back.
        let predicted = Duration::from_millis(16);
        assert_eq!(
            repaint_delay(false, true, &mut limiter, t0, t0, predicted),
            Some(QUIET_CHECK + predicted)
        );
    }

    #[test]
    fn nothing_is_scheduled_without_audio() {
        let t0 = Instant::now();
        let mut limiter = FrameLimiter::new(FrameRate::Fps60, t0);
        assert_eq!(
            repaint_delay(false, false, &mut limiter, t0, t0, Duration::ZERO),
            None
        );
    }

    #[test]
    fn fps_counter_reports_frames_per_second() {
        let t0 = Instant::now();
        let mut counter = FpsCounter::new(t0);
        for k in 1..=60u32 {
            counter.count(t0 + FrameRate::Fps60.interval() * k);
        }
        assert_eq!(counter.fps(), 60);
    }

    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use ink_backend::{
        BackendError, DeviceInfo, InputCallback, InputStream, OutputCallback, OutputStream,
        StreamConfig,
    };
    use ink_core::AudioConfig;

    use crate::gui::device_panel::SelectionError;

    /// A path for the settings of a test, in a fresh directory.
    fn settings_path(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ink-mixer-app-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("config.toml")
    }

    /// A window whose audio is not running, with settings saved to `path`
    /// (or never saved when `path` is `None`).
    fn app_with_settings(
        backend: &FakeBackend,
        config: &Config,
        path: Option<std::path::PathBuf>,
    ) -> InkMixerApp {
        let control = Arc::new(MixerControl::new());
        crate::settings::restore(&control, &config.mixer);
        let now = Instant::now();
        let settings = match path {
            Some(path) => Settings::new(config.clone(), path, &control, now),
            None => Settings::disabled(config.clone(), "not saved in this test", &control, now),
        };
        InkMixerApp::new(
            Box::new(backend.clone()),
            config,
            control,
            Err("stopped for the test".to_string()),
            settings,
        )
    }

    /// A backend with lists the test can change. Nothing can be opened, so
    /// a start fails after the lists and the selection were checked.
    #[derive(Clone, Default)]
    struct FakeBackend {
        devices: Rc<RefCell<Vec<DeviceInfo>>>,
        list_fails: Rc<Cell<bool>>,
    }

    impl AudioBackend for FakeBackend {
        fn enumerate_input_devices(&self) -> ink_backend::Result<Vec<DeviceInfo>> {
            if self.list_fails.get() {
                return Err(BackendError::HostUnavailable("fake".to_string()));
            }
            Ok(self.devices.borrow().clone())
        }

        fn enumerate_output_devices(&self) -> ink_backend::Result<Vec<DeviceInfo>> {
            self.enumerate_input_devices()
        }

        fn default_input(&self) -> ink_backend::Result<Option<DeviceInfo>> {
            Ok(self.devices.borrow().first().cloned())
        }

        fn default_output(&self) -> ink_backend::Result<Option<DeviceInfo>> {
            self.default_input()
        }

        fn open_input(
            &self,
            _device: &DeviceId,
            _config: StreamConfig,
            _callback: InputCallback,
        ) -> ink_backend::Result<InputStream> {
            Err(BackendError::Other("cannot open (fake)".to_string()))
        }

        fn open_output(
            &self,
            _device: &DeviceId,
            _config: StreamConfig,
            _callback: OutputCallback,
        ) -> ink_backend::Result<OutputStream> {
            Err(BackendError::Other("cannot open (fake)".to_string()))
        }
    }

    fn device(id: &str) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(id),
            name: format!("Device {id}"),
            channels: 2,
            sample_rates: vec![48_000],
        }
    }

    fn config_with(input: Option<&str>, output: Option<&str>) -> Config {
        Config {
            audio: AudioConfig {
                input_device: input.map(DeviceId::new),
                output_device: output.map(DeviceId::new),
            },
            ..Config::default()
        }
    }

    /// A window with the default device first in the fake's list, and the
    /// audio not running.
    fn stopped_app(backend: &FakeBackend, config: &Config) -> InkMixerApp {
        backend
            .devices
            .replace(vec![device("fake:default"), device("fake:headset")]);
        app_with_settings(backend, config, None)
    }

    #[test]
    fn a_session_that_failed_to_start_shows_the_error() {
        let backend = FakeBackend::default();
        let control = Arc::new(MixerControl::new());
        let settings = Settings::disabled(Config::default(), "x", &control, Instant::now());
        let app = InkMixerApp::new(
            Box::new(backend),
            &Config::default(),
            control,
            Err("no default input device".to_string()),
            settings,
        );
        assert!(app.session.is_none());
        assert_eq!(
            app.status,
            Status::error(
                "音声を開始できませんでした",
                Some("no default input device".to_string())
            )
        );
    }

    #[test]
    fn the_pickers_start_with_the_configured_devices_or_the_defaults() {
        let backend = FakeBackend::default();
        let app = stopped_app(&backend, &config_with(Some("fake:headset"), None));
        assert_eq!(
            app.devices.selected_input(),
            Some(&DeviceId::new("fake:headset"))
        );
        assert_eq!(
            app.devices.selected_output(),
            Some(&DeviceId::new("fake:default")),
            "no configured output: the default"
        );

        let app = stopped_app(&backend, &config_with(Some("fake:gone"), None));
        assert_eq!(
            app.devices.selected_input(),
            Some(&DeviceId::new("fake:default")),
            "a configured device that is not listed: the default"
        );
    }

    #[test]
    fn a_missing_device_list_is_shown_as_a_note() {
        let backend = FakeBackend::default();
        backend.list_fails.set(true);
        let app = app_with_settings(&backend, &Config::default(), None);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .starts_with("デバイスの一覧を取得できませんでした")
        );
        assert!(!app.devices.can_start());
    }

    #[test]
    fn start_does_not_replace_a_device_that_went_missing() {
        let backend = FakeBackend::default();
        let mut app = stopped_app(&backend, &config_with(Some("fake:headset"), None));

        // The headset disappears from the list before "start" is pressed.
        backend.devices.replace(vec![device("fake:default")]);
        app.apply(PanelAction::Start);

        assert!(app.session.is_none());
        let missing = SelectionError::InputMissing(DeviceId::new("fake:headset"));
        assert_eq!(app.status, Status::error(missing.message_ja(), None));
        assert_eq!(
            app.devices.selected_input(),
            Some(&DeviceId::new("fake:headset")),
            "the selection stays"
        );

        // It is back at the next press (a moment's gap in the list).
        backend
            .devices
            .replace(vec![device("fake:default"), device("fake:headset")]);
        app.apply(PanelAction::Start);
        assert!(
            matches!(&app.status, Status::Error { summary, .. } if summary == "音声を開始できませんでした"),
            "the device was found, and only opening failed: {:?}",
            app.status
        );
    }

    #[test]
    fn start_shows_when_the_lists_cannot_be_read() {
        let backend = FakeBackend::default();
        let mut app = stopped_app(&backend, &Config::default());
        backend.list_fails.set(true);
        app.apply(PanelAction::Start);
        match &app.status {
            Status::Error { summary, detail } => {
                assert_eq!(summary, "デバイスの一覧を取得できませんでした");
                assert!(detail.as_deref().unwrap().contains("failed to list input"));
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn start_shows_why_opening_failed() {
        let backend = FakeBackend::default();
        let mut app = stopped_app(&backend, &Config::default());
        app.apply(PanelAction::Restart);
        match &app.status {
            Status::Error { summary, detail } => {
                assert_eq!(summary, "音声を開始できませんでした");
                assert!(detail.as_deref().unwrap().contains("cannot open (fake)"));
            }
            other => panic!("expected an error, got {other:?}"),
        }
        assert!(app.session.is_none());
    }

    #[test]
    fn stop_shows_stopped_and_refresh_keeps_the_selection() {
        let backend = FakeBackend::default();
        let mut app = stopped_app(&backend, &config_with(Some("fake:headset"), None));
        app.apply(PanelAction::Stop);
        assert_eq!(app.status, Status::Stopped);

        backend.devices.replace(vec![device("fake:default")]);
        app.apply(PanelAction::Refresh);
        assert_eq!(app.notice, None);
        assert_eq!(
            app.devices.selected_input(),
            Some(&DeviceId::new("fake:headset"))
        );

        backend.list_fails.set(true);
        app.apply(PanelAction::Refresh);
        assert!(app.notice.is_some());
    }

    #[test]
    fn a_session_that_stopped_shows_japanese_and_the_english_detail() {
        let e = SessionError::Stream {
            side: crate::session::Side::Input,
            error: ink_backend::StreamError::DeviceUnavailable,
        };
        assert_eq!(
            Status::stopped_by(&e),
            Status::error(
                "音声が止まりました: 入力のデバイスが使えなくなりました（USB を抜きましたか？）",
                Some("input stream stopped: the audio device is no longer available".to_string())
            )
        );
    }

    #[test]
    fn a_replaced_configured_device_is_noted() {
        let used = DeviceId::new("fake:default");
        let config = config_with(Some("fake:headset"), Some("fake:default"));
        assert_eq!(
            fallback_notices(&config, &used, &used),
            vec![
                "設定の入力デバイス（fake:headset）が見つからないため、既定を使っています"
                    .to_string()
            ]
        );
        assert!(fallback_notices(&Config::default(), &used, &used).is_empty());
        assert!(
            fallback_notices(&config_with(Some("fake:default"), None), &used, &used).is_empty()
        );
    }

    #[test]
    fn the_earlier_of_the_repaint_and_the_save_wait_wins() {
        let ms = Duration::from_millis;
        let predicted = ms(16);
        // Only one of them.
        assert_eq!(next_repaint(None, None, predicted), None);
        assert_eq!(next_repaint(Some(ms(500)), None, predicted), Some(ms(500)));
        assert_eq!(next_repaint(None, Some(ms(300)), predicted), Some(ms(316)));
        // Both: the earlier one. The wait gets the predicted time added back.
        assert_eq!(
            next_repaint(Some(ms(500)), Some(ms(300)), predicted),
            Some(ms(316))
        );
        assert_eq!(
            next_repaint(Some(ms(16)), Some(ms(300)), predicted),
            Some(ms(16))
        );
    }

    #[test]
    fn a_settled_change_is_saved_and_a_failure_is_shown_then_cleared() {
        let backend = FakeBackend::default();
        let path = settings_path("save-error");
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        // The "directory" of the config file is a regular file: saving fails.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let mut app = app_with_settings(&backend, &Config::default(), Some(blocker.join("c.toml")));

        let t0 = Instant::now();
        app.control.mic.set_gain_db(4.0);
        app.save_settings(t0, false);
        assert_eq!(app.save_error, None, "not settled yet");
        app.save_settings(t0 + Duration::from_millis(400), false);
        let error = app.save_error.clone().unwrap();
        assert!(error.starts_with("設定を保存できませんでした: "), "{error}");

        // The place becomes usable; the next change is saved, and the note goes.
        std::fs::remove_file(&blocker).unwrap();
        app.control.mic.set_gain_db(5.0);
        app.save_settings(t0 + Duration::from_millis(500), false);
        app.save_settings(t0 + Duration::from_millis(900), false);
        assert_eq!(app.save_error, None);
        assert_eq!(
            Config::load(&blocker.join("c.toml"))
                .unwrap()
                .mixer
                .mic_gain_db(),
            Some(5.0)
        );
    }

    #[test]
    fn nothing_is_saved_while_the_mouse_button_is_down() {
        let backend = FakeBackend::default();
        let path = settings_path("pointer");
        let mut app = app_with_settings(&backend, &Config::default(), Some(path.clone()));
        let t0 = Instant::now();
        app.control.mic.set_gain_db(6.0);
        app.save_settings(t0, true);
        app.save_settings(t0 + Duration::from_secs(5), true);
        assert!(!path.exists(), "the slider is still held");
        app.save_settings(t0 + Duration::from_secs(5), false);
        assert!(path.exists());
    }

    #[test]
    fn settings_that_cannot_be_saved_say_so_and_never_write() {
        let backend = FakeBackend::default();
        let mut app = app_with_settings(&backend, &Config::default(), None);
        assert_eq!(
            app.settings.disabled_reason(),
            Some("not saved in this test")
        );
        app.control.mic.set_gain_db(6.0);
        let t0 = Instant::now();
        app.save_settings(t0, false);
        app.save_settings(t0 + Duration::from_secs(5), false);
        app.save_on_exit();
        assert_eq!(app.save_error, None);
    }

    #[test]
    fn a_change_is_saved_when_the_window_closes() {
        let backend = FakeBackend::default();
        let path = settings_path("exit");
        let mut app = app_with_settings(&backend, &Config::default(), Some(path.clone()));
        app.control.master.set_gain_db(-30.0);
        app.control.mic.set_muted(true);
        app.save_on_exit();
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.mixer.master_db(), Some(-30.0));
        assert_eq!(saved.mixer.mic_muted(), Some(true));
    }

    #[test]
    fn only_a_device_the_user_chose_is_passed_to_the_settings() {
        let backend = FakeBackend::default();
        backend
            .devices
            .replace(vec![device("fake:default"), device("fake:headset")]);
        // The saved headset is not listed: the window starts with the default.
        let config = config_with(Some("fake:gone"), None);
        let path = settings_path("chosen");
        let mut app = app_with_settings(&backend, &config, Some(path.clone()));
        assert_eq!(
            app.devices.selected_input(),
            Some(&DeviceId::new("fake:default"))
        );

        // The default that replaced it is not the user's choice.
        app.sync_chosen_devices();
        app.control.mic.set_gain_db(2.0);
        app.save_on_exit();
        assert_eq!(
            Config::load(&path).unwrap().audio.input_device,
            Some(DeviceId::new("fake:gone"))
        );

        // The user picks the headset: now it is saved.
        app.devices.user_selects(
            Some(DeviceId::new("fake:headset")),
            Some(DeviceId::new("fake:default")),
        );
        app.sync_chosen_devices();
        app.control.mic.set_gain_db(3.0);
        app.save_on_exit();
        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.audio.input_device,
            Some(DeviceId::new("fake:headset"))
        );
        assert_eq!(
            saved.audio.output_device,
            Some(DeviceId::new("fake:default"))
        );

        // Once passed on, the mark is gone: nothing more is passed.
        assert!(!app.devices.take_user_changed());
    }
}
