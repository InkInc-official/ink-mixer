//! The Sound Pad: eight pads in four columns and two rows, the rule for a
//! sound that is played again, and the button that stops them all.
//!
//! This only draws. What a pad is and does is decided by [`PadBank`]
//! (`sound_state.rs`), which has no egui in it. The window does not hold the
//! SE player either: it lends it for the frame (`Option<&mut dyn SoundSlots>`,
//! `None` while the audio is stopped).
//!
//! Drawing only reads the bank. What the user does (a press, a choice, the
//! volume, the rule, stop all) is collected as [`PadAction`]s and applied
//! after the drawing, one by one, so that the player is borrowed in one place
//! and not inside the closures of the layout.
//!
//! The pads use the width they are given: `ui.columns` cuts it in four. The
//! window is at least 720 points wide, which makes a pad about 150 points
//! wide inside its frame. Nothing here asks for a repaint: a press is an
//! event of its own, the results of the loader wake the window, and the meter
//! of the SE channel moves while a sound plays.

use eframe::egui;
use ink_core::graph::{SE_VOLUME_RANGE, SePolicy};
use ink_core::{AssetId, AssetKind};

use super::bgm_strip::shorten;
use super::strip::format_gain_db;
use crate::library_view::{Choice, LibraryView, Resolved};
use crate::se_loader::SeLoader;
use crate::sound_state::{
    PAD_COUNT, PadBank, PadState, Press, SoundSlots, format_clock, unusable_hint_ja,
};

/// The pads in a row.
const COLUMNS: usize = 4;
/// The rows of pads.
const ROWS: usize = PAD_COUNT / COLUMNS;
const _: () = assert!(
    PAD_COUNT.is_multiple_of(COLUMNS),
    "the pads fill whole rows"
);

/// The height of the big button of a pad.
const PAD_BUTTON_HEIGHT: f32 = 44.0;
/// What the value next to a slider takes from its width (`+0.0 dB`).
const SLIDER_VALUE_WIDTH: f32 = 80.0;
/// The narrowest a slider is drawn.
const MIN_SLIDER_WIDTH: f32 = 40.0;
/// The longest name in the list of sound effects, in characters.
const LIST_NAME_CHARS: usize = 32;

const WARNING: egui::Color32 = egui::Color32::from_rgb(230, 190, 60);
const ERROR: egui::Color32 = egui::Color32::from_rgb(230, 90, 90);

/// The three rules for a sound that is played again, in the order they are
/// listed.
const POLICIES: [SePolicy; 3] = [SePolicy::Overlap, SePolicy::Restart, SePolicy::Ignore];

/// The name of a rule, for the list.
fn policy_label(policy: SePolicy) -> &'static str {
    match policy {
        SePolicy::Overlap => "重ねて鳴らす",
        SePolicy::Restart => "止めて頭から",
        SePolicy::Ignore => "鳴っていれば無視",
    }
}

/// One sentence that says what a rule does.
fn policy_help(policy: SePolicy) -> &'static str {
    match policy {
        SePolicy::Overlap => "押すたびに、鳴っている音の上に重ねてもう一度鳴らします。",
        SePolicy::Restart => "鳴っている音を止めて、頭から鳴らし直します。",
        SePolicy::Ignore => "鳴っている間に押しても、何もしません。",
    }
}

/// The text on the big button: the name of the sound, or its ID while the name
/// is not known (the audio is stopped and the library does not have it), or
/// "未設定".
fn pad_title(name: Option<&str>, asset: Option<&AssetId>) -> String {
    match (name, asset) {
        (Some(name), _) => name.to_string(),
        (None, Some(id)) => format!("（{id}）"),
        (None, None) => "未設定".to_string(),
    }
}

/// Whether the big button can be pressed, and if not, why (for the tooltip of
/// a disabled button).
fn pad_button(state: &PadState, audio_running: bool, has_asset: bool) -> (bool, &'static str) {
    if !audio_running {
        return (false, "音声が止まっています");
    }
    match state {
        PadState::Ready { .. } => (true, ""),
        PadState::Loading => (false, "読み込み中です"),
        PadState::Failed(_) => (false, "読み込めませんでした"),
        PadState::Empty if has_asset => (false, "まだ読み込まれていません"),
        PadState::Empty => (false, "SE を選んでください"),
    }
}

/// How a note under a pad is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoteKind {
    /// Small and weak.
    Quiet,
    /// Red.
    Error,
}

/// The line under a pad: the length, that it is being read, or why it failed.
fn pad_note(state: &PadState, audio_running: bool, has_asset: bool) -> Option<(NoteKind, String)> {
    match state {
        PadState::Ready { length } => Some((NoteKind::Quiet, format_clock(*length))),
        PadState::Loading => Some((NoteKind::Quiet, "読み込み中…".to_string())),
        PadState::Failed(line) => Some((NoteKind::Error, line.clone())),
        PadState::Empty if has_asset && !audio_running => {
            Some((NoteKind::Quiet, "音声を開始すると読み込みます".to_string()))
        }
        PadState::Empty => None,
    }
}

/// The text of the closed list of a pad: the name of the chosen sound, with a
/// mark when it cannot be used, or what the saved ID is now.
fn selected_text(selected: Option<(&AssetId, &Resolved)>) -> String {
    match selected {
        None => "SE を選ぶ".to_string(),
        Some((_, Resolved::Found { name, .. })) => name.clone(),
        Some((_, Resolved::Missing { name })) => format!("{name}（ファイルが無い）"),
        Some((_, Resolved::Unreadable { name })) => format!("{name}（読めない）"),
        Some((id, Resolved::Unknown)) => format!("（見つかりません） {id}"),
        Some((id, Resolved::WrongKind)) => format!("（SE ではありません） {id}"),
    }
}

/// One line of the list of sound effects: the name (cut when long), the mark
/// of one that cannot be used, and the length.
fn list_item_text(choice: &Choice) -> String {
    let menu_label = choice.menu_label();
    let mark = menu_label.strip_prefix(&choice.label).unwrap_or_default();
    let mut text = format!("{}{mark}", shorten(&choice.label, LIST_NAME_CHARS));
    if let Some(length) = choice.length {
        text.push_str(&format!("　{}", format_clock(Some(length))));
    }
    text
}

/// What the user did in this frame.
#[derive(Debug, Clone, PartialEq)]
enum PadAction {
    /// The big button of a pad.
    Press(usize),
    /// A sound (or none) was chosen for a pad.
    Choose(usize, Option<AssetId>),
    /// The volume of a pad, in dB.
    Gain(usize, f32),
    /// The rule for a sound that is played again.
    Policy(SePolicy),
    /// The button that stops every sound.
    StopAll,
}

/// The Sound Pad of the window.
#[derive(Debug)]
pub struct SoundPad {
    /// The note about the last press that could not be played. It goes with
    /// the next press that plays, or when a sound is chosen; it has no timer,
    /// so it asks for no repaint.
    notice: Option<&'static str>,
    /// Where each pad was drawn, for the tests.
    #[cfg(test)]
    cells: [egui::Rect; PAD_COUNT],
}

impl SoundPad {
    /// Creates the Sound Pad.
    pub fn new() -> Self {
        Self {
            notice: None,
            #[cfg(test)]
            cells: [egui::Rect::NOTHING; PAD_COUNT],
        }
    }

    /// Draws the Sound Pad and does what the user asked for. `se` is the
    /// player of the session, or `None` while the audio is stopped; `loader`
    /// reads the sounds that are chosen.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        bank: &mut PadBank,
        view: &LibraryView,
        se: Option<&mut dyn SoundSlots>,
        loader: &SeLoader,
    ) {
        let audio_running = se.is_some();
        let mut actions = Vec::new();
        {
            // From here the bank is only read; the actions change it below.
            let bank: &PadBank = bank;
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.vertical(|ui| {
                    header(ui, bank, audio_running, &mut actions);
                    if let Some(notice) = self.notice {
                        ui.add(egui::Label::new(egui::RichText::new(notice).color(WARNING)).wrap());
                    }
                    for row in 0..ROWS {
                        ui.columns(COLUMNS, |columns| {
                            for (column, ui) in columns.iter_mut().enumerate() {
                                let slot = row * COLUMNS + column;
                                let cell =
                                    pad_cell(ui, slot, bank, view, audio_running, &mut actions);
                                #[cfg(test)]
                                {
                                    self.cells[slot] = cell;
                                }
                                let _ = cell;
                            }
                        });
                    }
                });
            });
        }
        self.apply(actions, bank, view, se, loader);
    }

    /// Does what the user asked for, in the order it happened.
    ///
    /// `se` is `Option<&mut dyn SoundSlots>`, which is not `Copy`: passing it
    /// on would move it, and the next action would find it gone. And
    /// `se.as_deref_mut()` does not help: it keeps the long lifetime of the
    /// trait object, while the callee wants `&'a mut (dyn SoundSlots + 'a)`,
    /// and a `&mut` does not let that lifetime shrink on its own. So each use
    /// takes a fresh, short borrow with [`reborrow`], which leaves `se`
    /// itself where it is and lets a frame hold any number of actions.
    fn apply(
        &mut self,
        actions: Vec<PadAction>,
        bank: &mut PadBank,
        view: &LibraryView,
        mut se: Option<&mut dyn SoundSlots>,
        loader: &SeLoader,
    ) {
        for action in actions {
            match action {
                PadAction::Press(slot) => {
                    if let Some(se) = reborrow(&mut se) {
                        match bank.press(slot, se) {
                            Press::Played => self.notice = None,
                            Press::NotReady => {}
                            other => self.notice = other.notice_ja(),
                        }
                    }
                }
                PadAction::Choose(slot, id) => {
                    bank.assign(slot, id, view, reborrow(&mut se), loader);
                    self.notice = None;
                }
                PadAction::Gain(slot, db) => bank.set_gain_db(slot, db),
                PadAction::Policy(policy) => bank.set_policy(policy),
                PadAction::StopAll => {
                    if let Some(se) = reborrow(&mut se)
                        && !bank.stop_all(se)
                    {
                        self.notice = Press::Busy.notice_ja();
                    }
                }
            }
        }
    }

    /// The note that is shown, for the tests.
    #[cfg(test)]
    fn notice(&self) -> Option<&'static str> {
        self.notice
    }
}

/// A short borrow of the player, for one use. The coercion in `Some(..)` is
/// what shrinks the lifetime of the trait object (see [`SoundPad::apply`]).
pub(super) fn reborrow<'a>(
    se: &'a mut Option<&mut dyn SoundSlots>,
) -> Option<&'a mut dyn SoundSlots> {
    match se {
        Some(se) => Some(&mut **se),
        None => None,
    }
}

/// The title, the rule for a sound that is played again, and stop all.
fn header(ui: &mut egui::Ui, bank: &PadBank, audio_running: bool, actions: &mut Vec<PadAction>) {
    ui.horizontal(|ui| {
        ui.strong("サウンドパッド");
        ui.label("重ねの扱い");
        let current = bank.policy();
        egui::ComboBox::from_id_salt("pad_policy")
            .selected_text(policy_label(current))
            .show_ui(ui, |ui| {
                for policy in POLICIES {
                    let item = ui
                        .selectable_label(policy == current, policy_label(policy))
                        .on_hover_text(policy_help(policy));
                    if item.clicked() && policy != current {
                        actions.push(PadAction::Policy(policy));
                    }
                }
            })
            .response
            .on_hover_text(policy_help(current));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let stop = ui.add_enabled(audio_running, egui::Button::new("全部止める"));
            if stop.clicked() {
                actions.push(PadAction::StopAll);
            }
            stop.on_disabled_hover_text("音声が止まっています");
        });
    });
}

/// One pad: the big button, the list, the volume, and the note. Returns where
/// it was drawn.
fn pad_cell(
    ui: &mut egui::Ui,
    slot: usize,
    bank: &PadBank,
    view: &LibraryView,
    audio_running: bool,
    actions: &mut Vec<PadAction>,
) -> egui::Rect {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.vertical(|ui| {
            let state = bank.state(slot);
            let asset = bank.asset(slot);
            let name = known_name(bank, view, slot);

            let (enabled, reason) = pad_button(state, audio_running, asset.is_some());
            let title = pad_title(name.as_deref(), asset);
            let button = egui::Button::new(egui::RichText::new(title).strong())
                .truncate()
                .min_size(egui::vec2(ui.available_width(), PAD_BUTTON_HEIGHT));
            let button = ui.add_enabled(enabled, button);
            if button.clicked() {
                actions.push(PadAction::Press(slot));
            }
            button.on_disabled_hover_text(reason);

            sound_list(ui, slot, bank, view, actions);
            volume(ui, slot, bank, asset.is_some(), actions);

            match pad_note(state, audio_running, asset.is_some()) {
                Some((NoteKind::Quiet, text)) => {
                    ui.label(egui::RichText::new(text).small().weak());
                }
                Some((NoteKind::Error, text)) => {
                    // A wrapped line made the pad half a point wider than
                    // its column (and so pushed the pads of the next
                    // columns); one point less to wrap in keeps it inside.
                    let width = (ui.available_width() - 1.0).max(0.0);
                    let response = ui
                        .scope(|ui| {
                            ui.set_max_width(width);
                            ui.add(
                                egui::Label::new(egui::RichText::new(text).small().color(ERROR))
                                    .wrap(),
                            )
                        })
                        .inner;
                    if let Some(detail) = bank.detail(slot) {
                        response.on_hover_text(detail);
                    }
                }
                None => {}
            }
        });
    })
    .response
    .rect
}

/// The name of the sound of the pad, if it is known: the bank has it once the
/// sound was looked up in a session; before that the library may know it.
fn known_name(bank: &PadBank, view: &LibraryView, slot: usize) -> Option<String> {
    if let Some(name) = bank.name(slot) {
        return Some(name.to_string());
    }
    match view.resolve(bank.asset(slot)?, AssetKind::Se) {
        Resolved::Found { name, .. }
        | Resolved::Missing { name }
        | Resolved::Unreadable { name } => Some(name),
        Resolved::Unknown | Resolved::WrongKind => None,
    }
}

/// The list to choose the sound of a pad from.
fn sound_list(
    ui: &mut egui::Ui,
    slot: usize,
    bank: &PadBank,
    view: &LibraryView,
    actions: &mut Vec<PadAction>,
) {
    let library_ready = view.library().is_some();
    let current = bank.asset(slot).copied();
    let resolved = current.map(|id| (id, view.resolve(&id, AssetKind::Se)));
    let text = selected_text(resolved.as_ref().map(|(id, resolved)| (id, resolved)));

    let response = ui
        .add_enabled_ui(library_ready, |ui| {
            egui::ComboBox::from_id_salt(("pad_sound", slot))
                .width(ui.available_width())
                .truncate()
                .selected_text(text.clone())
                .show_ui(ui, |ui| {
                    if ui.selectable_label(current.is_none(), "なし").clicked() && current.is_some()
                    {
                        actions.push(PadAction::Choose(slot, None));
                    }
                    for choice in view.choices(AssetKind::Se) {
                        let chosen = current == Some(choice.id);
                        let item = ui
                            .add_enabled_ui(choice.selectable(), |ui| {
                                ui.selectable_label(chosen, list_item_text(&choice))
                            })
                            .inner;
                        // A disabled widget shows only the disabled tooltip.
                        let item = match unusable_hint_ja(&choice.label, choice.state) {
                            Some(hint) => item.on_disabled_hover_text(hint),
                            None => item.on_hover_text(choice.label.clone()),
                        };
                        if item.clicked() && !chosen {
                            actions.push(PadAction::Choose(slot, Some(choice.id)));
                        }
                    }
                })
                .response
        })
        .inner;
    if library_ready {
        response.on_hover_text(text);
    } else {
        response.on_disabled_hover_text("ライブラリが使えないため、SE を選べません");
    }
}

/// The volume of a pad. The place stays when no sound is chosen, so the pads
/// do not change their height.
fn volume(
    ui: &mut egui::Ui,
    slot: usize,
    bank: &PadBank,
    has_asset: bool,
    actions: &mut Vec<PadAction>,
) {
    let mut db = bank.gain_db(slot);
    ui.spacing_mut().slider_width =
        (ui.available_width() - SLIDER_VALUE_WIDTH).max(MIN_SLIDER_WIDTH);
    let slider = egui::Slider::new(&mut db, SE_VOLUME_RANGE.min..=SE_VOLUME_RANGE.max)
        .step_by(0.5)
        .custom_formatter(|value, _| format_gain_db(value as f32))
        .custom_parser(parse_db);
    let response = ui.add_enabled(has_asset, slider);
    if response.changed() {
        actions.push(PadAction::Gain(slot, db));
    }
    response.on_disabled_hover_text("SE を選ぶと、音量を変えられます");
}

/// Reads a typed volume: `-3`, `-3.0` or `-3.0 dB`.
fn parse_db(text: &str) -> Option<f64> {
    text.trim()
        .trim_end_matches("dB")
        .trim()
        .parse::<f64>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use ink_core::AssetState;
    use ink_core::graph::{SeError, SoundData};

    use super::super::fonts;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// How long a test waits for the loader.
    const TIMEOUT: Duration = Duration::from_secs(10);
    const RATE: u32 = 48_000;

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    // -- the pure parts -----------------------------------------------------------

    #[test]
    fn the_title_is_the_name_then_the_id_then_not_set() {
        let se = id("se-0007");
        assert_eq!(pad_title(Some("拍手"), Some(&se)), "拍手");
        assert_eq!(pad_title(Some("拍手"), None), "拍手");
        assert_eq!(pad_title(None, Some(&se)), "（se-0007）");
        assert_eq!(pad_title(None, None), "未設定");
    }

    #[test]
    fn only_a_ready_pad_can_be_pressed_and_the_reason_is_told_otherwise() {
        let ready = PadState::Ready { length: None };
        let failed = PadState::Failed("x".to_string());
        let table = [
            // (state, audio running, has asset) -> (enabled, reason)
            ((&ready, true, true), (true, "")),
            ((&PadState::Loading, true, true), (false, "読み込み中です")),
            ((&failed, true, true), (false, "読み込めませんでした")),
            (
                (&PadState::Empty, true, true),
                (false, "まだ読み込まれていません"),
            ),
            (
                (&PadState::Empty, true, false),
                (false, "SE を選んでください"),
            ),
            // The audio first: nothing sounds without it.
            ((&ready, false, true), (false, "音声が止まっています")),
            (
                (&PadState::Empty, false, false),
                (false, "音声が止まっています"),
            ),
            (
                (&PadState::Loading, false, true),
                (false, "音声が止まっています"),
            ),
        ];
        for ((state, audio, has_asset), expected) in table {
            assert_eq!(
                pad_button(state, audio, has_asset),
                expected,
                "{state:?} {audio} {has_asset}"
            );
        }
    }

    #[test]
    fn the_note_under_a_pad_is_the_length_the_reading_or_the_failure() {
        let ready = PadState::Ready {
            length: Some(Duration::from_secs(125)),
        };
        let unknown = PadState::Ready { length: None };
        let failed = PadState::Failed("「拍手」のファイルが見つかりません".to_string());
        assert_eq!(
            pad_note(&ready, true, true),
            Some((NoteKind::Quiet, "2:05".to_string()))
        );
        assert_eq!(
            pad_note(&unknown, true, true),
            Some((NoteKind::Quiet, "--:--".to_string()))
        );
        assert_eq!(
            pad_note(&PadState::Loading, true, true),
            Some((NoteKind::Quiet, "読み込み中…".to_string()))
        );
        assert_eq!(
            pad_note(&failed, true, true),
            Some((
                NoteKind::Error,
                "「拍手」のファイルが見つかりません".to_string()
            ))
        );
        assert_eq!(
            pad_note(&PadState::Empty, false, true),
            Some((NoteKind::Quiet, "音声を開始すると読み込みます".to_string()))
        );
        assert_eq!(pad_note(&PadState::Empty, true, true), None);
        assert_eq!(pad_note(&PadState::Empty, true, false), None);
        assert_eq!(pad_note(&PadState::Empty, false, false), None);
    }

    #[test]
    fn each_rule_has_its_own_label_and_one_sentence_of_help() {
        let labels: Vec<_> = POLICIES.iter().map(|p| policy_label(*p)).collect();
        assert_eq!(labels, ["重ねて鳴らす", "止めて頭から", "鳴っていれば無視"]);
        let helps: Vec<_> = POLICIES.iter().map(|p| policy_help(*p)).collect();
        for help in &helps {
            assert!(help.ends_with('。'), "{help}");
            assert_eq!(help.matches('。').count(), 1, "one sentence: {help}");
        }
        assert_eq!(
            helps.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "the three are not the same"
        );
        // Every rule is in the list.
        for policy in [SePolicy::Overlap, SePolicy::Restart, SePolicy::Ignore] {
            assert!(POLICIES.contains(&policy));
        }
    }

    #[test]
    fn the_closed_list_shows_the_name_or_what_the_saved_id_is() {
        let se = id("se-0001");
        let found = Resolved::Found {
            path: PathBuf::from("x"),
            name: "拍手".to_string(),
            length: None,
        };
        let missing = Resolved::Missing {
            name: "消えた音".to_string(),
        };
        let unreadable = Resolved::Unreadable {
            name: "読めない音".to_string(),
        };
        assert_eq!(selected_text(None), "SE を選ぶ");
        assert_eq!(selected_text(Some((&se, &found))), "拍手");
        assert_eq!(
            selected_text(Some((&se, &missing))),
            "消えた音（ファイルが無い）"
        );
        assert_eq!(
            selected_text(Some((&se, &unreadable))),
            "読めない音（読めない）"
        );
        assert_eq!(
            selected_text(Some((&se, &Resolved::Unknown))),
            "（見つかりません） se-0001"
        );
        assert_eq!(
            selected_text(Some((&se, &Resolved::WrongKind))),
            "（SE ではありません） se-0001"
        );
    }

    fn choice(label: &str, length: Option<Duration>, state: AssetState) -> Choice {
        Choice {
            id: id("se-0001"),
            label: label.to_string(),
            length,
            state,
        }
    }

    #[test]
    fn a_line_of_the_list_has_the_name_the_mark_and_the_length() {
        let length = Some(Duration::from_secs(3));
        assert_eq!(
            list_item_text(&choice("拍手", length, AssetState::Available)),
            "拍手　0:03"
        );
        assert_eq!(
            list_item_text(&choice("消えた音", None, AssetState::Missing)),
            "消えた音（ファイルが無い）"
        );
        let long = "あ".repeat(100);
        let text = list_item_text(&choice(&long, None, AssetState::Unreadable));
        assert!(text.ends_with("（読めない）"), "{text}");
        assert!(text.contains('…'));
    }

    #[test]
    fn a_typed_volume_may_have_the_unit_or_not() {
        assert_eq!(parse_db("-3"), Some(-3.0));
        assert_eq!(parse_db(" -3.5 dB "), Some(-3.5));
        assert_eq!(parse_db("+2.0dB"), Some(2.0));
        assert_eq!(parse_db("loud"), None);
    }

    // -- a fake player, a library and a bank ------------------------------------

    /// What the Sound Pad asked of the player.
    #[derive(Default)]
    struct FakeSlots {
        played: Vec<(usize, f32, SePolicy)>,
        cleared: Vec<usize>,
        sounds: Vec<usize>,
        stopped: usize,
        /// The next presses fail with `Busy`.
        busy: bool,
        /// The next presses fail with another error.
        refuse: bool,
        /// Stop all fails.
        stop_fails: bool,
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
        fn play(&mut self, slot: usize, gain: f32, policy: SePolicy) -> Result<(), SeError> {
            if self.busy {
                return Err(SeError::Busy);
            }
            if self.refuse {
                return Err(SeError::EmptySound);
            }
            self.played.push((slot, gain, policy));
            Ok(())
        }
        fn stop_all(&mut self) -> Result<(), SeError> {
            if self.stop_fails {
                return Err(SeError::QueueFull);
            }
            self.stopped += 1;
            Ok(())
        }
        fn collect(&mut self) -> usize {
            0
        }
    }

    /// A directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ink-mixer-sound-pad-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
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
        fs::write(path, bytes).unwrap();
    }

    fn index_entry(id: &str, file: &str, read: bool) -> String {
        let info = if read {
            "\n[asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 1\nframes = 100\n"
        } else {
            ""
        };
        format!("[[asset]]\nid = \"{id}\"\nkind = \"se\"\npath = \"{file}\"\n{info}\n")
    }

    /// A library of sound effects, with Japanese names:
    ///
    /// - `se-0001` 拍手 テスト: a WAV that can be read
    /// - `se-0002`: the file is gone
    /// - `se-0003`: the library could not read it
    /// - `se-0004`: a very long name; a WAV that can be read
    /// - `se-0005`: the library has read it once, but the file is not a sound
    fn library(name: &str) -> LibraryView {
        let root = test_dir(name);
        let folder = root.join("sounds").join("se");
        fs::create_dir_all(&folder).unwrap();
        let long = format!("{}.wav", "とても長い効果音の名前".repeat(4));
        write_wav(&folder.join("拍手 テスト.wav"), 480);
        write_wav(&folder.join(&long), 480);
        fs::write(folder.join("読めない音.wav"), "x").unwrap();
        fs::write(folder.join("壊れた音.wav"), "x").unwrap();
        let index = format!(
            "version = 1\nnext_id = 100\n\n{}{}{}{}{}",
            index_entry("se-0001", "拍手 テスト.wav", true),
            index_entry("se-0002", "消えた音.wav", true),
            index_entry("se-0003", "読めない音.wav", false),
            index_entry("se-0004", &long, true),
            index_entry("se-0005", "壊れた音.wav", true),
        );
        fs::write(root.join("library.toml"), index).unwrap();
        let view = LibraryView::open(Some(&root));
        assert!(view.library().is_some());
        view
    }

    /// A loader that tells the test when a result is ready, so the test waits
    /// for it and does not sleep.
    fn loader() -> (SeLoader, mpsc::Receiver<()>) {
        let (sender, receiver) = mpsc::channel();
        let loader = SeLoader::start(Box::new(move || {
            let _ = sender.send(());
        }))
        .unwrap();
        (loader, receiver)
    }

    /// Takes the results of the loader until no pad is being read.
    fn settle(
        bank: &mut PadBank,
        loader: &SeLoader,
        wake: &mpsc::Receiver<()>,
        slots: &mut FakeSlots,
    ) {
        let start = Instant::now();
        while bank.loading() {
            let left = TIMEOUT.saturating_sub(start.elapsed());
            wake.recv_timeout(left).expect("the loader answered");
            bank.poll(loader, Some(slots));
        }
    }

    /// A bank with a pad in every state, all read:
    /// 0 ready, 1 unknown ID, 2 file gone, 3 unreadable, 4 not a sound,
    /// 5 ready (very long name, quiet), 6 and 7 empty.
    fn full_bank(view: &LibraryView) -> (PadBank, FakeSlots, SeLoader, mpsc::Receiver<()>) {
        use ink_core::PadSetting;
        let pad = |slot, asset: &str, gain_db| PadSetting {
            slot,
            asset: id(asset),
            gain_db,
        };
        let mut bank = PadBank::new(
            &[
                pad(0, "se-0001", 0.0),
                pad(1, "se-0099", 0.0),
                pad(2, "se-0002", 0.0),
                pad(3, "se-0003", 0.0),
                pad(4, "se-0005", 0.0),
                pad(5, "se-0004", -3.0),
            ],
            SePolicy::Overlap,
        );
        let mut slots = FakeSlots::default();
        let (loader, wake) = loader();
        bank.on_session_started(view, &mut slots, &loader);
        settle(&mut bank, &loader, &wake, &mut slots);
        (bank, slots, loader, wake)
    }

    #[test]
    fn the_bank_of_the_tests_has_a_pad_in_every_state() {
        let view = library("states");
        let (bank, _slots, _loader, _wake) = full_bank(&view);
        assert!(matches!(bank.state(0), PadState::Ready { .. }));
        for slot in 1..=4 {
            assert!(matches!(bank.state(slot), PadState::Failed(_)), "{slot}");
        }
        assert!(matches!(bank.state(5), PadState::Ready { .. }));
        assert!(matches!(bank.state(6), PadState::Empty));
        assert!(matches!(bank.state(7), PadState::Empty));
    }

    // -- what the user does -----------------------------------------------------

    #[test]
    fn a_press_plays_a_ready_pad_at_its_volume_and_the_rule_of_the_bank() {
        let view = library("press");
        let (mut bank, mut slots, loader, _wake) = full_bank(&view);
        let mut pad = SoundPad::new();
        bank.set_policy(SePolicy::Restart);
        bank.set_gain_db(5, -6.0);

        let actions = vec![
            PadAction::Press(0),
            PadAction::Press(5),
            PadAction::Press(6),
        ];
        pad.apply(actions, &mut bank, &view, Some(&mut slots), &loader);

        // The pad 6 is empty, so it plays nothing; the two others play.
        assert_eq!(slots.played.len(), 2, "{:?}", slots.played);
        assert_eq!(slots.played[0].0, 0);
        assert_eq!(slots.played[0].2, SePolicy::Restart);
        assert_eq!(slots.played[1].0, 5);
        assert!(
            slots.played[1].1 < 0.6 && slots.played[1].1 > 0.4,
            "-6 dB is about 0.5"
        );
        assert_eq!(pad.notice(), None);
    }

    #[test]
    fn a_press_that_the_player_cannot_take_says_so_and_the_next_press_that_plays_clears_it() {
        let view = library("busy");
        let (mut bank, mut slots, loader, _wake) = full_bank(&view);
        let mut pad = SoundPad::new();

        slots.busy = true;
        pad.apply(
            vec![PadAction::Press(0)],
            &mut bank,
            &view,
            Some(&mut slots),
            &loader,
        );
        assert_eq!(pad.notice(), Press::Busy.notice_ja());
        assert!(pad.notice().is_some());

        // A press of a pad that cannot play changes nothing.
        pad.apply(
            vec![PadAction::Press(6)],
            &mut bank,
            &view,
            Some(&mut slots),
            &loader,
        );
        assert_eq!(pad.notice(), Press::Busy.notice_ja());

        slots.busy = false;
        slots.refuse = true;
        pad.apply(
            vec![PadAction::Press(0)],
            &mut bank,
            &view,
            Some(&mut slots),
            &loader,
        );
        assert_eq!(pad.notice(), Press::Rejected.notice_ja());

        slots.refuse = false;
        pad.apply(
            vec![PadAction::Press(0)],
            &mut bank,
            &view,
            Some(&mut slots),
            &loader,
        );
        assert_eq!(pad.notice(), None);
        assert_eq!(slots.played.len(), 1);
    }

    #[test]
    fn without_a_player_nothing_is_pressed_and_nothing_is_lost() {
        let view = library("no-player");
        let (mut bank, _slots, loader, _wake) = full_bank(&view);
        let mut pad = SoundPad::new();
        let saved = bank.saved();
        let actions = vec![PadAction::Press(0), PadAction::StopAll];
        pad.apply(actions, &mut bank, &view, None, &loader);
        assert_eq!(pad.notice(), None);
        assert_eq!(bank.saved(), saved);
    }

    #[test]
    fn choosing_a_sound_empties_the_pad_then_reads_it_and_clears_the_note() {
        let view = library("choose");
        let (mut bank, mut slots, loader, wake) = full_bank(&view);
        let mut pad = SoundPad::new();
        slots.busy = true;
        pad.apply(
            vec![PadAction::Press(0)],
            &mut bank,
            &view,
            Some(&mut slots),
            &loader,
        );
        assert!(pad.notice().is_some());

        let actions = vec![PadAction::Choose(6, Some(id("se-0001")))];
        pad.apply(actions, &mut bank, &view, Some(&mut slots), &loader);
        assert_eq!(pad.notice(), None, "a choice clears the note");
        assert_eq!(
            slots.cleared,
            vec![6],
            "the old sound of the pad goes first"
        );
        assert!(matches!(bank.state(6), PadState::Loading));
        assert_eq!(bank.asset(6), Some(&id("se-0001")));

        settle(&mut bank, &loader, &wake, &mut slots);
        assert!(matches!(bank.state(6), PadState::Ready { .. }));

        // No sound at all.
        pad.apply(
            vec![PadAction::Choose(6, None)],
            &mut bank,
            &view,
            Some(&mut slots),
            &loader,
        );
        assert_eq!(bank.asset(6), None);
        assert!(matches!(bank.state(6), PadState::Empty));
    }

    #[test]
    fn the_volume_the_rule_and_stop_all_go_to_the_bank_and_the_player() {
        let view = library("rest");
        let (mut bank, mut slots, loader, _wake) = full_bank(&view);
        let mut pad = SoundPad::new();
        let actions = vec![
            PadAction::Gain(0, -12.0),
            PadAction::Gain(0, 99.0),
            PadAction::Policy(SePolicy::Ignore),
            PadAction::StopAll,
        ];
        pad.apply(actions, &mut bank, &view, Some(&mut slots), &loader);
        assert_eq!(bank.gain_db(0), SE_VOLUME_RANGE.max, "kept in the range");
        assert_eq!(bank.policy(), SePolicy::Ignore);
        assert_eq!(slots.stopped, 1);
        assert_eq!(pad.notice(), None);

        slots.stop_fails = true;
        pad.apply(
            vec![PadAction::StopAll],
            &mut bank,
            &view,
            Some(&mut slots),
            &loader,
        );
        assert_eq!(pad.notice(), Press::Busy.notice_ja());
    }

    // -- drawing without a window -------------------------------------------------

    /// A context with the bundled font, as the window has it.
    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        fonts::install(&ctx);
        ctx
    }

    /// Draws `draw` in a frame of the given width and returns the size it
    /// took.
    fn frame(ctx: &egui::Context, width: f32, mut draw: impl FnMut(&mut egui::Ui)) -> egui::Vec2 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 900.0),
            )),
            ..Default::default()
        };
        let mut size = egui::Vec2::ZERO;
        ctx.run_ui(input, |ui| {
            draw(ui);
            size = ui.min_rect().size();
        })
        .drop_without_applying_deltas();
        size
    }

    /// The window is never narrower than this (ARCHITECTURE: 720 × 480).
    const NARROWEST_WINDOW: f32 = 720.0;
    /// What the central panel keeps free on each side.
    const PANEL_MARGIN: f32 = 8.0;

    #[test]
    fn at_720_points_the_pads_make_four_columns_that_do_not_overlap_or_stick_out() {
        let ctx = context();
        let view = library("layout");
        let (mut bank, mut slots, loader, _wake) = full_bank(&view);
        let mut pad = SoundPad::new();
        // The central panel has a margin; the Sound Pad gets the rest.
        let usable = NARROWEST_WINDOW - 2.0 * PANEL_MARGIN;
        let size = frame(&ctx, usable, |ui| {
            pad.show(ui, &mut bank, &view, Some(&mut slots), &loader);
        });

        let cells = pad.cells;
        for row in 0..ROWS {
            let cells = &cells[row * COLUMNS..(row + 1) * COLUMNS];
            for pair in cells.windows(2) {
                assert!(
                    pair[0].right() <= pair[1].left() + 0.5,
                    "the pads of a row overlap: {pair:?}"
                );
                assert!(
                    (pair[0].top() - pair[1].top()).abs() < 0.5,
                    "the pads of a row are on one line: {pair:?}"
                );
                assert!(
                    (pair[0].width() - pair[1].width()).abs() < 1.0,
                    "the four pads are as wide as each other: {pair:?}"
                );
            }
            assert!(cells[COLUMNS - 1].right() <= usable + 0.5, "{cells:?}");
        }
        // The second row is below the first.
        assert!(cells[COLUMNS].top() >= cells[0].bottom() - 0.5, "{cells:?}");

        // A pad is about 150 to 175 points wide, and not more than a quarter.
        let width = cells[0].width();
        assert!((140.0..=180.0).contains(&width), "a pad is {width} wide");
        // The whole Sound Pad fills the width and is not higher than it
        // should be for two rows of pads (a single column would be 4× as high).
        assert!(size.x <= usable + 1.0 && size.x >= usable - 4.0, "{size:?}");
        assert!((200.0..=420.0).contains(&size.y), "height {}", size.y);
    }

    #[test]
    fn a_wider_window_makes_wider_pads_and_the_columns_stay_inside_the_width() {
        let ctx = context();
        let view = library("wider");
        let (mut bank, mut slots, loader, _wake) = full_bank(&view);
        let mut pad = SoundPad::new();
        let mut widths = Vec::new();
        for usable in [704.0, 900.0, 1200.0] {
            frame(&ctx, usable, |ui| {
                pad.show(ui, &mut bank, &view, Some(&mut slots), &loader);
            });
            widths.push(pad.cells[0].width());
            for row in 0..ROWS {
                let cells = &pad.cells[row * COLUMNS..(row + 1) * COLUMNS];
                assert!(cells[3].right() <= usable + 0.5, "{usable}: {cells:?}");
                assert!(cells[0].right() <= cells[1].left() + 0.5, "{usable}");
            }
        }
        assert!(widths[0] < widths[1] && widths[1] < widths[2], "{widths:?}");
    }

    #[test]
    fn it_draws_every_state_with_and_without_the_player_and_without_a_library() {
        let ctx = context();
        let view = library("draw");
        let (mut bank, mut slots, loader, _wake) = full_bank(&view);
        let mut pad = SoundPad::new();
        let saved = bank.saved();
        for width in [720.0, 480.0, 200.0] {
            frame(&ctx, width, |ui| {
                pad.show(ui, &mut bank, &view, Some(&mut slots), &loader);
            });
            frame(&ctx, width, |ui| {
                pad.show(ui, &mut bank, &view, None, &loader);
            });
        }
        let no_library = LibraryView::open(None);
        frame(&ctx, 720.0, |ui| {
            pad.show(ui, &mut bank, &no_library, None, &loader);
        });
        // A pad that is being read.
        let mut reading = PadBank::new(&[], SePolicy::Overlap);
        reading.assign(0, Some(id("se-0001")), &view, Some(&mut slots), &loader);
        assert!(matches!(reading.state(0), PadState::Loading));
        frame(&ctx, 720.0, |ui| {
            pad.show(ui, &mut reading, &view, Some(&mut slots), &loader);
        });
        // Drawing alone changes nothing.
        assert_eq!(bank.saved(), saved);
        assert!(slots.played.is_empty());
        assert_eq!(slots.stopped, 0);
    }

    #[test]
    fn the_volume_keeps_its_place_so_a_pad_with_a_sound_is_not_much_higher_than_an_empty_one() {
        use ink_core::PadSetting;
        let ctx = context();
        let view = library("height");
        let mut slots = FakeSlots::default();
        let (loader, wake) = loader();
        let mut pad = SoundPad::new();
        let mut empty = PadBank::new(&[], SePolicy::Overlap);
        let size_empty = frame(&ctx, 704.0, |ui| {
            pad.show(ui, &mut empty, &view, Some(&mut slots), &loader);
        });

        // Every pad has a sound that is read: the volume works, and each pad
        // has the line with the length under it.
        let all: Vec<PadSetting> = (0..PAD_COUNT)
            .map(|slot| PadSetting {
                slot,
                asset: id("se-0001"),
                gain_db: 0.0,
            })
            .collect();
        let mut ready = PadBank::new(&all, SePolicy::Overlap);
        ready.on_session_started(&view, &mut slots, &loader);
        settle(&mut ready, &loader, &wake, &mut slots);
        let size_ready = frame(&ctx, 704.0, |ui| {
            pad.show(ui, &mut ready, &view, Some(&mut slots), &loader);
        });
        let difference = size_ready.y - size_empty.y;
        // Two rows, each with one more small line (about 14 points).
        assert!(
            (0.0..=2.0 * 24.0).contains(&difference),
            "empty {size_empty:?} ready {size_ready:?}"
        );
    }

    #[test]
    fn failures_make_the_pads_higher_only_by_their_lines() {
        let ctx = context();
        let view = library("failures");
        let (mut bank, mut slots, loader, _wake) = full_bank(&view);
        let mut pad = SoundPad::new();
        let size = frame(&ctx, 704.0, |ui| {
            pad.show(ui, &mut bank, &view, Some(&mut slots), &loader);
        });
        // With four failed pads the Sound Pad is still under 400 points, and
        // the pads of a row stay on one line.
        assert!(size.y < 400.0, "{size:?}");
        for row in 0..ROWS {
            let tops: Vec<f32> = (0..COLUMNS)
                .map(|column| pad.cells[row * COLUMNS + column].top())
                .collect();
            assert!(
                tops.windows(2).all(|pair| (pair[0] - pair[1]).abs() < 0.5),
                "{tops:?}"
            );
        }
    }
}
