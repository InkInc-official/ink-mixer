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

mod app;
mod device_panel;
mod fonts;
mod frame_limiter;
mod meter;
mod strip;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use ink_backend::CpalBackend;
use ink_core::Config;
use ink_core::graph::MixerControl;
use tracing::{error, info, warn};

use crate::session::{AudioSession, choose_devices};
use crate::settings::{self, Settings};

/// The window title, also used by eframe as the application name.
const TITLE: &str = "Ink Mixer";

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
            // The window keeps the backend to start and stop the audio.
            Ok(Box::new(app::InkMixerApp::new(
                Box::new(backend),
                config,
                control,
                session,
                settings,
            )))
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

/// Creates the shared controls (with the saved levels restored) and starts
/// audio with the configured (or default) devices. The levels are set
/// before the audio starts, so the first block already uses them. A failure
/// to start is returned for the window to show.
fn start_audio(
    config: &Config,
    backend: &CpalBackend,
) -> (Arc<MixerControl>, Result<AudioSession, String>) {
    let control = new_control(config);

    let session = choose_devices(backend, config, None, None)
        .and_then(|(input, output)| AudioSession::start(backend, input, output, &control));
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
            .with_inner_size([640.0, 520.0])
            .with_min_inner_size([480.0, 420.0]),
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
        assert_eq!(options.viewport.inner_size, Some(egui::vec2(640.0, 520.0)));
    }
}
