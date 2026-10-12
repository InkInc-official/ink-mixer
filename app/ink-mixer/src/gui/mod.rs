//! The GUI (ADR-0004), opened by `ink-mixer` without a subcommand.
//!
//! eframe with the glow (OpenGL) renderer, or wgpu with the `wgpu` feature.
//! The window libraries (X11 / Wayland / OpenGL on Linux) are loaded only
//! here, at runtime, so `devices` and `run` work without a display.

mod app;
mod fonts;
mod frame_limiter;

use std::process::ExitCode;

use eframe::egui;
use tracing::{error, info};

/// The window title, also used by eframe as the application name.
const TITLE: &str = "Ink Mixer";

/// Opens the window and runs until it is closed. Exits with 1 when the
/// window cannot be opened (no display, no OpenGL, ...).
pub fn run() -> ExitCode {
    let result = eframe::run_native(
        TITLE,
        native_options(),
        Box::new(|cc| {
            report_renderer(cc);
            fonts::install(&cc.egui_ctx);
            Ok(Box::new(app::InkMixerApp::new()))
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
