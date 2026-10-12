//! Level meters: what the bar shows, and drawing it.
//!
//! The audio thread keeps the largest peak since the last read
//! (`PeakMeter`, ADR-0007); the window reads it once per frame and feeds it
//! to a [`MeterState`]. The state rises at once, falls slowly, holds the
//! peak for a moment, and latches a clip indicator. It takes the current
//! time as an argument, so it can be tested without a clock.

use std::time::{Duration, Instant};

use eframe::egui;
use ink_core::graph::{SILENCE_DB, gain_to_db};

/// The bottom of the scale; anything at or below it shows as empty.
pub const FLOOR_DB: f32 = SILENCE_DB;
/// The top of the scale (full scale).
pub const TOP_DB: f32 = 0.0;
/// How fast the bar falls when the level drops.
pub const FALL_DB_PER_SEC: f32 = 24.0;
/// How long the peak hold line stays before it falls.
pub const HOLD: Duration = Duration::from_millis(1500);
/// How fast the peak hold line falls after [`HOLD`].
pub const HOLD_FALL_DB_PER_SEC: f32 = 12.0;
/// The bar is yellow from this level up.
pub const YELLOW_DB: f32 = -18.0;
/// The bar is red from this level up.
pub const RED_DB: f32 = -6.0;

const GREEN: egui::Color32 = egui::Color32::from_rgb(70, 190, 110);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(230, 190, 60);
const RED: egui::Color32 = egui::Color32::from_rgb(220, 60, 60);

/// What one meter shows.
#[derive(Debug, Clone)]
pub struct MeterState {
    /// The bar level in dB; [`FLOOR_DB`] is empty.
    bar_db: f32,
    /// The peak hold level in dB.
    hold_db: f32,
    /// When the peak hold was last raised.
    hold_set: Instant,
    /// Set when a peak reached full scale; stays until cleared.
    clipped: bool,
    last_update: Instant,
}

impl MeterState {
    /// Creates an empty meter.
    pub fn new(now: Instant) -> Self {
        Self {
            bar_db: FLOOR_DB,
            hold_db: FLOOR_DB,
            hold_set: now,
            clipped: false,
            last_update: now,
        }
    }

    /// Feeds the peak read at `now` (a linear level, from `PeakMeter::take`).
    pub fn update(&mut self, peak: f32, now: Instant) {
        let dt = now.saturating_duration_since(self.last_update);
        self.last_update = now;

        if peak >= 1.0 {
            self.clipped = true;
        }

        // The bar jumps up to a higher peak and otherwise falls slowly.
        let level = gain_to_db(peak).max(FLOOR_DB);
        let fallen = (self.bar_db - FALL_DB_PER_SEC * dt.as_secs_f32()).max(FLOOR_DB);
        self.bar_db = level.max(fallen);

        // The hold line follows the bar up, waits, then falls slowly.
        if self.bar_db >= self.hold_db {
            self.hold_db = self.bar_db;
            self.hold_set = now;
        } else {
            let since_set = now.saturating_duration_since(self.hold_set);
            if since_set > HOLD {
                let falling = (since_set - HOLD).min(dt);
                self.hold_db -= HOLD_FALL_DB_PER_SEC * falling.as_secs_f32();
                self.hold_db = self.hold_db.max(self.bar_db);
            }
        }
    }

    /// Returns the bar height in 0.0..=1.0.
    pub fn bar_fraction(&self) -> f32 {
        fraction(self.bar_db)
    }

    /// Returns the peak hold line height in 0.0..=1.0.
    pub fn hold_fraction(&self) -> f32 {
        fraction(self.hold_db)
    }

    /// Returns the peak hold level in dB (may exceed 0 dBFS).
    pub fn hold_db(&self) -> f32 {
        self.hold_db
    }

    /// Returns whether a peak reached full scale since the last clear.
    pub fn clipped(&self) -> bool {
        self.clipped
    }

    /// Turns the clip indicator off.
    pub fn clear_clip(&mut self) {
        self.clipped = false;
    }

    /// Returns whether anything is still moving (above the floor), so the
    /// window has to keep repainting.
    pub fn active(&self) -> bool {
        self.bar_db > FLOOR_DB || self.hold_db > FLOOR_DB
    }
}

/// Maps a level in dB to 0.0..=1.0, linear in dB from the floor to the top.
fn fraction(db: f32) -> f32 {
    ((db - FLOOR_DB) / (TOP_DB - FLOOR_DB)).clamp(0.0, 1.0)
}

/// Formats a level in dBFS for display, such as `-12.3 dBFS` or `-∞ dBFS`.
pub fn format_dbfs(db: f32) -> String {
    if db > FLOOR_DB {
        format!("{db:+.1} dBFS")
    } else {
        "-∞ dBFS".to_string()
    }
}

/// Draws a vertical meter of `size`: green, yellow, and red zones up to the
/// bar, and the peak hold line.
pub fn draw(ui: &mut egui::Ui, state: &MeterState, size: egui::Vec2) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(40));

    let y = |f: f32| rect.bottom() - rect.height() * f;
    let bar = state.bar_fraction();
    for (from_db, to_db, color) in [
        (FLOOR_DB, YELLOW_DB, GREEN),
        (YELLOW_DB, RED_DB, YELLOW),
        (RED_DB, TOP_DB, RED),
    ] {
        let low = fraction(from_db);
        let high = fraction(to_db).min(bar);
        if high > low {
            let zone = egui::Rect::from_min_max(
                egui::pos2(rect.left(), y(high)),
                egui::pos2(rect.right(), y(low)),
            );
            painter.rect_filled(zone, 0.0, color);
        }
    }

    if state.hold_db() > FLOOR_DB {
        let hold_y = y(state.hold_fraction());
        painter.line_segment(
            [
                egui::pos2(rect.left(), hold_y),
                egui::pos2(rect.right(), hold_y),
            ],
            egui::Stroke::new(2.0, egui::Color32::from_gray(230)),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use ink_core::graph::db_to_gain;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-3,
            "{actual} is not {expected}"
        );
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn bar_rises_at_once_and_falls_24_db_per_second() {
        let t0 = Instant::now();
        let mut meter = MeterState::new(t0);
        meter.update(db_to_gain(-12.0), t0);
        assert_close(meter.bar_db, -12.0);

        // Silence for half a second: 12 dB lower.
        meter.update(0.0, at(t0, 500));
        assert_close(meter.bar_db, -24.0);

        // A higher peak takes over at once.
        meter.update(db_to_gain(-6.0), at(t0, 516));
        assert_close(meter.bar_db, -6.0);
    }

    #[test]
    fn hold_waits_then_falls_12_db_per_second() {
        let t0 = Instant::now();
        let mut meter = MeterState::new(t0);
        meter.update(db_to_gain(-10.0), t0);

        meter.update(0.0, at(t0, 1000));
        assert_close(meter.hold_db(), -10.0);
        meter.update(0.0, at(t0, 1500));
        assert_close(meter.hold_db(), -10.0);

        // 0.5 s after the 1.5 s hold: 6 dB lower.
        meter.update(0.0, at(t0, 2000));
        assert_close(meter.hold_db(), -16.0);
    }

    #[test]
    fn hold_never_falls_below_the_bar() {
        let t0 = Instant::now();
        let mut meter = MeterState::new(t0);
        meter.update(db_to_gain(-10.0), t0);
        // A steady -20 dB keeps the bar there while the hold falls to it.
        for ms in (100..=6000).step_by(100) {
            meter.update(db_to_gain(-20.0), at(t0, ms));
        }
        assert_close(meter.bar_db, -20.0);
        assert_close(meter.hold_db(), -20.0);
    }

    #[test]
    fn scale_is_linear_in_db_from_floor_to_top() {
        assert_eq!(fraction(-60.0), 0.0);
        assert_eq!(fraction(-80.0), 0.0);
        assert_close(fraction(-30.0), 0.5);
        assert_eq!(fraction(0.0), 1.0);
        assert_eq!(fraction(6.0), 1.0);
    }

    #[test]
    fn clip_latches_until_cleared() {
        let t0 = Instant::now();
        let mut meter = MeterState::new(t0);
        meter.update(0.99, t0);
        assert!(!meter.clipped());

        meter.update(1.0, at(t0, 16));
        assert!(meter.clipped());
        meter.update(0.0, at(t0, 5000));
        assert!(meter.clipped(), "stays on after the level drops");

        meter.clear_clip();
        assert!(!meter.clipped());
    }

    #[test]
    fn meter_is_inactive_once_everything_has_fallen() {
        let t0 = Instant::now();
        let mut meter = MeterState::new(t0);
        assert!(!meter.active());

        meter.update(db_to_gain(-6.0), t0);
        assert!(meter.active());
        // The bar needs 2.25 s and the hold 1.5 s + 4.5 s to reach -60 dB.
        meter.update(0.0, at(t0, 3000));
        assert!(meter.active(), "the hold line is still falling");
        for ms in (3100..=6100).step_by(100) {
            meter.update(0.0, at(t0, ms));
        }
        assert!(!meter.active());
    }

    #[test]
    fn levels_format_in_dbfs() {
        assert_eq!(format_dbfs(-12.34), "-12.3 dBFS");
        assert_eq!(format_dbfs(3.0), "+3.0 dBFS");
        assert_eq!(format_dbfs(-60.0), "-∞ dBFS");
    }
}
