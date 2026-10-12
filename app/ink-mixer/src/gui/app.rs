//! The main window (#39: the GUI foundation).
//!
//! For now it shows sample Japanese labels, the frame rate switch, and a
//! moving bar for checking the frame rate cap. The mixer strips replace the
//! sample and the bar in #40.

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use eframe::egui;

use super::frame_limiter::{FrameLimiter, FrameRate};

/// The main window.
pub struct InkMixerApp {
    limiter: FrameLimiter,
    fps: FpsCounter,
    /// The moving bar for checking the frame rate cap. Removed in #40.
    demo: DemoBar,
}

impl InkMixerApp {
    /// Creates the window state at 60 fps with nothing moving.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            limiter: FrameLimiter::new(FrameRate::default(), now),
            fps: FpsCounter::new(now),
            demo: DemoBar::new(now),
        }
    }

    /// Whether anything on screen moves and needs repainting by itself.
    /// The meters set this in #40.
    fn animating(&self) -> bool {
        self.demo.running
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

    fn central(&mut self, ui: &mut egui::Ui) {
        ui.heading("ミキサー");
        ui.label("マイク ・ BGM ・ 効果音 ・ 外部入力 ・ マスター");
        ui.label("ノイズ除去 ・ 声を整える ・ 低音カット ・ リバーブ");
        ui.weak("ミキサーの画面は、この土台の上に作ります（#40）。");
        ui.add_space(16.0);

        ui.group(|ui| {
            ui.label("動作確認用（#40 で消します）");
            ui.horizontal(|ui| {
                let label = if self.demo.running {
                    "止める"
                } else {
                    "動かす"
                };
                if ui.toggle_value(&mut self.demo.running, label).changed() {
                    self.demo.start = Instant::now();
                }
                ui.weak("動かしている間だけ、上限の fps で描き直します。");
            });
            draw_bar(ui, self.demo.level(Instant::now()));
        });
    }
}

impl Default for InkMixerApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for InkMixerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        self.fps.count(frame_start);

        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));

        let predicted = Duration::from_secs_f32(ui.input(|i| i.predicted_dt));
        if let Some(delay) = repaint_delay(
            self.animating(),
            &mut self.limiter,
            frame_start,
            Instant::now(),
            predicted,
        ) {
            ui.ctx().request_repaint_after(delay);
        }
    }
}

/// Decides whether to schedule the next frame. While something moves, the
/// next frame comes from the frame rate cap; otherwise nothing is scheduled
/// and egui repaints only on input, so an idle window uses almost no CPU.
fn repaint_delay(
    animating: bool,
    limiter: &mut FrameLimiter,
    frame_start: Instant,
    now: Instant,
    predicted_dt: Duration,
) -> Option<Duration> {
    animating.then(|| limiter.delay_after(frame_start, now, predicted_dt))
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

/// A bar that moves while running, for checking the frame rate cap.
#[derive(Debug)]
struct DemoBar {
    running: bool,
    start: Instant,
}

impl DemoBar {
    fn new(now: Instant) -> Self {
        Self {
            running: false,
            start: now,
        }
    }

    /// The bar length in 0.0..=1.0: a slow sine while running, 0 otherwise.
    fn level(&self, now: Instant) -> f32 {
        if !self.running {
            return 0.0;
        }
        let t = now.saturating_duration_since(self.start).as_secs_f32();
        0.5 - 0.5 * (t * TAU * 0.5).cos()
    }
}

fn draw_bar(ui: &mut egui::Ui, level: f32) {
    let width = ui.available_width().min(400.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 16.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(40));
    let mut filled = rect;
    filled.set_width(rect.width() * level.clamp(0.0, 1.0));
    painter.rect_filled(filled, 2.0, egui::Color32::from_rgb(70, 190, 110));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_scheduled_while_idle() {
        let t0 = Instant::now();
        let mut limiter = FrameLimiter::new(FrameRate::Fps60, t0);
        assert_eq!(
            repaint_delay(false, &mut limiter, t0, t0, Duration::ZERO),
            None
        );
    }

    #[test]
    fn the_cap_schedules_frames_while_animating() {
        let t0 = Instant::now();
        let mut limiter = FrameLimiter::new(FrameRate::Fps30, t0);
        assert_eq!(
            repaint_delay(true, &mut limiter, t0, t0, Duration::ZERO),
            Some(FrameRate::Fps30.interval())
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
    fn demo_bar_moves_only_while_running() {
        let t0 = Instant::now();
        let mut bar = DemoBar::new(t0);
        assert_eq!(bar.level(t0 + Duration::from_secs(1)), 0.0);

        bar.running = true;
        assert_eq!(bar.level(t0), 0.0);
        let top = bar.level(t0 + Duration::from_secs(1));
        assert!((top - 1.0).abs() < 1e-6, "{top}");
    }
}
