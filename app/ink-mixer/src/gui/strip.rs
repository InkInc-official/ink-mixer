//! One mixer channel strip: name, clip indicator, meter, fader, levels, and
//! mute.
//!
//! The strip uses only the channel's `ChannelControl` (#35): it reads and
//! sets the gain in dB, the mute, and the meter. It never touches the audio
//! engine itself.

use std::time::Instant;

use eframe::egui;
use ink_core::graph::{ChannelControl, SILENCE_DB};

use super::meter::{self, MeterState};

/// Width of a strip.
const STRIP_WIDTH: f32 = 120.0;
/// Height of the meter and the fader.
const FADER_HEIGHT: f32 = 200.0;
/// Width of the meter.
const METER_WIDTH: f32 = 16.0;
/// Fader step in dB.
const FADER_STEP_DB: f64 = 0.5;

/// The state of one strip on screen.
pub struct Strip {
    name: &'static str,
    has_mute: bool,
    meter: MeterState,
}

impl Strip {
    /// Creates a strip titled `name`, with a mute button if `has_mute`.
    pub fn new(name: &'static str, has_mute: bool, now: Instant) -> Self {
        Self {
            name,
            has_mute,
            meter: MeterState::new(now),
        }
    }

    /// Reads the channel's meter. Call once per frame, before [`show`].
    ///
    /// [`show`]: Strip::show
    pub fn update_meter(&mut self, control: &ChannelControl, now: Instant) {
        self.meter.update(control.meter().take(), now);
    }

    /// Returns whether the meter is still moving.
    pub fn active(&self) -> bool {
        self.meter.active()
    }

    /// Draws the strip and applies the fader and mute to `control`.
    pub fn show(&mut self, ui: &mut egui::Ui, control: &ChannelControl) {
        ui.group(|ui| {
            ui.set_width(STRIP_WIDTH);
            ui.vertical_centered(|ui| {
                ui.strong(self.name);
                self.clip_indicator(ui);

                ui.horizontal(|ui| {
                    meter::draw(ui, &self.meter, egui::vec2(METER_WIDTH, FADER_HEIGHT))
                        .on_hover_text("ピーク（白い線は直近の最大）");
                    fader(ui, control);
                });

                ui.monospace(meter::format_dbfs(self.meter.hold_db()));
                ui.monospace(format_gain_db(control.gain_db()));

                if self.has_mute {
                    let mut muted = control.is_muted();
                    if ui.toggle_value(&mut muted, "ミュート").changed() {
                        control.set_muted(muted);
                    }
                }
            });
        });
    }

    /// The "CLIP" lamp: red after a peak reached 0 dBFS, until clicked.
    fn clip_indicator(&mut self, ui: &mut egui::Ui) {
        let clipped = self.meter.clipped();
        let (text_color, fill) = if clipped {
            (egui::Color32::WHITE, egui::Color32::from_rgb(200, 40, 40))
        } else {
            (egui::Color32::from_gray(110), egui::Color32::from_gray(45))
        };
        let lamp = egui::Button::new(egui::RichText::new("CLIP").small().color(text_color))
            .fill(fill)
            .small();
        let response = ui.add(lamp).on_hover_text(if clipped {
            "0 dBFS に届きました。クリックで消えます"
        } else {
            "0 dBFS に届くと赤くなります"
        });
        if response.clicked() {
            self.meter.clear_clip();
        }
    }
}

/// A vertical fader for the channel's gain in dB, within its range.
fn fader(ui: &mut egui::Ui, control: &ChannelControl) {
    let range = control.range();
    let mut db = control.gain_db();
    ui.spacing_mut().slider_width = FADER_HEIGHT;
    let slider = egui::Slider::new(&mut db, range.min..=range.max)
        .vertical()
        .step_by(FADER_STEP_DB)
        .show_value(false);
    if ui.add(slider).changed() {
        control.set_gain_db(db);
    }
}

/// Formats a gain in dB for display, such as `+0.0 dB`; the lowest value,
/// which is silence, shows as `-∞ dB`.
pub fn format_gain_db(db: f32) -> String {
    if db > SILENCE_DB {
        format!("{db:+.1} dB")
    } else {
        "-∞ dB".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ink_core::graph::MIC_GAIN_RANGE;

    #[test]
    fn gains_format_with_sign_and_silence_as_minus_infinity() {
        assert_eq!(format_gain_db(0.0), "+0.0 dB");
        assert_eq!(format_gain_db(-12.0), "-12.0 dB");
        assert_eq!(format_gain_db(24.0), "+24.0 dB");
        assert_eq!(format_gain_db(-60.0), "-∞ dB");
    }

    #[test]
    fn update_meter_takes_the_peak_from_the_channel() {
        let now = Instant::now();
        let control = ChannelControl::new(MIC_GAIN_RANGE);
        let mut strip = Strip::new("マイク", true, now);
        assert!(!strip.active());

        control.meter().record(0.5);
        strip.update_meter(&control, now);
        assert!(strip.active());
        // The peak was taken, so the channel's meter is back to 0.
        assert_eq!(control.meter().take(), 0.0);
    }
}
