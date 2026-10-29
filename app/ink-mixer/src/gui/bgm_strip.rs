//! The BGM strip: the channel strip of the song (volume, mute, meter) and the
//! controls next to it (the song, play / pause / stop, the position, the
//! loop, and the messages).
//!
//! This only draws. What the controls do is decided by [`BgmPanel`]
//! (`sound_state.rs`), which has no egui in it. The window does not hold the
//! `Bgm` either: it lends it for the frame (`Option<&Bgm>`, `None` while the
//! audio is stopped).
//!
//! The controls take the width they are given, not less than
//! [`MIN_CONTROLS_WIDTH`], so the window decides the layout: the strip is
//! drawn with [`BgmStrip::show_strip`] and the controls with
//! [`BgmStrip::show_controls`], in one row or in two.
//!
//! Failures of the song reach the panel through `BgmPanel::poll`, which the
//! window calls at the start of every frame (not here: a frame is drawn at
//! least every half second while the audio runs). While a song plays the
//! position needs a repaint of its own when the meter is still (a silent
//! part, or a muted channel): [`position_refresh`].

use std::time::{Duration, Instant};

use eframe::egui;
use ink_core::graph::ChannelControl;
use ink_core::{AssetId, AssetKind};

use super::strip::Strip;
use crate::bgm_worker::Bgm;
use crate::library_view::{Choice, LibraryView, Resolved};
use crate::sound_state::{BgmPanel, Transport, format_clock, unusable_hint_ja};

/// The narrowest the controls are drawn, in points (the text inside).
pub const MIN_CONTROLS_WIDTH: f32 = 180.0;
/// What the frame of a group takes from the width (its margin and stroke).
const GROUP_PADDING: f32 = 16.0;
/// The longest name in the list of songs, in characters. The name of the
/// chosen song is cut by the box itself, to the width it has.
const LIST_NAME_CHARS: usize = 32;
/// How often a playing song needs a repaint to move the position, when the
/// meter does not ask for one. Four a second is far under the frame rate cap,
/// and the text counts whole seconds.
const POSITION_REFRESH: Duration = Duration::from_millis(250);

const WARNING: egui::Color32 = egui::Color32::from_rgb(230, 190, 60);
const ERROR: egui::Color32 = egui::Color32::from_rgb(230, 90, 90);

/// The note under the loop switch (ARCHITECTURE.md §10).
const LOOP_HINT: &str =
    "MP3 はつなぎ目に隙間が出ることがあります。ループさせたい曲は WAV がおすすめです";

/// How long to wait for the next repaint to move the position, or `None` when
/// the position does not move (nothing plays).
pub fn position_refresh(transport: Transport) -> Option<Duration> {
    matches!(transport, Transport::Playing).then_some(POSITION_REFRESH)
}

/// The label of the play button, and whether it can be pressed, for what the
/// song is doing.
fn play_button(transport: Transport) -> (&'static str, bool) {
    match transport {
        Transport::NoSong => ("▶ 再生", false),
        Transport::Stopped => ("▶ 再生", true),
        Transport::Playing => ("一時停止", true),
        Transport::Paused => ("▶ 再開", true),
        Transport::Finished => ("▶ 頭から", true),
        Transport::Failed => ("▶ もう一度", true),
    }
}

/// Whether the stop button can be pressed: there is something to stop or to
/// clear (a song that is at its start, or no song, has nothing).
fn stop_enabled(transport: Transport) -> bool {
    matches!(
        transport,
        Transport::Playing | Transport::Paused | Transport::Finished | Transport::Failed
    )
}

/// Why the buttons cannot be pressed.
fn disabled_reason(transport: Transport, audio_running: bool) -> &'static str {
    if !audio_running {
        "音声が止まっています"
    } else if transport == Transport::NoSong {
        "曲を選んでください"
    } else {
        "今は押せません"
    }
}

/// The position to show. A song that is stopped is at its start; the player
/// keeps the old position for a moment after Stop (the sound fades out
/// first), and that must not be shown.
fn shown_position(transport: Transport, position: Duration) -> Duration {
    match transport {
        Transport::NoSong | Transport::Stopped => Duration::ZERO,
        _ => position,
    }
}

/// How far the song is, from 0 to 1. Unknown or empty length is 0.
fn progress_fraction(position: Duration, length: Option<Duration>) -> f32 {
    match length {
        Some(length) if !length.is_zero() => {
            (position.as_secs_f32() / length.as_secs_f32()).clamp(0.0, 1.0)
        }
        _ => 0.0,
    }
}

/// Cuts `name` to `max` characters, keeping the start and the end and putting
/// `…` between. A name that fits is not changed.
pub(super) fn shorten(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        return name.to_string();
    }
    if max <= 1 {
        return "…".repeat(max);
    }
    let head = max / 2;
    let tail = max - 1 - head;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// The text of the closed list: the name of the chosen song, with a mark when
/// it cannot be used, or what the saved ID is now.
fn selected_text(selected: Option<(&AssetId, &Resolved)>) -> String {
    match selected {
        None => "曲を選ぶ".to_string(),
        Some((_, Resolved::Found { name, .. })) => name.clone(),
        Some((_, Resolved::Missing { name })) => format!("{name}（ファイルが無い）"),
        Some((_, Resolved::Unreadable { name })) => format!("{name}（読めない）"),
        Some((id, Resolved::Unknown)) => format!("（見つかりません） {id}"),
        Some((id, Resolved::WrongKind)) => format!("（曲ではありません） {id}"),
    }
}

/// One line of the list of songs: the name (cut when long), the mark of a
/// song that cannot be used, and the length.
fn list_item_text(choice: &Choice) -> String {
    let menu_label = choice.menu_label();
    let mark = menu_label.strip_prefix(&choice.label).unwrap_or_default();
    let mut text = format!("{}{mark}", shorten(&choice.label, LIST_NAME_CHARS));
    if let Some(length) = choice.length {
        text.push_str(&format!("　{}", format_clock(Some(length))));
    }
    text
}

/// What to tell about a song in the list that cannot be chosen. The words are
/// the ones of the song that is chosen (`sound_state::problem_ja`): they are
/// written once, in [`unusable_hint_ja`].
fn list_item_hint(choice: &Choice) -> Option<String> {
    unusable_hint_ja(&choice.label, choice.state)
}

/// The channel strip of the BGM and the controls of the song.
pub struct BgmStrip {
    strip: Strip,
}

impl BgmStrip {
    /// Creates the strip, titled "BGM", with a mute button.
    pub fn new(now: Instant) -> Self {
        Self {
            strip: Strip::new("BGM", true, now),
        }
    }

    /// Reads the meter of the channel. Call once per frame, before drawing.
    pub fn update_meter(&mut self, control: &ChannelControl, now: Instant) {
        self.strip.update_meter(control, now);
    }

    /// Returns whether the meter is still moving.
    pub fn active(&self) -> bool {
        self.strip.active()
    }

    /// Draws the channel strip alone: the fader, the mute, the meter.
    pub fn show_strip(&mut self, ui: &mut egui::Ui, control: &ChannelControl) {
        self.strip.show(ui, control);
    }

    /// Draws the controls of the song alone, in the width that is left (not
    /// less than [`MIN_CONTROLS_WIDTH`]).
    pub fn show_controls(
        &mut self,
        ui: &mut egui::Ui,
        panel: &mut BgmPanel,
        view: &LibraryView,
        bgm: Option<&Bgm>,
    ) {
        let width = (ui.available_width() - GROUP_PADDING).max(MIN_CONTROLS_WIDTH);
        // The group takes the layout of the row (left to right); the controls
        // go top to bottom.
        ui.group(|ui| {
            ui.set_width(width);
            ui.vertical(|ui| {
                song_list(ui, width, panel, view, bgm);
                ui.add_space(4.0);
                transport_buttons(ui, panel, bgm);
                ui.add_space(4.0);
                position_bar(ui, width, panel, bgm);
                loop_switch(ui, panel, bgm);
                messages(ui, panel);
            });
        });
    }
}

/// The list to choose the song from.
fn song_list(
    ui: &mut egui::Ui,
    width: f32,
    panel: &mut BgmPanel,
    view: &LibraryView,
    bgm: Option<&Bgm>,
) {
    let library_ready = view.library().is_some();
    let resolved = panel
        .selected()
        .map(|id| (*id, view.resolve(id, AssetKind::Bgm)));
    let text = selected_text(resolved.as_ref().map(|(id, resolved)| (id, resolved)));

    // `Some(None)` is the choice of no song.
    let mut chosen: Option<Option<AssetId>> = None;
    let response = ui
        .add_enabled_ui(library_ready, |ui| {
            egui::ComboBox::from_id_salt("bgm_song")
                .width(width)
                .truncate()
                .selected_text(text.clone())
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(panel.selected().is_none(), "なし")
                        .clicked()
                    {
                        chosen = Some(None);
                    }
                    for choice in view.choices(AssetKind::Bgm) {
                        let current = panel.selected() == Some(&choice.id);
                        let item = ui
                            .add_enabled_ui(choice.selectable(), |ui| {
                                ui.selectable_label(current, list_item_text(&choice))
                            })
                            .inner;
                        // A disabled widget shows only the disabled tooltip.
                        let item = match list_item_hint(&choice) {
                            Some(hint) => item.on_disabled_hover_text(hint),
                            None => item.on_hover_text(choice.label.clone()),
                        };
                        if item.clicked() && !current {
                            chosen = Some(Some(choice.id));
                        }
                    }
                })
                .response
        })
        .inner;
    if library_ready {
        response.on_hover_text(text);
    } else {
        response.on_disabled_hover_text("ライブラリが使えないため、曲を選べません");
    }
    if let Some(id) = chosen {
        panel.select(id, view, bgm);
    }
}

/// The play (pause) button and the stop button.
fn transport_buttons(ui: &mut egui::Ui, panel: &mut BgmPanel, bgm: Option<&Bgm>) {
    let transport = panel.transport(bgm);
    let audio_running = bgm.is_some();
    let reason = disabled_reason(transport, audio_running);
    ui.horizontal(|ui| {
        let (label, can_play) = play_button(transport);
        let play = ui.add_enabled(can_play && audio_running, egui::Button::new(label));
        if play.clicked()
            && let Some(bgm) = bgm
        {
            panel.toggle_play(bgm);
        }
        play.on_disabled_hover_text(reason);

        let stop = ui.add_enabled(
            stop_enabled(transport) && audio_running,
            egui::Button::new("■ 停止"),
        );
        if stop.clicked()
            && let Some(bgm) = bgm
        {
            panel.stop(bgm);
        }
        stop.on_disabled_hover_text(reason);
    });
}

/// The bar and the clock: `1:23 / 3:45`.
fn position_bar(ui: &mut egui::Ui, width: f32, panel: &BgmPanel, bgm: Option<&Bgm>) {
    let transport = panel.transport(bgm);
    let (position, length) = match bgm {
        Some(bgm) => panel.position(bgm),
        None => (Duration::ZERO, None),
    };
    let position = shown_position(transport, position);
    let text = format!(
        "{} / {}",
        format_clock(Some(position)),
        format_clock(length)
    );
    ui.add(
        egui::ProgressBar::new(progress_fraction(position, length))
            .desired_width(width)
            .text(text),
    );
}

/// The loop switch and its note.
fn loop_switch(ui: &mut egui::Ui, panel: &mut BgmPanel, bgm: Option<&Bgm>) {
    let mut looping = panel.looping();
    if ui.checkbox(&mut looping, "ループ").changed() {
        panel.set_looping(looping, bgm);
    }
    ui.add(egui::Label::new(egui::RichText::new(LOOP_HINT).small().weak()).wrap());
}

/// Why the song cannot be played, what failed, and the note about the
/// restart.
fn messages(ui: &mut egui::Ui, panel: &BgmPanel) {
    if let Some(problem) = panel.problem() {
        ui.add(egui::Label::new(egui::RichText::new(problem).color(WARNING)).wrap());
    }
    if let Some(failure) = panel.failure() {
        ui.add(egui::Label::new(egui::RichText::new(&failure.summary_ja).color(ERROR)).wrap());
        ui.add(egui::Label::new(egui::RichText::new(&failure.detail).small().weak()).wrap());
    }
    if let Some(notice) = panel.notice() {
        ui.add(egui::Label::new(egui::RichText::new(notice).color(WARNING)).wrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ink_core::graph::{BgmPlayer, bgm_channel};

    use super::super::fonts;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    const ALL: [Transport; 6] = [
        Transport::NoSong,
        Transport::Stopped,
        Transport::Playing,
        Transport::Paused,
        Transport::Finished,
        Transport::Failed,
    ];

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    // -- the pure parts ---------------------------------------------------------

    #[test]
    fn the_play_button_follows_what_the_song_is_doing() {
        use Transport::*;
        let table = [
            (NoSong, "▶ 再生", false),
            (Stopped, "▶ 再生", true),
            (Playing, "一時停止", true),
            (Paused, "▶ 再開", true),
            (Finished, "▶ 頭から", true),
            (Failed, "▶ もう一度", true),
        ];
        for (transport, label, enabled) in table {
            assert_eq!(play_button(transport), (label, enabled), "{transport:?}");
        }
    }

    #[test]
    fn the_labels_use_only_signs_the_bundled_font_has() {
        // Noto Sans JP has ▶ and ■ but not ⏸ (the font is the only one:
        // egui's own fonts are off, ADR-0006).
        for transport in ALL {
            let (label, _) = play_button(transport);
            assert!(!label.contains('\u{23F8}'), "{label}");
        }
    }

    #[test]
    fn stop_can_be_pressed_only_when_there_is_something_to_stop_or_clear() {
        use Transport::*;
        for (transport, expected) in [
            (NoSong, false),
            (Stopped, false),
            (Playing, true),
            (Paused, true),
            (Finished, true),
            (Failed, true),
        ] {
            assert_eq!(stop_enabled(transport), expected, "{transport:?}");
        }
    }

    #[test]
    fn the_reason_is_the_audio_first_then_the_missing_song() {
        assert_eq!(
            disabled_reason(Transport::NoSong, false),
            "音声が止まっています"
        );
        assert_eq!(
            disabled_reason(Transport::Playing, false),
            "音声が止まっています"
        );
        assert_eq!(
            disabled_reason(Transport::NoSong, true),
            "曲を選んでください"
        );
        assert_eq!(disabled_reason(Transport::Stopped, true), "今は押せません");
    }

    #[test]
    fn a_stopped_song_shows_its_start_whatever_position_the_player_still_has() {
        let old = Duration::from_secs(83);
        assert_eq!(shown_position(Transport::Stopped, old), Duration::ZERO);
        assert_eq!(shown_position(Transport::NoSong, old), Duration::ZERO);
        for transport in [
            Transport::Playing,
            Transport::Paused,
            Transport::Finished,
            Transport::Failed,
        ] {
            assert_eq!(shown_position(transport, old), old, "{transport:?}");
        }
    }

    #[test]
    fn the_bar_is_empty_without_a_length_and_never_goes_past_full() {
        let secs = Duration::from_secs;
        assert_eq!(progress_fraction(secs(10), None), 0.0);
        assert_eq!(progress_fraction(secs(10), Some(Duration::ZERO)), 0.0);
        assert_eq!(progress_fraction(secs(0), Some(secs(100))), 0.0);
        assert_eq!(progress_fraction(secs(25), Some(secs(100))), 0.25);
        assert_eq!(progress_fraction(secs(100), Some(secs(100))), 1.0);
        assert_eq!(progress_fraction(secs(150), Some(secs(100))), 1.0);
    }

    #[test]
    fn only_a_playing_song_asks_for_a_repaint_and_not_faster_than_the_frame_rate_cap() {
        for transport in ALL {
            let refresh = position_refresh(transport);
            if transport == Transport::Playing {
                assert_eq!(refresh, Some(Duration::from_millis(250)));
                // The cap is 60 frames a second at most: 16.7 ms apart.
                assert!(refresh.unwrap() > Duration::from_millis(17));
            } else {
                assert_eq!(refresh, None, "{transport:?}");
            }
        }
    }

    #[test]
    fn a_long_name_keeps_its_start_and_its_end_and_a_short_one_is_left_alone() {
        assert_eq!(shorten("", 5), "");
        assert_eq!(shorten("abc", 5), "abc");
        assert_eq!(shorten("abcde", 5), "abcde", "exactly the limit");
        assert_eq!(shorten("abcdef", 5), "ab…ef");
        assert_eq!(shorten("abcdefghij", 5), "ab…ij");
        assert_eq!(shorten("abcdefghij", 1), "…");
        assert_eq!(shorten("abcdefghij", 0), "");
        assert_eq!(shorten("あいう", 5), "あいう", "9 bytes, 3 characters");
        // Characters, not bytes.
        assert_eq!(shorten("とても長い曲の名前です", 5), "とて…です");
        let long = "あ".repeat(100);
        let short = shorten(&long, LIST_NAME_CHARS);
        assert_eq!(short.chars().count(), LIST_NAME_CHARS);
        assert!(short.contains('…'));
    }

    #[test]
    fn the_closed_list_shows_the_name_or_what_the_saved_id_is() {
        let song = id("bgm-0001");
        let found = Resolved::Found {
            path: PathBuf::from("x"),
            name: "曲 テスト".to_string(),
            length: None,
        };
        let missing = Resolved::Missing {
            name: "消えた曲".to_string(),
        };
        let unreadable = Resolved::Unreadable {
            name: "読めない曲".to_string(),
        };
        assert_eq!(selected_text(None), "曲を選ぶ");
        assert_eq!(selected_text(Some((&song, &found))), "曲 テスト");
        assert_eq!(
            selected_text(Some((&song, &missing))),
            "消えた曲（ファイルが無い）"
        );
        assert_eq!(
            selected_text(Some((&song, &unreadable))),
            "読めない曲（読めない）"
        );
        assert_eq!(
            selected_text(Some((&song, &Resolved::Unknown))),
            "（見つかりません） bgm-0001"
        );
        assert_eq!(
            selected_text(Some((&song, &Resolved::WrongKind))),
            "（曲ではありません） bgm-0001"
        );
    }

    fn choice(label: &str, length: Option<Duration>, state: ink_core::AssetState) -> Choice {
        Choice {
            id: id("bgm-0001"),
            label: label.to_string(),
            length,
            state,
        }
    }

    #[test]
    fn a_line_of_the_list_has_the_name_the_mark_and_the_length() {
        use ink_core::AssetState::*;
        let length = Some(Duration::from_secs(185));
        assert_eq!(
            list_item_text(&choice("曲 テスト", length, Available)),
            "曲 テスト　3:05"
        );
        assert_eq!(
            list_item_text(&choice("曲 テスト", None, Available)),
            "曲 テスト"
        );
        assert_eq!(
            list_item_text(&choice("消えた曲", None, Missing)),
            "消えた曲（ファイルが無い）"
        );
        assert_eq!(
            list_item_text(&choice("読めない曲", length, Unreadable)),
            "読めない曲（読めない）　3:05"
        );
        // The mark stays when the name is cut.
        let long = "あ".repeat(100);
        let text = list_item_text(&choice(&long, None, Missing));
        assert!(text.ends_with("（ファイルが無い）"), "{text}");
        assert!(text.contains('…'));
    }

    #[test]
    fn a_song_that_cannot_be_chosen_says_what_the_chosen_song_would_say() {
        use ink_core::AssetState::*;
        for state in [Available, Missing, Unreadable] {
            assert_eq!(
                list_item_hint(&choice("曲 テスト", None, state)),
                unusable_hint_ja("曲 テスト", state),
                "{state:?}"
            );
        }
        // And the text is the one that names the way out.
        assert_eq!(list_item_hint(&choice("曲", None, Available)), None);
        let unreadable = list_item_hint(&choice("読めない曲", None, Unreadable)).unwrap();
        assert!(unreadable.contains("読めない曲") && unreadable.contains("再スキャン"));
        let missing = list_item_hint(&choice("消えた曲", None, Missing)).unwrap();
        assert!(missing.contains("消えた曲") && missing.contains("見つかりません"));
    }

    // -- drawing without a window ------------------------------------------------

    /// A directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ink-mixer-bgm-strip-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn index_entry(id: &str, file: &str, read: bool) -> String {
        let info = if read {
            "\n[asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 1\nframes = 100\n"
        } else {
            ""
        };
        format!("[[asset]]\nid = \"{id}\"\nkind = \"bgm\"\npath = \"{file}\"\n{info}\n")
    }

    /// A library with `bgm-0001` that can be used, `bgm-0002` whose file is
    /// gone and `bgm-0003` that the library could not read. The names are
    /// Japanese; one is very long.
    fn library(name: &str) -> LibraryView {
        let root = test_dir(name);
        let folder = root.join("sounds").join("bgm");
        fs::create_dir_all(&folder).unwrap();
        let long = format!("{}.wav", "とても長い曲の名前".repeat(4));
        fs::write(folder.join("曲 テスト.wav"), "x").unwrap();
        fs::write(folder.join("読めない曲.wav"), "x").unwrap();
        fs::write(folder.join(&long), "x").unwrap();
        let index = format!(
            "version = 1\nnext_id = 100\n\n{}{}{}{}",
            index_entry("bgm-0001", "曲 テスト.wav", true),
            index_entry("bgm-0002", "消えた曲.wav", true),
            index_entry("bgm-0003", "読めない曲.wav", false),
            index_entry("bgm-0004", &long, true),
        );
        fs::write(root.join("library.toml"), index).unwrap();
        let view = LibraryView::open(Some(&root));
        assert!(view.library().is_some());
        view
    }

    /// The player (the audio side) and the `Bgm` (the control side), in the
    /// order of `AudioSession` (ADR-0009): the fields of a struct drop in the
    /// order they are declared, so `_player` drops first and `bgm` after it.
    /// (Plain `let`s drop the other way round, the last one first, which would
    /// drop the `Bgm` before its player.)
    struct AudioSide {
        _player: BgmPlayer,
        bgm: Bgm,
    }

    impl AudioSide {
        fn new() -> Self {
            let (controller, feeder, player) = bgm_channel(48_000, 128);
            let bgm = Bgm::start(controller, feeder).unwrap();
            Self {
                _player: player,
                bgm,
            }
        }
    }

    /// The strip and the controls side by side, in one row: the controls
    /// take the rest of the row. The window lays them out in two rows; this
    /// is the way that once made the controls take the layout of the row
    /// (left to right) and go out of it.
    fn show_row(
        strip: &mut BgmStrip,
        ui: &mut egui::Ui,
        control: &ChannelControl,
        panel: &mut BgmPanel,
        view: &LibraryView,
        bgm: Option<&Bgm>,
    ) {
        ui.horizontal_top(|ui| {
            strip.show_strip(ui, control);
            strip.show_controls(ui, panel, view, bgm);
        });
    }

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
                egui::vec2(width, 600.0),
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

    #[test]
    fn it_draws_without_audio_and_without_a_library() {
        let ctx = context();
        let mut strip = BgmStrip::new(Instant::now());
        let control = ChannelControl::new(ink_core::graph::BGM_VOLUME_RANGE);
        let view = LibraryView::open(None);
        let mut panel = BgmPanel::new(Some(id("bgm-0005")), true);
        for _ in 0..3 {
            frame(&ctx, 720.0, |ui| {
                show_row(&mut strip, ui, &control, &mut panel, &view, None);
            });
        }
        // Drawing changes nothing.
        assert_eq!(panel.selected(), Some(&id("bgm-0005")));
        assert!(panel.looping());
    }

    #[test]
    fn it_draws_every_saved_song_state_and_keeps_the_choice() {
        let ctx = context();
        let view = library("states");
        let control = ChannelControl::new(ink_core::graph::BGM_VOLUME_RANGE);
        for song in ["bgm-0001", "bgm-0002", "bgm-0003", "bgm-0004", "bgm-0099"] {
            let mut strip = BgmStrip::new(Instant::now());
            let mut panel = BgmPanel::new(Some(id(song)), false);
            frame(&ctx, 720.0, |ui| {
                show_row(&mut strip, ui, &control, &mut panel, &view, None);
            });
            assert_eq!(panel.selected(), Some(&id(song)), "{song}");
        }
    }

    #[test]
    fn it_draws_with_a_player_in_every_width_down_to_the_narrowest() {
        let ctx = context();
        let view = library("with-player");
        let control = ChannelControl::new(ink_core::graph::BGM_VOLUME_RANGE);
        let audio = AudioSide::new();
        let bgm = &audio.bgm;
        let mut panel = BgmPanel::new(None, false);
        panel.select(Some(id("bgm-0001")), &view, Some(bgm));
        let mut strip = BgmStrip::new(Instant::now());
        for width in [900.0, 720.0, 480.0, 300.0, 200.0] {
            frame(&ctx, width, |ui| {
                show_row(&mut strip, ui, &control, &mut panel, &view, Some(bgm));
            });
        }
        assert_eq!(panel.selected(), Some(&id("bgm-0001")));
    }

    #[test]
    fn the_meter_of_the_channel_makes_the_strip_active() {
        let now = Instant::now();
        let control = ChannelControl::new(ink_core::graph::BGM_VOLUME_RANGE);
        let mut strip = BgmStrip::new(now);
        assert!(!strip.active());
        control.meter().record(0.5);
        strip.update_meter(&control, now);
        assert!(strip.active());
    }

    #[test]
    fn next_to_the_strip_the_controls_fill_the_rest_of_the_row_and_stay_inside_it() {
        let ctx = context();
        let view = library("row");
        let control = ChannelControl::new(ink_core::graph::BGM_VOLUME_RANGE);
        let mut strip = BgmStrip::new(Instant::now());
        let mut panel = BgmPanel::new(Some(id("bgm-0004")), true);
        // The controls once took the left-to-right layout of the row and
        // went out of it: 1,700 points wide, 580 high.
        for width in [900.0, 720.0, 600.0, 400.0] {
            // A row that is aligned to the top, and a plain `ui.horizontal`.
            let sizes = [
                frame(&ctx, width, |ui| {
                    show_row(&mut strip, ui, &control, &mut panel, &view, None);
                }),
                frame(&ctx, width, |ui| {
                    ui.horizontal(|ui| {
                        strip.show_strip(ui, &control);
                        strip.show_controls(ui, &mut panel, &view, None);
                    });
                }),
            ];
            for size in sizes {
                assert!(size.x <= width + 1.0, "{width}: {size:?}");
                assert!(size.x >= width - 4.0, "{width}: {size:?}");
                assert!(size.y < 400.0, "{width}: {size:?}");
            }
        }
    }

    #[test]
    fn the_controls_take_the_width_they_are_given_and_not_less_than_the_minimum() {
        let ctx = context();
        let view = library("widths");
        let mut strip = BgmStrip::new(Instant::now());
        let mut panel = BgmPanel::new(Some(id("bgm-0004")), true);
        let mut measured = Vec::new();
        for width in [700.0, 400.0, 250.0, 100.0] {
            let size = frame(&ctx, width, |ui| {
                strip.show_controls(ui, &mut panel, &view, None);
            });
            measured.push((width, size.x));
        }
        // Wide: it fills what is there (less the frame and the window's own
        // margin); narrow: it does not shrink under the minimum.
        let (_, wide) = measured[0];
        let (_, middle) = measured[1];
        let (_, narrowest) = measured[3];
        assert!(wide > middle, "{measured:?}");
        assert_eq!(MIN_CONTROLS_WIDTH, 180.0, "the width the plan says");
        assert!(narrowest >= 180.0, "{measured:?}");
        assert!(
            (narrowest - 180.0).abs() < 40.0,
            "{measured:?}: the minimum is about {MIN_CONTROLS_WIDTH}"
        );
    }
}
