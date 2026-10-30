//! The main window: the device pickers, the four strips (MIC, BGM, SE and
//! MASTER), the controls of the song, and the Sound Pad (#40, #41, #64).
//!
//! The window owns the audio session that plays MIC, BGM and SE through the
//! mixer (`session.rs`) and shares a `MixerControl` with it. The strips read
//! and set the controls; the window never touches the audio engine itself.
//!
//! The window holds no `Bgm` and no `SeController`: every frame it borrows
//! them from the session (`session.bgm()`, `session.se_mut()`), one after the
//! other, because the second needs `&mut` of the session. What it keeps of
//! the song and the pads is IDs and numbers ([`Sounds`]), so the session can
//! be dropped anywhere (a stop, a device change, an error) and the audio side
//! goes in the order of ADR-0009.
//!
//! The middle of the window is one column that scrolls up and down and uses
//! the whole width, in three parts (ARCHITECTURE: layout of #64): the four
//! strips in a row, the controls of the song, and the Sound Pad.
//!
//! The window also owns the backend, so it can start and stop the session
//! and change the devices: it drops the old session (the input stops first),
//! gets the device lists again, and opens a new one.
//!
//! Every frame the window gives the settings (`settings.rs`) the chance to
//! save a change that has settled, and asks to be repainted in time for it.
//! The settings are also saved when the window closes (#42).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use ink_backend::{AudioBackend, DeviceId};
use ink_core::Config;
use ink_core::graph::MixerControl;
use tracing::{error, info, warn};

use super::SESSION_OPTIONS;
use super::bgm_strip::{BgmStrip, position_refresh};
use super::device_panel::{DevicePanel, PanelAction};
use super::frame_limiter::{FrameLimiter, FrameRate};
use super::sound_pad::{SoundPad, reborrow};
use super::strip::Strip;
use crate::bgm_worker::Bgm;
use crate::library_view::LibraryView;
use crate::se_loader::SeLoader;
use crate::session::{AudioSession, SessionError};
use crate::settings::{Settings, SoundChoices};
use crate::sound_state::{BgmPanel, PAD_COUNT, PadBank, PadState, SoundSlots};

/// While audio runs but the meters are still, the window checks this often
/// for new sound and for a stream that stopped.
const QUIET_CHECK: Duration = Duration::from_millis(500);

/// The song and the Sound Pad of the window: the library, what the user
/// chose, and the parts that draw them. No audio object is in here (see the
/// top of the file).
pub struct Sounds {
    view: LibraryView,
    /// Where the library is, to open it again.
    root: Option<PathBuf>,
    panel: BgmPanel,
    bgm: BgmStrip,
    se: Strip,
    bank: PadBank,
    pad: SoundPad,
    /// Reads the sounds of the pads. It is joined when the window goes, after
    /// the session (the field of the session is declared before this one).
    loader: SeLoader,
    /// The audio stopped by itself while a song played: the next start tells
    /// the user that the song stopped. A stop by the user does not.
    stopped_while_playing: bool,
}

impl Sounds {
    /// Gathers the parts the window starts with. `root` is where the library
    /// was opened, for the "read again" button.
    pub fn new(
        view: LibraryView,
        root: Option<PathBuf>,
        panel: BgmPanel,
        bank: PadBank,
        loader: SeLoader,
        now: Instant,
    ) -> Self {
        Self {
            view,
            root,
            panel,
            bgm: BgmStrip::new(now),
            se: Strip::new("SE", true, now),
            bank,
            pad: SoundPad::new(),
            loader,
            stopped_while_playing: false,
        }
    }

    /// The failure of the song, if one came. Call every frame: it is only
    /// seen when it is asked for.
    fn poll_bgm(&mut self, bgm: Option<&Bgm>) {
        if let Some(bgm) = bgm {
            self.panel.poll(bgm);
        }
    }

    /// The sounds the loader has read, and the ones the player gave back.
    fn poll_se(&mut self, se: Option<&mut dyn SoundSlots>) {
        self.bank.poll(&self.loader, se);
    }

    /// A session with a BGM player started: the chosen song is opened.
    fn bgm_started(&mut self, bgm: &Bgm, was_playing: bool) {
        self.panel.on_session_started(&self.view, bgm, was_playing);
    }

    /// A session started: every chosen sound is read again, for its rate.
    fn se_started(&mut self, se: &mut dyn SoundSlots) {
        self.bank.on_session_started(&self.view, se, &self.loader);
    }

    /// The library is opened again, and a chosen song that could not be used
    /// is tried again.
    fn reload_view(&mut self, bgm: Option<&Bgm>) {
        self.view.reload(self.root.as_deref());
        self.panel.on_library_reloaded(&self.view, bgm);
    }

    /// After the library was opened again: the pads that failed are read once
    /// more with the same sound. A pad that works is left as it is.
    fn reload_pads(&mut self, mut se: Option<&mut dyn SoundSlots>) {
        for slot in 0..PAD_COUNT {
            if matches!(self.bank.state(slot), PadState::Failed(_)) {
                let id = self.bank.asset(slot).copied();
                self.bank
                    .assign(slot, id, &self.view, reborrow(&mut se), &self.loader);
            }
        }
    }

    /// What is saved of the song and the pads.
    pub(super) fn choices(&self) -> SoundChoices {
        SoundChoices::of(&self.panel, &self.bank)
    }

    /// Whether the meter of the BGM or the SE still moves.
    fn meters_active(&self) -> bool {
        self.bgm.active() || self.se.active()
    }

    /// The row with the "read again" button, and, when the library cannot be
    /// used, what is wrong and what to do. Returns whether the button was
    /// pressed.
    fn library_row(&self, ui: &mut egui::Ui) -> bool {
        let warning = egui::Color32::from_rgb(230, 190, 60);
        let error = egui::Color32::from_rgb(230, 90, 90);
        if let Some(problem) = self.view.unavailable() {
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.vertical(|ui| {
                    let line = |text: &str| {
                        egui::Label::new(egui::RichText::new(text).color(error)).wrap()
                    };
                    ui.add(line(&problem.summary_ja));
                    if let Some(howto) = &problem.howto_ja {
                        ui.add(egui::Label::new(howto.as_str()).wrap());
                    }
                    ui.add(
                        egui::Label::new(egui::RichText::new(&problem.detail).small().weak())
                            .wrap(),
                    );
                });
            });
        }
        let mut pressed = false;
        ui.horizontal(|ui| {
            pressed = ui
                .button("ライブラリを読み直す")
                .on_hover_text("音源フォルダの索引を開き直します（取り込みはしません）")
                .clicked();
            let skipped = self.view.warnings();
            if skipped > 0 {
                ui.colored_label(
                    warning,
                    format!("索引の {skipped} 件を読み込めませんでした"),
                );
            }
        });
        pressed
    }
}

/// The main window.
pub struct InkMixerApp {
    backend: Box<dyn AudioBackend>,
    control: Arc<MixerControl>,
    /// The running audio, or `None` when it is stopped. It is declared before
    /// `sounds`, so it is dropped first.
    session: Option<AudioSession>,
    /// The song and the Sound Pad; `None` until [`with_sounds`] (a window
    /// without them shows MIC and MASTER only).
    ///
    /// [`with_sounds`]: InkMixerApp::with_sounds
    sounds: Option<Sounds>,
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
            sounds: None,
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

    /// Gives the window the song and the Sound Pad, and starts them in the
    /// session that is running, if there is one.
    pub fn with_sounds(mut self, sounds: Sounds) -> Self {
        self.sounds = Some(sounds);
        self.start_sounds(false);
        self
    }

    /// Adds a line to the note under the status line.
    pub fn with_notice(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        self.notice = Some(match self.notice.take() {
            Some(old) => format!("{old}  {text}"),
            None => text,
        });
        self
    }

    /// Tells the song and the pads that a session started. `was_playing` is
    /// whether a song played when the old audio stopped.
    fn start_sounds(&mut self, was_playing: bool) {
        let (Some(sounds), Some(session)) = (&mut self.sounds, &mut self.session) else {
            return;
        };
        if let Some(bgm) = session.bgm() {
            sounds.bgm_started(bgm, was_playing);
        }
        sounds.se_started(session.se_mut());
    }

    /// Whether a song is playing, or was when the audio stopped by itself.
    /// Ask this before the old session is dropped. It also forgets the second
    /// answer: it is told once.
    fn take_was_playing(&mut self) -> bool {
        let Some(sounds) = &mut self.sounds else {
            return false;
        };
        let now = sounds
            .panel
            .was_playing(self.session.as_ref().and_then(|s| s.bgm()));
        std::mem::take(&mut sounds.stopped_while_playing) || now
    }

    /// Puts back what [`take_was_playing`](Self::take_was_playing) took, when
    /// the start did not work: the song did stop, and the next start that
    /// works says so.
    fn keep_was_playing(&mut self, was_playing: bool) {
        if let Some(sounds) = &mut self.sounds {
            sounds.stopped_while_playing = was_playing;
        }
    }

    /// The failure of the song and the sounds that were read. At the start of
    /// every frame: a failure is only seen when it is asked for.
    fn poll_sounds(&mut self) {
        let Some(sounds) = &mut self.sounds else {
            return;
        };
        sounds.poll_bgm(self.session.as_ref().and_then(|s| s.bgm()));
        sounds.poll_se(
            self.session
                .as_mut()
                .map(|s| s.se_mut() as &mut dyn SoundSlots),
        );
    }

    /// Reads the meters of the four channels.
    fn update_meters(&mut self, now: Instant) {
        self.mic.update_meter(&self.control.mic, now);
        self.master.update_meter(&self.control.master, now);
        if let Some(sounds) = &mut self.sounds {
            sounds.bgm.update_meter(&self.control.bgm, now);
            sounds.se.update_meter(&self.control.se, now);
        }
    }

    /// Whether the meter of any of the four channels still moves.
    fn meters_active(&self) -> bool {
        self.mic.active()
            || self.master.active()
            || self.sounds.as_ref().is_some_and(Sounds::meters_active)
    }

    /// Passes the song and the pads, as the window has them, to the settings.
    /// Only a change is saved.
    fn sync_sound_choices(&mut self) {
        if let Some(sounds) = &self.sounds {
            self.settings.set_sound_choices(sounds.choices());
        }
    }

    /// How long until the playing position needs a repaint of its own, if a
    /// song plays.
    fn position_refresh(&self) -> Option<Duration> {
        let sounds = self.sounds.as_ref()?;
        let bgm = self.session.as_ref().and_then(|s| s.bgm());
        position_refresh(sounds.panel.transport(bgm))
    }

    /// Checks the streams; on a failure, stops the audio and shows why.
    fn check_session(&mut self, now: Instant) {
        let failed = match &mut self.session {
            Some(session) => session.check(now).err(),
            None => None,
        };
        if let Some(e) = failed {
            error!("audio stopped: {e}");
            // The song was playing when the audio died: the next start says so.
            let was_playing = self.take_was_playing();
            if let Some(sounds) = &mut self.sounds {
                sounds.stopped_while_playing = was_playing;
            }
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
        // The user stopped it: the next start has nothing to tell about the song.
        if let Some(sounds) = &mut self.sounds {
            sounds.stopped_while_playing = false;
        }
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
        // Before the old session goes: afterwards nothing says what it did.
        let was_playing = self.take_was_playing();
        self.session = None;
        self.notice = None;

        if let Err(e) = self.devices.refresh(self.backend.as_ref()) {
            warn!("{e}");
            self.status = Status::error("デバイスの一覧を取得できませんでした", Some(e));
            self.keep_was_playing(was_playing);
            return;
        }
        let (input, output) = match self.devices.selected_devices() {
            Ok(devices) => devices,
            Err(e) => {
                warn!("cannot start: {e:?}");
                self.status = Status::error(e.message_ja(), None);
                self.keep_was_playing(was_playing);
                return;
            }
        };
        match AudioSession::start_with(
            self.backend.as_ref(),
            input,
            output,
            &self.control,
            SESSION_OPTIONS,
        ) {
            Ok(session) => {
                info!(
                    "audio started: {} -> {}, {} Hz",
                    session.input_device().id,
                    session.output_device().id,
                    session.sample_rate()
                );
                self.status = Status::running(&session);
                self.session = Some(session);
                self.start_sounds(was_playing);
            }
            Err(e) => {
                error!("audio could not start: {e}");
                self.status = Status::error("音声を開始できませんでした", Some(e));
                self.keep_was_playing(was_playing);
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

    /// The middle of the window: one column that scrolls up and down and uses
    /// the whole width (never sideways), with the four strips in a row of a
    /// fixed height, the controls of the song, and the Sound Pad.
    fn central(&mut self, ui: &mut egui::Ui) -> egui::scroll_area::ScrollAreaOutput<()> {
        egui::ScrollArea::vertical()
            .id_salt("central")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 190, 60),
                    "注意：マイクの音を出力しています。ヘッドホンかヘッドセットで使ってください",
                );
                ui.add_space(8.0);
                ui.horizontal_top(|ui| {
                    self.mic.show(ui, &self.control.mic);
                    if let Some(sounds) = &mut self.sounds {
                        sounds.bgm.show_strip(ui, &self.control.bgm);
                        sounds.se.show(ui, &self.control.se);
                    }
                    self.master.show(ui, &self.control.master);
                });

                let Some(sounds) = &mut self.sounds else {
                    return;
                };
                ui.add_space(8.0);
                if sounds.library_row(ui) {
                    sounds.reload_view(self.session.as_ref().and_then(|s| s.bgm()));
                    sounds.reload_pads(
                        self.session
                            .as_mut()
                            .map(|s| s.se_mut() as &mut dyn SoundSlots),
                    );
                }
                ui.add_space(4.0);
                let bgm = self.session.as_ref().and_then(|s| s.bgm());
                sounds
                    .bgm
                    .show_controls(ui, &mut sounds.panel, &sounds.view, bgm);
                ui.add_space(8.0);
                let se = self
                    .session
                    .as_mut()
                    .map(|s| s.se_mut() as &mut dyn SoundSlots);
                sounds
                    .pad
                    .show(ui, &mut sounds.bank, &sounds.view, se, &sounds.loader);
            })
    }
}

impl eframe::App for InkMixerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        self.fps.count(frame_start);
        self.check_session(frame_start);
        // First, so that a failure that came is shown in this frame.
        self.poll_sounds();
        self.update_meters(frame_start);

        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        let running = self.session.is_some();
        let mut action = None;
        egui::Panel::top("devices").show(ui, |ui| action = self.devices.show(ui, running));
        if let Some(action) = action {
            self.apply(action);
        }
        self.sync_chosen_devices();
        egui::Panel::bottom("status").show(ui, |ui| self.status_line(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            self.central(ui);
        });

        // After the panels, so that this frame's moves of the faders, the
        // mute and the choices are seen.
        self.sync_sound_choices();
        let pointer_down = ui.input(|i| i.pointer.any_down());
        self.save_settings(frame_start, pointer_down);

        let meters_active = self.meters_active();
        let predicted = Duration::from_secs_f32(ui.input(|i| i.predicted_dt));
        let now = Instant::now();
        let delay = with_position_refresh(
            next_repaint(
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
            ),
            self.position_refresh(),
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

/// Adds the repaint that a playing song needs for its position to the other
/// ones: the earlier wins. Like the save wait it gets the predicted frame time
/// added back, because egui subtracts it. While nothing plays there is
/// nothing to add.
fn with_position_refresh(
    delay: Option<Duration>,
    position: Option<Duration>,
    predicted_dt: Duration,
) -> Option<Duration> {
    match (delay, position.map(|refresh| refresh + predicted_dt)) {
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
            detail: None,
            note: None,
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

    // -- the song and the Sound Pad (#64) -------------------------------------

    use std::sync::mpsc;

    use ink_core::graph::{BgmPlayer, SeError, SePolicy, SoundData, bgm_channel};
    use ink_core::{AssetId, PadSetting};

    use crate::sound_state::Transport;

    const RATE: u32 = 48_000;
    /// How long a test waits for the loader.
    const WAIT: Duration = Duration::from_secs(10);

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    /// A directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ink-mixer-app-sounds-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A mono 16-bit WAV file at `RATE` with `samples` samples.
    fn write_wav(path: &std::path::Path, samples: usize) {
        let data_len = (samples * 2) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&RATE.to_le_bytes());
        bytes.extend_from_slice(&(RATE * 2).to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for _ in 0..samples {
            bytes.extend_from_slice(&8_192_i16.to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn index_entry(id: &str, kind: &str, file: &str) -> String {
        format!(
            "[[asset]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{file}\"\n\n\
             [asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 1\nframes = 480\n\n"
        )
    }

    /// The folders of a library with a song `bgm-0001` and the sound effect
    /// `se-0001` (Japanese names), and the text of an index that lists them.
    fn library_files(root: &std::path::Path) -> String {
        let bgm = root.join("sounds").join("bgm");
        let se = root.join("sounds").join("se");
        std::fs::create_dir_all(&bgm).unwrap();
        std::fs::create_dir_all(&se).unwrap();
        write_wav(&bgm.join("曲 テスト.wav"), 4_800);
        write_wav(&se.join("拍手 テスト.wav"), 480);
        format!(
            "version = 1\nnext_id = 100\n\n{}{}",
            index_entry("bgm-0001", "bgm", "曲 テスト.wav"),
            index_entry("se-0001", "se", "拍手 テスト.wav"),
        )
    }

    /// A library that lists the song and the sound effect.
    fn open_library(name: &str) -> (PathBuf, LibraryView) {
        let root = test_dir(name);
        let index = library_files(&root);
        std::fs::write(root.join("library.toml"), index).unwrap();
        let view = LibraryView::open(Some(&root));
        assert!(view.library().is_some());
        (root, view)
    }

    /// What the window asked of the SE player.
    #[derive(Default)]
    struct FakeSlots {
        cleared: Vec<usize>,
        sounds: Vec<usize>,
    }

    impl SoundSlots for FakeSlots {
        fn sample_rate(&self) -> u32 {
            RATE
        }
        fn set_sound(&mut self, slot: usize, _sound: Arc<SoundData>) -> Result<(), SeError> {
            self.sounds.push(slot);
            Ok(())
        }
        fn clear(&mut self, slot: usize) -> Result<(), SeError> {
            self.cleared.push(slot);
            Ok(())
        }
        fn play(&mut self, _slot: usize, _gain: f32, _policy: SePolicy) -> Result<(), SeError> {
            Ok(())
        }
        fn stop_all(&mut self) -> Result<(), SeError> {
            Ok(())
        }
        fn collect(&mut self) -> usize {
            0
        }
    }

    /// The BGM player (audio side) and the `Bgm` (control side), the audio
    /// side first as in `AudioSession` (ADR-0009): the fields of a struct
    /// drop in the order they are declared.
    struct AudioSide {
        _player: BgmPlayer,
        bgm: Bgm,
    }

    impl AudioSide {
        fn new() -> Self {
            let (controller, feeder, player) = bgm_channel(RATE, 128);
            let bgm = Bgm::start(controller, feeder).unwrap();
            Self {
                _player: player,
                bgm,
            }
        }
    }

    /// A loader that tells the test when a result is ready, so that the test
    /// waits for it and does not sleep.
    fn loader() -> (SeLoader, mpsc::Receiver<()>) {
        let (sender, receiver) = mpsc::channel();
        let loader = SeLoader::start(Box::new(move || {
            let _ = sender.send(());
        }))
        .unwrap();
        (loader, receiver)
    }

    /// Takes the results of the loader until no pad is being read.
    fn settle(sounds: &mut Sounds, wake: &mpsc::Receiver<()>, slots: &mut FakeSlots) {
        let start = Instant::now();
        while sounds.bank.loading() {
            wake.recv_timeout(WAIT.saturating_sub(start.elapsed()))
                .expect("the loader answered");
            sounds.poll_se(Some(slots));
        }
    }

    /// The song and the Sound Pad for a window: `choices` as the settings
    /// have them, in `view`.
    fn sounds_for(
        view: LibraryView,
        root: Option<PathBuf>,
        choices: &SoundChoices,
    ) -> (Sounds, mpsc::Receiver<()>) {
        let (loader, wake) = loader();
        let sounds = Sounds::new(
            view,
            root,
            BgmPanel::new(choices.bgm_asset, choices.bgm_loop),
            PadBank::new(&choices.pads, choices.pad_policy),
            loader,
            Instant::now(),
        );
        (sounds, wake)
    }

    fn choices_with(song: Option<&str>, pads: &[(usize, &str, f32)]) -> SoundChoices {
        SoundChoices {
            bgm_asset: song.map(id),
            bgm_loop: true,
            pad_policy: SePolicy::Restart,
            pads: pads
                .iter()
                .map(|&(slot, asset, gain_db)| PadSetting {
                    slot,
                    asset: id(asset),
                    gain_db,
                })
                .collect(),
        }
    }

    /// A stopped window with the song and the Sound Pad. The settings are
    /// saved to `path` (or never).
    fn app_with_sounds(
        name: &str,
        view: LibraryView,
        root: Option<PathBuf>,
        config: &Config,
        save: bool,
    ) -> (InkMixerApp, mpsc::Receiver<()>, PathBuf) {
        let backend = FakeBackend::default();
        backend
            .devices
            .replace(vec![device("fake:default"), device("fake:headset")]);
        let path = settings_path(name);
        let app = app_with_settings(&backend, config, save.then(|| path.clone()));
        let (sounds, wake) = sounds_for(view, root, &SoundChoices::from_config(config));
        (app.with_sounds(sounds), wake, path)
    }

    fn config_with_sounds(choices: &SoundChoices) -> Config {
        use ink_core::{BgmConfig, SoundPadConfig};
        Config {
            bgm: BgmConfig::new(choices.bgm_asset.as_ref(), choices.bgm_loop),
            sound_pad: SoundPadConfig::new(choices.pad_policy, &choices.pads),
            ..Config::default()
        }
    }

    // -- the layout of the middle -----------------------------------------------

    /// A context with the bundled font, as the window has it.
    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        super::super::fonts::install(&ctx);
        ctx
    }

    fn input(width: f32, height: f32) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, height),
            )),
            ..Default::default()
        }
    }

    /// Draws the middle of the window and returns what the scroll area
    /// reports.
    fn draw_central(
        app: &mut InkMixerApp,
        ctx: &egui::Context,
        width: f32,
        height: f32,
    ) -> egui::scroll_area::ScrollAreaOutput<()> {
        let mut output = None;
        ctx.run_ui(input(width, height), |ui| {
            output = Some(app.central(ui));
        })
        .drop_without_applying_deltas();
        output.unwrap()
    }

    #[test]
    fn a_strip_has_the_same_height_whatever_the_height_it_is_given_and_inside_a_scroll_area() {
        let ctx = context();
        let control = MixerControl::new();
        let mut strip = Strip::new("マイク", true, Instant::now());
        let mut heights = Vec::new();
        for screen in [200.0, 480.0, 900.0, 4_000.0] {
            let mut height = 0.0;
            ctx.run_ui(input(704.0, screen), |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            let before = ui.min_rect().top();
                            strip.show(ui, &control.mic);
                            height = ui.min_rect().bottom() - before;
                        });
                    });
            })
            .drop_without_applying_deltas();
            heights.push(height);
        }
        // About 320 points, fixed: the meter and the fader are 200 points high
        // and do not take what is left of the window.
        for height in &heights {
            assert!((318.0..=322.0).contains(height), "{heights:?}");
        }
        assert!(
            heights
                .windows(2)
                .all(|pair| (pair[0] - pair[1]).abs() < 0.5),
            "{heights:?}"
        );
    }

    #[test]
    fn at_720_points_the_middle_scrolls_up_and_down_and_never_sideways() {
        let ctx = context();
        let (root, view) = open_library("scroll");
        let choices = choices_with(
            Some("bgm-0001"),
            &[(0, "se-0001", 0.0), (1, "se-0099", 0.0)],
        );
        let (mut app, _wake, _path) = app_with_sounds(
            "scroll",
            view,
            Some(root),
            &config_with_sounds(&choices),
            false,
        );
        // The width of the middle of a 720 points wide window (a margin of 8
        // on each side), and a window that is not high enough for everything.
        let low = draw_central(&mut app, &ctx, 704.0, 300.0);
        assert!(
            low.content_size.x <= low.inner_rect.width() + 0.5,
            "sideways: {:?} in {:?}",
            low.content_size,
            low.inner_rect
        );
        assert!(
            (low.inner_rect.width() - 704.0).abs() < 1.0,
            "the whole width is used: {:?}",
            low.inner_rect
        );
        assert!(
            low.content_size.y > low.inner_rect.height(),
            "it scrolls up and down: {:?} in {:?}",
            low.content_size,
            low.inner_rect
        );
        // The strips (320), the controls of the song (about 130), and the
        // Sound Pad (250 to 350), with the lines between them.
        assert!(
            (700.0..=1_000.0).contains(&low.content_size.y),
            "{:?}",
            low.content_size
        );

        // A high window shows everything: the content does not grow with it.
        let high = draw_central(&mut app, &ctx, 704.0, 4_000.0);
        assert!(high.content_size.y <= high.inner_rect.height());
        assert!((high.content_size.y - low.content_size.y).abs() < 1.0);
        assert!(high.content_size.x <= high.inner_rect.width() + 0.5);
    }

    #[test]
    fn a_window_without_the_song_and_the_pads_shows_the_mic_and_the_master_only() {
        let ctx = context();
        let backend = FakeBackend::default();
        let mut app = stopped_app(&backend, &Config::default());
        assert!(app.sounds.is_none());
        let mut output = draw_central(&mut app, &ctx, 704.0, 600.0);
        for _ in 0..3 {
            output = draw_central(&mut app, &ctx, 704.0, 600.0);
        }
        assert!(output.content_size.y < 400.0, "{:?}", output.content_size);
        assert!(output.content_size.x <= output.inner_rect.width() + 0.5);
        // The two strips are far narrower than the window, but the scroll
        // area takes the whole width. (This egui does not shrink a scroll area
        // when it is drawn on its own as here, so this holds with
        // `auto_shrink` on as well: that the window itself does not shrink is
        // one of the things to look at on a real screen.)
        assert!(
            (output.inner_rect.width() - 704.0).abs() < 1.0,
            "{:?}",
            output.inner_rect
        );
    }

    #[test]
    fn a_library_that_cannot_be_used_is_told_and_the_choices_are_kept_and_nothing_is_written() {
        let ctx = context();
        let root = test_dir("broken");
        let index = root.join("library.toml");
        std::fs::write(&index, "garbage").unwrap();
        let view = LibraryView::open(Some(&root));
        assert!(view.library().is_none());
        let choices = choices_with(Some("bgm-0001"), &[(0, "se-0001", -3.0)]);
        let (mut app, _wake, path) = app_with_sounds(
            "broken",
            view,
            Some(root.clone()),
            &config_with_sounds(&choices),
            true,
        );

        let output = draw_central(&mut app, &ctx, 704.0, 600.0);
        assert!(output.content_size.x <= output.inner_rect.width() + 0.5);
        let sounds = app.sounds.as_ref().unwrap();
        let mut pressed = true;
        ctx.run_ui(input(704.0, 600.0), |ui| {
            pressed = sounds.library_row(ui);
        })
        .drop_without_applying_deltas();
        assert!(!pressed, "drawing does not press the button");
        // The choices stay in the window and in what is saved.
        assert_eq!(sounds.choices(), choices);
        app.sync_sound_choices();
        app.save_on_exit();
        assert!(!path.exists(), "nothing changed, nothing is saved");
        // And the index was not touched.
        assert_eq!(std::fs::read_to_string(&index).unwrap(), "garbage");
    }

    // -- the meters and the repaints --------------------------------------------

    #[test]
    fn the_meter_of_any_of_the_four_channels_keeps_the_window_repainting() {
        let (_root, view) = open_library("meters");
        let (mut app, _wake, _path) =
            app_with_sounds("meters", view, None, &Config::default(), false);
        let now = Instant::now();
        app.update_meters(now);
        assert!(!app.meters_active(), "nothing moves");

        for channel in 0..4 {
            let meter = match channel {
                0 => app.control.mic.meter(),
                1 => app.control.bgm.meter(),
                2 => app.control.se.meter(),
                _ => app.control.master.meter(),
            };
            meter.record(0.5);
            app.update_meters(now);
            assert!(app.meters_active(), "channel {channel}");
            // Taken: the peak is gone; the strip stays active for a moment,
            // so a later time lets it settle.
            app.update_meters(now + Duration::from_secs(60));
            assert!(!app.meters_active(), "channel {channel} settled");
        }
    }

    #[test]
    fn a_playing_song_adds_its_repaint_to_the_other_ones_and_nothing_else_does() {
        let ms = Duration::from_millis;
        let predicted = ms(16);
        assert_eq!(with_position_refresh(None, None, predicted), None);
        assert_eq!(
            with_position_refresh(Some(ms(500)), None, predicted),
            Some(ms(500))
        );
        // Alone: the position, with the predicted time added back.
        assert_eq!(
            with_position_refresh(None, Some(ms(250)), predicted),
            Some(ms(266))
        );
        // The quiet check of half a second is later than the position.
        assert_eq!(
            with_position_refresh(Some(ms(500)), Some(ms(250)), predicted),
            Some(ms(266))
        );
        // Moving meters are earlier than the position: the frame rate cap wins.
        assert_eq!(
            with_position_refresh(Some(ms(16)), Some(ms(250)), predicted),
            Some(ms(16))
        );
    }

    #[test]
    fn without_a_session_no_position_is_asked_for() {
        let (_root, view) = open_library("no-position");
        let (app, _wake, _path) =
            app_with_sounds("no-position", view, None, &Config::default(), false);
        assert!(app.session.is_none());
        assert_eq!(app.position_refresh(), None);
        let sounds = app.sounds.as_ref().unwrap();
        assert_eq!(sounds.panel.transport(None), Transport::NoSong);
    }

    // -- saving the choices -------------------------------------------------------

    #[test]
    fn the_same_choices_every_frame_are_not_a_change() {
        let (root, view) = open_library("same");
        // Pads out of order, a volume, a song and a rule: all of it comes back
        // from the window as it was read.
        let choices = choices_with(
            Some("bgm-0001"),
            &[(3, "se-0001", -4.5), (0, "se-0099", 0.0)],
        );
        let config = config_with_sounds(&choices);
        let (mut app, _wake, path) = app_with_sounds("same", view, Some(root), &config, true);
        let t0 = Instant::now();
        for frame in 0..40u64 {
            app.sync_sound_choices();
            app.save_settings(t0 + Duration::from_millis(100 * frame), false);
        }
        assert!(!path.exists(), "nothing was saved");
        assert_eq!(app.settings.wait(t0 + Duration::from_secs(10)), None);
        assert!(app.save_error.is_none());
    }

    #[test]
    fn a_new_choice_of_the_window_is_saved_once_it_has_settled() {
        let (root, view) = open_library("choose");
        let (mut app, _wake, path) =
            app_with_sounds("choose", view, Some(root), &Config::default(), true);
        let t0 = Instant::now();
        app.sync_sound_choices();
        app.save_settings(t0, false);
        assert!(!path.exists());

        let sounds = app.sounds.as_mut().unwrap();
        sounds
            .panel
            .select(Some(id("bgm-0001")), &sounds.view, None);
        sounds
            .bank
            .assign(2, Some(id("se-0001")), &sounds.view, None, &sounds.loader);
        sounds.bank.set_gain_db(2, -6.0);

        app.sync_sound_choices();
        app.save_settings(t0 + Duration::from_millis(100), false);
        assert!(!path.exists(), "it has not settled");
        assert!(app.settings.wait(t0 + Duration::from_millis(100)).is_some());
        app.sync_sound_choices();
        app.save_settings(t0 + Duration::from_millis(500), false);

        let saved = SoundChoices::from_config(&Config::load(&path).unwrap());
        assert_eq!(saved.bgm_asset, Some(id("bgm-0001")));
        assert_eq!(saved.pads.len(), 1);
        assert_eq!(saved.pads[0].slot, 2);
        assert_eq!(saved.pads[0].asset, id("se-0001"));
        assert_eq!(saved.pads[0].gain_db, -6.0);
    }

    // -- the session ----------------------------------------------------------------

    #[test]
    fn the_user_stopping_the_audio_forgets_that_a_song_was_playing_and_an_error_remembers_it() {
        let (_root, view) = open_library("stopping");
        let (mut app, _wake, _path) =
            app_with_sounds("stopping", view, None, &Config::default(), false);

        // A stop by the user.
        app.sounds.as_mut().unwrap().stopped_while_playing = true;
        app.stop();
        assert!(!app.sounds.as_ref().unwrap().stopped_while_playing);
        assert!(!app.take_was_playing());

        // The audio died while a song played: it is told once, at the start.
        app.sounds.as_mut().unwrap().stopped_while_playing = true;
        assert!(app.take_was_playing());
        assert!(!app.take_was_playing(), "only once");
    }

    #[test]
    fn a_start_that_does_not_work_keeps_that_a_song_was_playing_for_the_next_start() {
        let (_root, view) = open_library("failed-start");
        let (mut app, _wake, _path) =
            app_with_sounds("failed-start", view, None, &Config::default(), false);

        // The audio stopped by itself while a song played. The fake cannot
        // open a stream, so the start fails after the lists and the choice of
        // the devices were fine: the song was not forgotten.
        app.sounds.as_mut().unwrap().stopped_while_playing = true;
        app.start_selected();
        assert!(
            matches!(app.status, Status::Error { .. }),
            "{:?}",
            app.status
        );
        assert!(app.sounds.as_ref().unwrap().stopped_while_playing);
        // And again: still kept, and told only when it is asked for.
        app.start_selected();
        assert!(app.sounds.as_ref().unwrap().stopped_while_playing);
        assert!(app.take_was_playing());
        assert!(!app.sounds.as_ref().unwrap().stopped_while_playing);

        // Nothing was playing: nothing is kept.
        app.start_selected();
        assert!(!app.sounds.as_ref().unwrap().stopped_while_playing);
    }

    #[test]
    fn a_start_that_fails_before_the_devices_are_chosen_keeps_it_too() {
        let (_root, view) = open_library("failed-lists");
        let backend = FakeBackend::default();
        backend
            .devices
            .replace(vec![device("fake:default"), device("fake:headset")]);
        let path = settings_path("failed-lists");
        let app = app_with_settings(&backend, &Config::default(), Some(path));
        let (sounds, _wake) = sounds_for(view, None, &SoundChoices::default());
        let mut app = app.with_sounds(sounds);

        // The lists cannot be read.
        backend.list_fails.set(true);
        app.sounds.as_mut().unwrap().stopped_while_playing = true;
        app.start_selected();
        assert!(app.sounds.as_ref().unwrap().stopped_while_playing);

        // No device to choose.
        backend.list_fails.set(false);
        backend.devices.replace(Vec::new());
        app.start_selected();
        assert!(app.sounds.as_ref().unwrap().stopped_while_playing);
    }

    #[test]
    fn a_note_is_added_to_the_ones_the_window_has() {
        let backend = FakeBackend::default();
        let app = stopped_app(&backend, &Config::default());
        let app = app.with_notice("A");
        assert_eq!(app.notice.as_deref(), Some("A"));
        let app = app.with_notice("B");
        assert_eq!(app.notice.as_deref(), Some("A  B"));
    }

    #[test]
    fn a_window_without_sounds_passes_no_choices_and_keeps_the_saved_ones() {
        let choices = choices_with(
            Some("bgm-0001"),
            &[(2, "se-0001", -3.0), (5, "se-0099", 1.5)],
        );
        let config = config_with_sounds(&choices);
        let backend = FakeBackend::default();
        let path = settings_path("without-sounds");
        let mut app = app_with_settings(&backend, &config, Some(path.clone()));
        assert!(app.sounds.is_none());

        app.control.mic.set_gain_db(2.0);
        app.sync_sound_choices();
        app.save_on_exit();
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.mixer.mic_gain_db(), Some(2.0));
        assert_eq!(SoundChoices::from_config(&saved), choices);
    }

    #[test]
    fn a_window_without_sounds_has_nothing_to_tell_at_a_start() {
        let backend = FakeBackend::default();
        let mut app = stopped_app(&backend, &Config::default());
        assert!(!app.take_was_playing());
        app.stop();
        app.start_sounds(true);
        assert!(app.sounds.is_none());
    }

    #[test]
    fn a_started_session_opens_the_song_with_a_note_only_after_a_song_that_played() {
        let (root, _first_view) = open_library("started");
        let choices = choices_with(Some("bgm-0001"), &[(0, "se-0001", 0.0)]);
        for was_playing in [false, true] {
            let (mut sounds, wake) =
                sounds_for(LibraryView::open(Some(&root)), Some(root.clone()), &choices);
            let audio = AudioSide::new();
            let mut slots = FakeSlots::default();
            sounds.bgm_started(&audio.bgm, was_playing);
            sounds.se_started(&mut slots);
            assert_eq!(
                sounds.panel.notice().is_some(),
                was_playing,
                "was_playing {was_playing}"
            );
            // The pads are read again for the rate of the new player.
            assert!(sounds.bank.loading());
            settle(&mut sounds, &wake, &mut slots);
            assert!(matches!(sounds.bank.state(0), PadState::Ready { .. }));
            assert_eq!(slots.sounds, vec![0]);
        }
    }

    #[test]
    fn reading_the_library_again_reads_only_the_pads_that_failed() {
        let root = test_dir("reload");
        // The index lists the sound effect, but its file is not there yet.
        let index = library_files(&root);
        let se_file = root.join("sounds").join("se").join("拍手 テスト.wav");
        let kept = std::fs::read(&se_file).unwrap();
        std::fs::remove_file(&se_file).unwrap();
        // And a second sound effect that works from the start.
        write_wav(&root.join("sounds").join("se").join("鐘.wav"), 480);
        let index = format!("{index}{}", index_entry("se-0002", "se", "鐘.wav"));
        std::fs::write(root.join("library.toml"), &index).unwrap();

        let view = LibraryView::open(Some(&root));
        let choices = choices_with(None, &[(0, "se-0001", 0.0), (1, "se-0002", 0.0)]);
        let (mut sounds, wake) = sounds_for(view, Some(root.clone()), &choices);
        let mut slots = FakeSlots::default();
        sounds.se_started(&mut slots);
        settle(&mut sounds, &wake, &mut slots);
        assert!(matches!(sounds.bank.state(0), PadState::Failed(_)));
        assert!(matches!(sounds.bank.state(1), PadState::Ready { .. }));

        // The file comes back, and the library is read again.
        std::fs::write(&se_file, kept).unwrap();
        slots.cleared.clear();
        sounds.reload_view(None);
        sounds.reload_pads(Some(&mut slots));
        assert_eq!(slots.cleared, vec![0], "only the pad that failed");
        assert!(matches!(sounds.bank.state(0), PadState::Loading));
        assert!(
            matches!(sounds.bank.state(1), PadState::Ready { .. }),
            "the pad that works is not read again"
        );
        settle(&mut sounds, &wake, &mut slots);
        assert!(matches!(sounds.bank.state(0), PadState::Ready { .. }));
        assert_eq!(sounds.bank.asset(0), Some(&id("se-0001")), "the same sound");
    }
}
