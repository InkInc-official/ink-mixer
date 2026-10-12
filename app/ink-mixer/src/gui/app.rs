//! The main window: the MIC and MASTER strips (#40).
//!
//! The window owns the audio session that plays MIC -> Gain -> MASTER
//! (`session.rs`) and shares a `MixerControl` with it. The strips read and
//! set the controls; the window never touches the audio engine itself.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use ink_core::graph::MixerControl;
use tracing::error;

use super::frame_limiter::{FrameLimiter, FrameRate};
use super::strip::Strip;
use crate::session::AudioSession;

/// While audio runs but the meters are still, the window checks this often
/// for new sound and for a stream that stopped.
const QUIET_CHECK: Duration = Duration::from_millis(500);

/// The main window.
pub struct InkMixerApp {
    control: Arc<MixerControl>,
    /// The running audio, or `None` when it could not start or stopped.
    session: Option<AudioSession>,
    /// The line at the bottom: the devices in use, or an error.
    status: Status,
    mic: Strip,
    master: Strip,
    limiter: FrameLimiter,
    fps: FpsCounter,
}

/// What the bottom line shows.
enum Status {
    Running(String),
    Error(String),
}

impl InkMixerApp {
    /// Creates the window for `control`, with the audio session that was
    /// started for it (or the error that kept it from starting).
    pub fn new(control: Arc<MixerControl>, session: Result<AudioSession, String>) -> Self {
        let now = Instant::now();
        let (session, status) = match session {
            Ok(session) => {
                let status = Status::Running(format!(
                    "入力: {}  /  出力: {}  /  {} Hz",
                    session.input_device().name,
                    session.output_device().name,
                    session.sample_rate()
                ));
                (Some(session), status)
            }
            Err(e) => (
                None,
                Status::Error(format!("音声を開始できませんでした: {e}")),
            ),
        };
        Self {
            control,
            session,
            status,
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
            self.status = Status::Error(format!("音声が止まりました: {e}"));
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
            Status::Error(text) => {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 90), text);
            }
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
        egui::Panel::bottom("status").show(ui, |ui| self.status_line(ui));
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));

        let meters_active = self.mic.active() || self.master.active();
        let predicted = Duration::from_secs_f32(ui.input(|i| i.predicted_dt));
        if let Some(delay) = repaint_delay(
            meters_active,
            self.session.is_some(),
            &mut self.limiter,
            frame_start,
            Instant::now(),
            predicted,
        ) {
            ui.ctx().request_repaint_after(delay);
        }
    }
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

    #[test]
    fn a_session_that_failed_to_start_shows_the_error() {
        let control = Arc::new(MixerControl::new());
        let app = InkMixerApp::new(control, Err("no default input device".to_string()));
        assert!(app.session.is_none());
        match &app.status {
            Status::Error(text) => assert!(text.contains("no default input device")),
            Status::Running(_) => panic!("expected an error"),
        }
    }
}
