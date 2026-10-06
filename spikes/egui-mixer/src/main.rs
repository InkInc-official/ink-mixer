//! Spike (GitHub #21): egui mixer prototype for measuring memory / CPU.
//!
//! Dummy meters only; no audio I/O and no dependency on ink-core / ink-backend.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

/// Candidate system fonts for Japanese text, tried in order.
/// The fonts are read at runtime and never redistributed.
const JAPANESE_FONTS: &[(&str, u32)] = &[
    // Linux: Noto Sans CJK JP (OFL-1.1). Index 0 in the collection is JP.
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
    // Windows: Yu Gothic Medium, then Meiryo.
    (r"C:\Windows\Fonts\YuGothM.ttc", 0),
    (r"C:\Windows\Fonts\meiryo.ttc", 0),
];

const CHANNELS: [(&str, &str); 5] = [
    ("MIC", "マイク"),
    ("BGM", "BGM"),
    ("SE", "効果音"),
    ("AUX", "外部入力"),
    ("MASTER", "マスター"),
];

const USAGE: &str = "usage: egui-mixer [--renderer glow|wgpu] [--vsync on|off] [--fps 60|30|max]";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RendererChoice {
    Glow,
    Wgpu,
}

/// Command-line options for profiling (GitHub #21).
#[derive(Clone, Copy)]
struct Options {
    renderer: RendererChoice,
    vsync: bool,
    /// Frame rate cap while the meters run; `None` repaints as fast as possible.
    fps: Option<u32>,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            renderer: RendererChoice::Glow,
            vsync: true,
            fps: Some(60),
        };
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match (flag.as_str(), value.as_str()) {
                ("--renderer", "glow") => options.renderer = RendererChoice::Glow,
                ("--renderer", "wgpu") => options.renderer = RendererChoice::Wgpu,
                ("--vsync", "on") => options.vsync = true,
                ("--vsync", "off") => options.vsync = false,
                ("--fps", "60") => options.fps = Some(60),
                ("--fps", "30") => options.fps = Some(30),
                ("--fps", "max") => options.fps = None,
                _ => return Err(format!("invalid option: {flag} {value}")),
            }
        }
        if options.renderer == RendererChoice::Wgpu && !cfg!(feature = "wgpu") {
            return Err("--renderer wgpu needs a build with `--features wgpu`".to_string());
        }
        Ok(options)
    }

    fn label(self) -> String {
        let renderer = match self.renderer {
            RendererChoice::Glow => "glow",
            RendererChoice::Wgpu => "wgpu",
        };
        let vsync = if self.vsync { "on" } else { "off" };
        let fps = self.fps.map_or("max".to_string(), |f| f.to_string());
        format!("{renderer} / vsync {vsync} / fps {fps}")
    }

    fn native_options(self) -> eframe::NativeOptions {
        let mut native = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([640.0, 420.0])
                .with_title("Ink Mixer — egui spike"),
            ..Default::default()
        };
        native.glow_options.vsync = self.vsync;
        native.renderer = match self.renderer {
            RendererChoice::Glow => eframe::Renderer::Glow,
            #[cfg(feature = "wgpu")]
            RendererChoice::Wgpu => {
                configure_wgpu(&mut native.wgpu_options, self.vsync);
                eframe::Renderer::Wgpu
            }
            #[cfg(not(feature = "wgpu"))]
            RendererChoice::Wgpu => unreachable!("rejected in Options::parse"),
        };
        native
    }
}

#[cfg(feature = "wgpu")]
fn configure_wgpu(config: &mut eframe::egui_wgpu::WgpuConfiguration, vsync: bool) {
    use eframe::wgpu;
    config.surface.present_mode = if vsync {
        wgpu::PresentMode::Fifo
    } else {
        wgpu::PresentMode::AutoNoVsync
    };
    // On Windows use DX12 only; elsewhere keep wgpu's default backends.
    #[cfg(windows)]
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup {
        setup.instance_descriptor.backends = wgpu::Backends::DX12;
    }
}

/// Prints the GPU actually used, since a laptop may have two.
fn report_gpu(cc: &eframe::CreationContext<'_>) {
    if let Some(gl) = &cc.gl {
        use eframe::glow::HasContext as _;
        // SAFETY: plain string queries on the context created by eframe.
        let (vendor, renderer) = unsafe {
            (
                gl.get_parameter_string(eframe::glow::VENDOR),
                gl.get_parameter_string(eframe::glow::RENDERER),
            )
        };
        eprintln!("gpu: glow vendor={vendor} renderer={renderer}");
    }
    #[cfg(feature = "wgpu")]
    if let Some(state) = &cc.wgpu_render_state {
        let info = state.adapter.get_info();
        eprintln!("gpu: wgpu name={} backend={:?}", info.name, info.backend);
    }
}

fn main() -> std::process::ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}\n{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };
    eprintln!("settings: {}", options.label());

    let result = eframe::run_native(
        "Ink Mixer egui spike",
        options.native_options(),
        Box::new(move |cc| {
            report_gpu(cc);
            install_japanese_font(&cc.egui_ctx);
            Ok(Box::new(MixerApp::new(options)))
        }),
    );
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn install_japanese_font(ctx: &egui::Context) {
    let Some((path, index, bytes)) = JAPANESE_FONTS
        .iter()
        .find_map(|&(path, index)| std::fs::read(path).ok().map(|b| (path, index, b)))
    else {
        eprintln!("warning: no Japanese font found; Japanese labels will not render");
        return;
    };
    eprintln!("using Japanese font: {path} (index {index})");

    let mut fonts = egui::FontDefinitions::default();
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    fonts.font_data.insert("japanese".into(), Arc::new(data));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "japanese".into());
    }
    ctx.set_fonts(fonts);
}

struct Channel {
    gain_db: f32,
    muted: bool,
}

struct MixerApp {
    options: Options,
    start: Instant,
    /// When the next capped frame is due (fixed-interval schedule).
    next_frame: Instant,
    channels: Vec<Channel>,
    meters_paused: bool,
    noise_reduction: bool,
    voice_shaping: bool,
    frames: u32,
    fps_window_start: Instant,
    fps: u32,
}

impl MixerApp {
    fn new(options: Options) -> Self {
        let now = Instant::now();
        Self {
            options,
            start: now,
            next_frame: now,
            channels: CHANNELS
                .iter()
                .map(|_| Channel {
                    gain_db: 0.0,
                    muted: false,
                })
                .collect(),
            meters_paused: false,
            noise_reduction: false,
            voice_shaping: false,
            frames: 0,
            fps_window_start: now,
            fps: 0,
        }
    }

    fn count_frame(&mut self) {
        self.frames += 1;
        let elapsed = self.fps_window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.fps = (f64::from(self.frames) / elapsed.as_secs_f64()).round() as u32;
            self.frames = 0;
            self.fps_window_start = Instant::now();
        }
    }
}

/// Dummy meter level in 0.0..=1.0, from a sine plus a faster "noise" sine.
fn dummy_level(t: f32, channel: usize) -> f32 {
    let phase = channel as f32 * 1.3;
    let base = 0.45 + 0.35 * (t * (0.7 + channel as f32 * 0.2) + phase).sin();
    let jitter = 0.12 * (t * 23.0 + phase * 3.0).sin() * (t * 7.0).cos();
    (base + jitter).clamp(0.0, 1.0)
}

fn draw_meter(ui: &mut egui::Ui, level: f32) {
    let size = egui::vec2(16.0, 180.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(40));
    let height = rect.height() * level;
    let filled =
        egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - height), rect.max);
    let color = if level > 0.9 {
        egui::Color32::from_rgb(220, 60, 60)
    } else if level > 0.7 {
        egui::Color32::from_rgb(230, 190, 60)
    } else {
        egui::Color32::from_rgb(70, 190, 110)
    };
    painter.rect_filled(filled, 2.0, color);
}

impl eframe::App for MixerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        self.count_frame();
        let t = if self.meters_paused {
            0.0
        } else {
            self.start.elapsed().as_secs_f32()
        };

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.toggle_value(&mut self.noise_reduction, "ノイズ除去");
                ui.toggle_value(&mut self.voice_shaping, "声を整える");
                ui.separator();
                ui.toggle_value(&mut self.meters_paused, "メーター停止");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.monospace(format!("{} fps  ({})", self.fps, self.options.label()));
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for (i, ((name, label), channel)) in
                    CHANNELS.iter().zip(self.channels.iter_mut()).enumerate()
                {
                    ui.group(|ui| {
                        // Fixed strip width so that all five channels fit side by side.
                        ui.set_width(96.0);
                        ui.vertical_centered(|ui| {
                            ui.strong(*name);
                            ui.label(*label);
                            ui.horizontal(|ui| {
                                let gain = 10f32.powf(channel.gain_db / 20.0);
                                let level = if channel.muted || self.meters_paused {
                                    0.0
                                } else {
                                    (dummy_level(t, i) * gain).min(1.0)
                                };
                                draw_meter(ui, level);
                                ui.add(
                                    egui::Slider::new(&mut channel.gain_db, -60.0..=6.0)
                                        .vertical()
                                        .step_by(0.5)
                                        .suffix(" dB"),
                                );
                            });
                            ui.toggle_value(&mut channel.muted, "ミュート");
                        });
                    });
                }
            });
        });

        // While the meters run, cap the frame rate here; when paused, repaint
        // only on input.
        if !self.meters_paused {
            match self.options.fps {
                None => ui.ctx().request_repaint(),
                Some(fps) => {
                    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
                    // Advance the schedule only on the scheduled frame, so that
                    // extra repaints (e.g. from input) do not shift it.
                    if frame_start >= self.next_frame {
                        self.next_frame += interval;
                        if self.next_frame <= frame_start {
                            // Fell behind; restart the schedule from now.
                            self.next_frame = frame_start + interval;
                        }
                    }
                    let remaining = self.next_frame.saturating_duration_since(Instant::now());
                    // egui subtracts the predicted frame time from the delay
                    // (Context::request_repaint_after), so add it back; otherwise
                    // a 16 ms delay becomes 0 and the frame rate is not capped.
                    let predicted = Duration::from_secs_f32(ui.input(|i| i.predicted_dt));
                    ui.ctx().request_repaint_after(remaining + predicted);
                }
            }
        }
    }
}
