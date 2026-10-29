//! The GUI (ADR-0004), opened by `ink-mixer` without a subcommand.
//!
//! eframe with the glow (OpenGL) renderer, or wgpu with the `wgpu` feature.
//! The window libraries (X11 / Wayland / OpenGL on Linux) are loaded only
//! here, at runtime, so `devices` and `run` work without a display.
//!
//! When the window opens, the saved levels are restored and audio starts
//! with the configured (or default) devices (#40). The window then lets the
//! user choose the devices and start / stop (#41), and saves the choices and
//! the levels (#42).
//!
//! The song and the Sound Pad (#64) are made here too: the library is opened
//! (the folders are made if they are not there; a broken index is not
//! touched), the loader of the sound effects is started, and the song and the
//! pads of the settings are given to the window. The levels of the BGM and
//! the SE come back with the others (`settings::restore`).

mod app;
mod bgm_strip;
mod device_panel;
mod fonts;
mod frame_limiter;
mod meter;
mod sound_pad;
mod strip;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use ink_backend::CpalBackend;
use ink_core::Config;
use ink_core::graph::MixerControl;
use tracing::{error, info, warn};

use crate::library_cmd::library_root;
use crate::library_view::LibraryView;
use crate::se_loader::SeLoader;
use crate::session::{AudioSession, SessionOptions, choose_devices};
use crate::settings::{self, Settings, SoundChoices};
use crate::sound_state::{BgmPanel, PadBank};
use app::Sounds;

/// The window title, also used by eframe as the application name.
const TITLE: &str = "Ink Mixer";
/// The size the window opens with, and the least it can be made, in points.
/// The middle scrolls up and down, and nothing is wider than the least width
/// (the strips and the Sound Pad were measured at 720).
const DEFAULT_SIZE: (f32, f32) = (900.0, 640.0);
const MIN_SIZE: (f32, f32) = (720.0, 480.0);
/// What every session of the window is made of: the song is always there,
/// so the BGM strip and the controls have something to work with.
const SESSION_OPTIONS: SessionOptions = SessionOptions { bgm: true };

/// Opens the window and runs until it is closed. Exits with 1 when the
/// window cannot be opened (no display, no OpenGL, ...).
///
/// `save_to` tells where the settings are saved: `Ok(path)`, or `Err(reason)`
/// when they must not be saved (the file could not be read), with the reason
/// to show in the window.
pub fn run(config: &Config, save_to: Result<PathBuf, String>) -> ExitCode {
    let result = eframe::run_native(
        TITLE,
        native_options(),
        Box::new(|cc| {
            report_renderer(cc);
            fonts::install(&cc.egui_ctx);
            let backend = CpalBackend::new();
            let (control, session) = start_audio(config, &backend);
            let settings = make_settings(config, save_to, &control);
            // The loader asks the window to repaint when it has a result, so
            // nothing has to look for it.
            let context = cc.egui_ctx.clone();
            let loader = SeLoader::start(Box::new(move || context.request_repaint()));
            let sounds = make_sounds(
                settings.sound_choices(),
                library_root(),
                loader,
                Instant::now(),
            );
            // The window keeps the backend to start and stop the audio.
            let window =
                app::InkMixerApp::new(Box::new(backend), config, control, session, settings);
            Ok(Box::new(match sounds {
                Ok(sounds) => window.with_sounds(sounds),
                Err(reason) => window.with_notice(reason),
            }))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(
                "failed to open the window: {e} \
                 (`ink-mixer devices` and `ink-mixer run` work without a window)"
            );
            ExitCode::FAILURE
        }
    }
}

/// Creates the shared controls with the saved levels restored. Nothing is
/// saved on the first run; the master volume is then the start value, and
/// never higher than it (see `settings.rs`).
fn new_control(config: &Config) -> Arc<MixerControl> {
    let control = Arc::new(MixerControl::new());
    settings::restore(&control, &config.mixer);
    control
}

/// Creates the settings to save, from the controls as they were restored.
fn make_settings(
    config: &Config,
    save_to: Result<PathBuf, String>,
    control: &MixerControl,
) -> Settings {
    let now = Instant::now();
    match save_to {
        Ok(path) => Settings::new(config.clone(), path, control, now),
        Err(reason) => {
            warn!("the settings are not saved: {reason}");
            Settings::disabled(config.clone(), reason, control, now)
        }
    }
}

/// Gathers the song and the Sound Pad: opens the library in `root` and puts
/// the saved choices in the parts that show them. A library that cannot be
/// used is not an error here: the window tells why and keeps the choices.
///
/// Without the loader of the sound effects there is no Sound Pad, and so no
/// song either: the reason is returned for the window to show. The window
/// then never passes choices to the settings, so the saved song and pads are
/// still in the file the next time.
fn make_sounds(
    choices: &SoundChoices,
    root: Option<PathBuf>,
    loader: io::Result<SeLoader>,
    now: Instant,
) -> Result<Sounds, String> {
    let loader = loader.map_err(|e| {
        error!("the loader of the sound effects could not start: {e}");
        format!(
            "SE を読み込む仕組みを開始できませんでした（{e}）。BGM と SE は使えません。\
             保存してある曲とパッドの設定は消えません"
        )
    })?;
    let view = LibraryView::open(root.as_deref());
    Ok(Sounds::new(
        view,
        root,
        BgmPanel::new(choices.bgm_asset, choices.bgm_loop),
        PadBank::new(&choices.pads, choices.pad_policy),
        loader,
        now,
    ))
}

/// Creates the shared controls (with the saved levels restored) and starts
/// audio with the configured (or default) devices, with the BGM. The levels
/// are set before the audio starts, so the first block already uses them. A
/// failure to start is returned for the window to show.
fn start_audio(
    config: &Config,
    backend: &CpalBackend,
) -> (Arc<MixerControl>, Result<AudioSession, String>) {
    let control = new_control(config);

    let session = choose_devices(backend, config, None, None).and_then(|(input, output)| {
        AudioSession::start_with(backend, input, output, &control, SESSION_OPTIONS)
    });
    match &session {
        Ok(session) => info!(
            "audio started: {} -> {}, {} Hz",
            session.input_device().id,
            session.output_device().id,
            session.sample_rate()
        ),
        Err(e) => error!("audio could not start: {e}"),
    }
    (control, session)
}

fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([DEFAULT_SIZE.0, DEFAULT_SIZE.1])
            .with_min_inner_size([MIN_SIZE.0, MIN_SIZE.1]),
        renderer: renderer(),
        ..Default::default()
    }
}

/// glow (OpenGL) by default; wgpu when built with the `wgpu` feature.
#[cfg(not(feature = "wgpu"))]
fn renderer() -> eframe::Renderer {
    eframe::Renderer::Glow
}

/// glow (OpenGL) by default; wgpu when built with the `wgpu` feature.
#[cfg(feature = "wgpu")]
fn renderer() -> eframe::Renderer {
    eframe::Renderer::Wgpu
}

/// Logs the GPU in use, since a laptop may have two.
fn report_renderer(cc: &eframe::CreationContext<'_>) {
    if let Some(gl) = &cc.gl {
        use eframe::glow::HasContext as _;
        // SAFETY: plain string queries on the context that eframe created
        // and made current for this window.
        let (vendor, renderer) = unsafe {
            (
                gl.get_parameter_string(eframe::glow::VENDOR),
                gl.get_parameter_string(eframe::glow::RENDERER),
            )
        };
        info!("renderer: OpenGL (glow), {vendor}, {renderer}");
    }
    #[cfg(feature = "wgpu")]
    if let Some(state) = &cc.wgpu_render_state {
        let adapter = state.adapter.get_info();
        info!("renderer: wgpu, {} ({:?})", adapter.name, adapter.backend);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ink_core::MixerConfig;

    #[test]
    fn glow_is_the_default_renderer() {
        let expected = if cfg!(feature = "wgpu") {
            "Wgpu"
        } else {
            "Glow"
        };
        assert_eq!(format!("{:?}", renderer()), expected);
    }

    #[test]
    fn the_controls_start_with_the_saved_levels_and_a_master_of_at_most_minus_12() {
        let config_with = |mixer| Config {
            mixer,
            ..Config::default()
        };

        // Nothing saved: the defaults.
        let control = new_control(&Config::default());
        assert_eq!(control.mic.gain_db(), 0.0);
        assert!(!control.mic.is_muted());
        assert_eq!(control.master.gain_db(), -12.0);

        // Saved levels come back; a loud master starts at -12.
        let control = new_control(&config_with(MixerConfig::new(5.5, true, 6.0)));
        assert_eq!(control.mic.gain_db(), 5.5);
        assert!(control.mic.is_muted());
        assert_eq!(control.master.gain_db(), -12.0);

        // A quieter master is kept.
        let control = new_control(&config_with(MixerConfig::new(-3.0, false, -20.0)));
        assert_eq!(control.mic.gain_db(), -3.0);
        assert_eq!(control.master.gain_db(), -20.0);
    }

    #[test]
    fn settings_are_saved_to_a_path_or_disabled_with_a_reason() {
        let control = MixerControl::new();
        let saved = make_settings(&Config::default(), Ok(PathBuf::from("c.toml")), &control);
        assert_eq!(saved.disabled_reason(), None);

        let reason = "設定ファイルを読めないため、設定は保存されません".to_string();
        let off = make_settings(&Config::default(), Err(reason.clone()), &control);
        assert_eq!(off.disabled_reason(), Some(reason.as_str()));
    }

    #[test]
    fn window_has_title_and_size() {
        let options = native_options();
        assert_eq!(options.viewport.title.as_deref(), Some(TITLE));
        assert_eq!(options.viewport.inner_size, Some(egui::vec2(900.0, 640.0)));
        assert_eq!(
            options.viewport.min_inner_size,
            Some(egui::vec2(720.0, 480.0))
        );
    }

    #[test]
    fn every_session_of_the_window_has_the_bgm() {
        // `start_audio` and the window's start both use this.
        assert_eq!(SESSION_OPTIONS, SessionOptions { bgm: true });
    }

    // -- the song and the Sound Pad ------------------------------------------

    use ink_core::graph::SePolicy;
    use ink_core::{AssetId, BgmConfig, PadSetting, SoundPadConfig};

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    fn saved_choices() -> SoundChoices {
        SoundChoices {
            bgm_asset: Some(id("bgm-0001")),
            bgm_loop: true,
            pad_policy: SePolicy::Restart,
            pads: vec![
                PadSetting {
                    slot: 2,
                    asset: id("se-0001"),
                    gain_db: -3.0,
                },
                PadSetting {
                    slot: 5,
                    asset: id("se-0099"),
                    gain_db: 1.5,
                },
            ],
        }
    }

    fn config_with(choices: &SoundChoices) -> Config {
        Config {
            bgm: BgmConfig::new(choices.bgm_asset.as_ref(), choices.bgm_loop),
            sound_pad: SoundPadConfig::new(choices.pad_policy, &choices.pads),
            ..Config::default()
        }
    }

    /// A fresh directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ink-mixer-gui-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn quiet_loader() -> io::Result<SeLoader> {
        SeLoader::start(Box::new(|| {}))
    }

    #[test]
    fn the_sounds_start_with_the_saved_song_and_pads_whatever_the_library_is() {
        let choices = saved_choices();
        let now = Instant::now();

        // No place for the library: the choices are kept, and the window says
        // why the library cannot be used.
        let sounds = make_sounds(&choices, None, quiet_loader(), now).unwrap();
        assert_eq!(sounds.choices(), choices);

        // A library that cannot be used: the same, and the index is not touched.
        let root = test_dir("broken-index");
        let index = root.join("library.toml");
        std::fs::write(&index, "garbage").unwrap();
        let sounds = make_sounds(&choices, Some(root.clone()), quiet_loader(), now).unwrap();
        assert_eq!(sounds.choices(), choices);
        assert_eq!(std::fs::read_to_string(&index).unwrap(), "garbage");
        assert!(
            !root.join("sounds").exists(),
            "a library that cannot be used makes no folder"
        );

        // Nothing saved: the defaults.
        let sounds = make_sounds(&SoundChoices::default(), None, quiet_loader(), now).unwrap();
        assert_eq!(sounds.choices(), SoundChoices::default());
    }

    #[test]
    fn a_library_that_can_be_used_gets_its_folders_and_no_index() {
        let root = test_dir("new-library");
        let sounds = make_sounds(
            &SoundChoices::default(),
            Some(root.clone()),
            quiet_loader(),
            Instant::now(),
        )
        .unwrap();
        assert!(root.join("sounds").join("bgm").is_dir());
        assert!(root.join("sounds").join("se").is_dir());
        assert!(
            !root.join("library.toml").exists(),
            "the index is written by the first change, not by opening"
        );
        drop(sounds);
    }

    #[test]
    fn without_the_loader_there_are_no_sounds_and_the_saved_choices_stay_in_the_file() {
        let choices = saved_choices();
        let config = config_with(&choices);
        let dir = test_dir("no-loader");
        let path = dir.join("config.toml");
        let t0 = Instant::now();
        let control = new_control(&config);
        let mut settings = make_settings(&config, Ok(path.clone()), &control);

        let result = make_sounds(
            settings.sound_choices(),
            None,
            Err(io::Error::other("no thread")),
            t0,
        );
        let reason = result.err().expect("no loader, no sounds");
        assert!(reason.contains("BGM と SE は使えません"), "{reason}");
        assert!(reason.contains("消えません"), "{reason}");
        assert!(reason.contains("no thread"), "{reason}");

        // The window has no sounds, so it passes no choices; another change
        // is saved, and the song and the pads come back as they were.
        control.mic.set_gain_db(4.0);
        assert_eq!(settings.flush(&control), Some(Ok(())));
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.mixer.mic_gain_db(), Some(4.0));
        assert_eq!(SoundChoices::from_config(&saved), choices);
    }
}
