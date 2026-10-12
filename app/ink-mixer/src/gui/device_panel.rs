//! The device pickers and the start / stop button (#41).
//!
//! The panel holds the device lists and the selected IDs, and draws them. It
//! never touches the audio: [`DevicePanel::show`] only tells the window what
//! the user asked for, and the window starts and stops the session.
//!
//! The selected ID is kept even when the device is not in the list, so a
//! device that disappeared for a moment (or was unplugged) is still shown as
//! selected, and the window can say that it was not found.

use eframe::egui;
use ink_backend::{AudioBackend, DeviceId, DeviceInfo};

/// What the user asked for in the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelAction {
    /// Start with the selected devices (the audio is stopped).
    Start,
    /// Stop the audio.
    Stop,
    /// A device was changed while running: stop and start again.
    Restart,
    /// Get the device lists again.
    Refresh,
}

/// Why the selected devices cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    NoInput,
    NoOutput,
    /// The selected input is not in the list.
    InputMissing(DeviceId),
    /// The selected output is not in the list.
    OutputMissing(DeviceId),
}

impl SelectionError {
    /// A one-line explanation in Japanese for the window.
    pub fn message_ja(&self) -> String {
        match self {
            Self::NoInput => "入力デバイスが選ばれていません".to_string(),
            Self::NoOutput => "出力デバイスが選ばれていません".to_string(),
            Self::InputMissing(id) => format!(
                "選んだ入力デバイスが見つかりません（{id}）。もう一度「開始」を押してください"
            ),
            Self::OutputMissing(id) => format!(
                "選んだ出力デバイスが見つかりません（{id}）。もう一度「開始」を押してください"
            ),
        }
    }
}

/// The device lists and the selection.
#[derive(Debug, Default)]
pub struct DevicePanel {
    inputs: Vec<DeviceInfo>,
    outputs: Vec<DeviceInfo>,
    selected_input: Option<DeviceId>,
    selected_output: Option<DeviceId>,
}

impl DevicePanel {
    /// Creates a panel with empty lists and nothing selected.
    pub fn new() -> Self {
        Self::default()
    }

    /// Gets both lists from `backend`. On an error, the lists stay as they
    /// were and the error is returned for the window to show.
    pub fn refresh(&mut self, backend: &dyn AudioBackend) -> Result<(), String> {
        let inputs = backend
            .enumerate_input_devices()
            .map_err(|e| format!("failed to list input devices: {e}"))?;
        let outputs = backend
            .enumerate_output_devices()
            .map_err(|e| format!("failed to list output devices: {e}"))?;
        self.set_lists(inputs, outputs);
        Ok(())
    }

    /// Replaces the lists. The selection is kept, even if it is not listed.
    pub fn set_lists(&mut self, inputs: Vec<DeviceInfo>, outputs: Vec<DeviceInfo>) {
        self.inputs = inputs;
        self.outputs = outputs;
    }

    /// Sets the selection.
    #[cfg(test)]
    pub fn select(&mut self, input: Option<DeviceId>, output: Option<DeviceId>) {
        self.selected_input = input;
        self.selected_output = output;
    }

    /// Selects the first listed ID of each candidate list (see
    /// [`first_listed`]), for the initial selection.
    pub fn select_first_listed(
        &mut self,
        input: &[Option<&DeviceId>],
        output: &[Option<&DeviceId>],
    ) {
        self.selected_input = first_listed(&self.inputs, input);
        self.selected_output = first_listed(&self.outputs, output);
    }

    /// Returns the selected input ID.
    #[cfg(test)]
    pub fn selected_input(&self) -> Option<&DeviceId> {
        self.selected_input.as_ref()
    }

    /// Returns the selected output ID.
    #[cfg(test)]
    pub fn selected_output(&self) -> Option<&DeviceId> {
        self.selected_output.as_ref()
    }

    /// Looks up the selected devices in the lists.
    pub fn selected_devices(&self) -> Result<(DeviceInfo, DeviceInfo), SelectionError> {
        let input_id = self
            .selected_input
            .as_ref()
            .ok_or(SelectionError::NoInput)?;
        let output_id = self
            .selected_output
            .as_ref()
            .ok_or(SelectionError::NoOutput)?;
        let input = find(&self.inputs, input_id)
            .ok_or_else(|| SelectionError::InputMissing(input_id.clone()))?;
        let output = find(&self.outputs, output_id)
            .ok_or_else(|| SelectionError::OutputMissing(output_id.clone()))?;
        Ok((input.clone(), output.clone()))
    }

    /// Returns whether both selected devices are listed, so "start" can work.
    pub fn can_start(&self) -> bool {
        self.selected_devices().is_ok()
    }

    /// Draws the panel. `running` tells whether the audio is running.
    pub fn show(&mut self, ui: &mut egui::Ui, running: bool) -> Option<PanelAction> {
        let before = (self.selected_input.clone(), self.selected_output.clone());
        device_row(
            ui,
            "入力:",
            "input_device",
            &self.inputs,
            &mut self.selected_input,
        );
        device_row(
            ui,
            "出力:",
            "output_device",
            &self.outputs,
            &mut self.selected_output,
        );
        let changed = before != (self.selected_input.clone(), self.selected_output.clone());

        let mut pressed = None;
        ui.horizontal(|ui| {
            let (label, action) = if running {
                ("停止", PanelAction::Stop)
            } else {
                ("開始", PanelAction::Start)
            };
            let enabled = running || self.can_start();
            if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                pressed = Some(action);
            }
            if ui.button("一覧を更新").clicked() {
                pressed = Some(PanelAction::Refresh);
            }
        });
        decide(pressed, changed, running)
    }
}

/// Combines the button and a changed selection into one action: a button
/// wins; a changed selection restarts only while running.
fn decide(pressed: Option<PanelAction>, changed: bool, running: bool) -> Option<PanelAction> {
    match pressed {
        Some(action) => Some(action),
        None if changed && running => Some(PanelAction::Restart),
        None => None,
    }
}

/// Draws one labelled drop-down list of devices.
fn device_row(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    list: &[DeviceInfo],
    selected: &mut Option<DeviceId>,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(id)
            .width((ui.available_width() - 8.0).max(200.0))
            .selected_text(device_label(list, selected.as_ref()))
            .show_ui(ui, |ui| {
                for device in list {
                    ui.selectable_value(
                        selected,
                        Some(device.id.clone()),
                        format!("{} ({})", device.name, device.id),
                    );
                }
            });
    });
}

/// Returns the text of the selected device: its name and ID, or a note when
/// nothing is selected or the device is not in the list.
fn device_label(list: &[DeviceInfo], selected: Option<&DeviceId>) -> String {
    match selected {
        None => "（選ばれていません）".to_string(),
        Some(id) => match find(list, id) {
            Some(device) => format!("{} ({})", device.name, device.id),
            None => format!("（見つかりません） {id}"),
        },
    }
}

fn find<'a>(list: &'a [DeviceInfo], id: &DeviceId) -> Option<&'a DeviceInfo> {
    list.iter().find(|d| d.id == *id)
}

/// Returns the first candidate that is in `list`. Candidates are given in
/// order of preference (for example the ID that was started, the configured
/// ID, the default), and `None` entries are skipped.
pub fn first_listed(list: &[DeviceInfo], candidates: &[Option<&DeviceId>]) -> Option<DeviceId> {
    candidates
        .iter()
        .flatten()
        .find(|id| find(list, id).is_some())
        .map(|id| (*id).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(id),
            name: name.to_string(),
            channels: 2,
            sample_rates: vec![48_000],
        }
    }

    fn headset() -> DeviceInfo {
        device("alsa:headset", "Logi USB Headset")
    }

    fn hid() -> DeviceInfo {
        device("alsa:hid", "USB Audio and HID")
    }

    #[test]
    fn first_listed_follows_the_order_and_skips_unlisted_ids() {
        let list = [headset(), hid()];
        let started = DeviceId::new("alsa:hid");
        let configured = DeviceId::new("alsa:headset");
        let gone = DeviceId::new("alsa:gone");

        assert_eq!(
            first_listed(&list, &[Some(&started), Some(&configured)]),
            Some(started.clone())
        );
        assert_eq!(
            first_listed(&list, &[Some(&gone), Some(&configured)]),
            Some(configured.clone())
        );
        assert_eq!(first_listed(&list, &[None, Some(&gone), None]), None);
        assert_eq!(first_listed(&list, &[]), None);
        assert_eq!(first_listed(&[], &[Some(&started)]), None);
    }

    #[test]
    fn selected_devices_are_found_in_the_lists() {
        let mut panel = DevicePanel::new();
        panel.set_lists(vec![headset(), hid()], vec![headset()]);
        panel.select(
            Some(DeviceId::new("alsa:hid")),
            Some(DeviceId::new("alsa:headset")),
        );

        let (input, output) = panel.selected_devices().unwrap();
        assert_eq!(input.name, "USB Audio and HID");
        assert_eq!(output.name, "Logi USB Headset");
        assert!(panel.can_start());
    }

    #[test]
    fn nothing_selected_or_nothing_listed_cannot_start() {
        let mut panel = DevicePanel::new();
        assert_eq!(panel.selected_devices(), Err(SelectionError::NoInput));
        assert!(!panel.can_start());

        panel.set_lists(vec![headset()], vec![headset()]);
        panel.select(Some(DeviceId::new("alsa:headset")), None);
        assert_eq!(panel.selected_devices(), Err(SelectionError::NoOutput));
        assert!(!panel.can_start());

        // Empty lists after a failed or empty enumeration.
        panel.set_lists(Vec::new(), Vec::new());
        panel.select(
            Some(DeviceId::new("alsa:headset")),
            Some(DeviceId::new("alsa:headset")),
        );
        assert!(!panel.can_start());
    }

    #[test]
    fn a_selection_that_disappeared_stays_selected_but_is_reported() {
        let mut panel = DevicePanel::new();
        panel.set_lists(vec![headset()], vec![headset()]);
        panel.select(
            Some(DeviceId::new("alsa:headset")),
            Some(DeviceId::new("alsa:headset")),
        );
        assert!(panel.can_start());

        // The headset is unplugged: the new list does not have it.
        panel.set_lists(vec![hid()], vec![hid()]);
        assert_eq!(
            panel.selected_input(),
            Some(&DeviceId::new("alsa:headset")),
            "the selection is not replaced by another device"
        );
        assert_eq!(
            panel.selected_devices(),
            Err(SelectionError::InputMissing(DeviceId::new("alsa:headset")))
        );

        // It comes back with the next refresh.
        panel.set_lists(vec![headset(), hid()], vec![headset(), hid()]);
        assert!(panel.can_start());
    }

    #[test]
    fn select_first_listed_picks_from_the_candidates_of_each_side() {
        let mut panel = DevicePanel::new();
        panel.set_lists(vec![headset(), hid()], vec![headset()]);
        let configured = DeviceId::new("alsa:headset");
        let gone = DeviceId::new("alsa:gone");
        panel.select_first_listed(&[Some(&gone), Some(&configured)], &[Some(&configured)]);
        assert_eq!(panel.selected_input(), Some(&configured));
        assert_eq!(panel.selected_output(), Some(&configured));

        panel.select_first_listed(&[Some(&gone)], &[None]);
        assert_eq!(panel.selected_input(), None);
        assert_eq!(panel.selected_output(), None);
    }

    #[test]
    fn labels_show_the_device_or_say_why_not() {
        let list = [headset()];
        assert_eq!(
            device_label(&list, Some(&DeviceId::new("alsa:headset"))),
            "Logi USB Headset (alsa:headset)"
        );
        assert_eq!(device_label(&list, None), "（選ばれていません）");
        assert_eq!(
            device_label(&list, Some(&DeviceId::new("alsa:gone"))),
            "（見つかりません） alsa:gone"
        );
    }

    #[test]
    fn a_button_wins_and_a_changed_selection_restarts_only_while_running() {
        use PanelAction::*;
        assert_eq!(decide(Some(Refresh), true, true), Some(Refresh));
        assert_eq!(decide(Some(Stop), false, true), Some(Stop));
        assert_eq!(decide(None, true, true), Some(Restart));
        assert_eq!(decide(None, true, false), None, "stopped: only select");
        assert_eq!(decide(None, false, true), None);
        assert_eq!(decide(None, false, false), None);
    }

    #[test]
    fn selection_errors_have_a_japanese_line() {
        assert_eq!(
            SelectionError::NoInput.message_ja(),
            "入力デバイスが選ばれていません"
        );
        assert_eq!(
            SelectionError::OutputMissing(DeviceId::new("alsa:gone")).message_ja(),
            "選んだ出力デバイスが見つかりません（alsa:gone）。もう一度「開始」を押してください"
        );
    }
}
