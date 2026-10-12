//! The GUI (ADR-0004), opened by `ink-mixer` without a subcommand.
//!
//! eframe with the glow (OpenGL) renderer, or wgpu with the `wgpu` feature.
//! The window libraries (X11 / Wayland / OpenGL on Linux) are loaded only
//! here, at runtime, so `devices` and `run` work without a display.
//!
//! When the window opens, audio starts with the configured (or default)
//! devices (#40). Choosing devices and starting / stopping come in #41.

mod app;
mod fonts;
mod frame_limiter;
mod meter;
mod strip;

use std::process::ExitCode;
use std::sync::Arc;

use eframe::egui;
use ink_backend::CpalBackend;
use ink_core::Config;
use ink_core::graph::MixerControl;
use tracing::{error, info};

use crate::session::{AudioSession, choose_devices};

/// The window title, also used by eframe as the application name.
const TITLE: &str = "Ink Mixer";

/// The master volume when the window opens. The microphone is heard at
/// once, so it starts lower to avoid a loud surprise or feedback, as the
/// `run` command does. Saved settings replace it in #42.
const INITIAL_MASTER_DB: f32 = -12.0;

/// Opens the window and runs until it is closed. Exits with 1 when the
/// window cannot be opened (no display, no OpenGL, ...).
pub fn run(config: &Config) -> ExitCode {
    let result = eframe::run_native(
        TITLE,
        native_options(),
        Box::new(|cc| {
            report_renderer(cc);
            fonts::install(&cc.egui_ctx);
            let (control, session) = start_audio(config);
            Ok(Box::new(app::InkMixerApp::new(control, session)))
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

/// Creates the shared controls and starts audio with the configured (or
/// default) devices. A failure is returned for the window to show.
fn start_audio(config: &Config) -> (Arc<MixerControl>, Result<AudioSession, String>) {
    let control = Arc::new(MixerControl::new());
    control.master.set_gain_db(INITIAL_MASTER_DB);

    let backend = CpalBackend::new();
    let session = choose_devices(&backend, config, None, None)
        .and_then(|(input, output)| AudioSession::start(&backend, input, output, &control));
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
            .with_inner_size([640.0, 420.0])
            .with_min_inner_size([480.0, 320.0]),
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
    fn window_has_title_and_size() {
        let options = native_options();
        assert_eq!(options.viewport.title.as_deref(), Some(TITLE));
        assert_eq!(options.viewport.inner_size, Some(egui::vec2(640.0, 420.0)));
    }
}
