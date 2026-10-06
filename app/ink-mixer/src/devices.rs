//! `ink-mixer devices`: 入出力デバイスの一覧を stdout に表示する。
//!
//! ログ（warn / error）は stderr に出し、stdout には一覧だけを書く（AGENTS.md §10）。

use std::fmt::Write as _;
use std::process::ExitCode;

use ink_backend::{AudioBackend, DeviceId, DeviceInfo};
use ink_core::Config;
use tracing::{error, warn};

/// デフォルトデバイスの取得結果。
#[derive(Debug)]
enum DefaultDevice {
    Found(DeviceId),
    None,
    Unavailable,
}

/// 表示に必要な情報をまとめたもの。
#[derive(Debug)]
struct DeviceReport {
    inputs: Vec<DeviceInfo>,
    outputs: Vec<DeviceInfo>,
    default_input: DefaultDevice,
    default_output: DefaultDevice,
    selected_input: Option<DeviceId>,
    selected_output: Option<DeviceId>,
}

/// デバイス一覧を取得して表示する。一覧の取得に失敗したら終了コード 1。
pub fn run(backend: &dyn AudioBackend, config: &Config) -> ExitCode {
    let inputs = match backend.enumerate_input_devices() {
        Ok(devices) => devices,
        Err(e) => {
            error!("failed to list input devices: {e}");
            return ExitCode::FAILURE;
        }
    };
    let outputs = match backend.enumerate_output_devices() {
        Ok(devices) => devices,
        Err(e) => {
            error!("failed to list output devices: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = DeviceReport {
        inputs,
        outputs,
        default_input: default_id("input", backend.default_input()),
        default_output: default_id("output", backend.default_output()),
        selected_input: config.audio.input_device.clone(),
        selected_output: config.audio.output_device.clone(),
    };
    print!("{}", format_devices(&report));
    ExitCode::SUCCESS
}

fn default_id(direction: &str, result: ink_backend::Result<Option<DeviceInfo>>) -> DefaultDevice {
    match result {
        Ok(Some(device)) => DefaultDevice::Found(device.id),
        Ok(None) => DefaultDevice::None,
        Err(e) => {
            warn!("failed to get the default {direction} device: {e}");
            DefaultDevice::Unavailable
        }
    }
}

fn format_devices(report: &DeviceReport) -> String {
    let mut out = String::new();
    write_list(
        &mut out,
        "Input devices:",
        &report.inputs,
        &report.default_input,
        report.selected_input.as_ref(),
    );
    out.push('\n');
    write_list(
        &mut out,
        "Output devices:",
        &report.outputs,
        &report.default_output,
        report.selected_output.as_ref(),
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "Default input:   {}",
        default_text(&report.default_input)
    );
    let _ = writeln!(
        out,
        "Selected input:  {}",
        selected_text(report.selected_input.as_ref(), &report.inputs)
    );
    let _ = writeln!(
        out,
        "Default output:  {}",
        default_text(&report.default_output)
    );
    let _ = writeln!(
        out,
        "Selected output: {}",
        selected_text(report.selected_output.as_ref(), &report.outputs)
    );
    out
}

fn write_list(
    out: &mut String,
    title: &str,
    devices: &[DeviceInfo],
    default: &DefaultDevice,
    selected: Option<&DeviceId>,
) {
    let _ = writeln!(out, "{title}");
    if devices.is_empty() {
        let _ = writeln!(out, "  (none)");
        return;
    }
    for device in devices {
        let mut marks = String::new();
        if matches!(default, DefaultDevice::Found(id) if *id == device.id) {
            marks.push_str("  [default]");
        }
        if selected == Some(&device.id) {
            marks.push_str("  [selected]");
        }
        let rates: Vec<String> = device.sample_rates.iter().map(u32::to_string).collect();
        let _ = writeln!(out, "  {}{marks}", device.name);
        let _ = writeln!(out, "    id:           {}", device.id);
        let _ = writeln!(out, "    channels:     {}", device.channels);
        let _ = writeln!(out, "    sample rates: {}", rates.join(", "));
    }
}

fn default_text(default: &DefaultDevice) -> String {
    match default {
        DefaultDevice::Found(id) => id.to_string(),
        DefaultDevice::None => "(none)".to_string(),
        DefaultDevice::Unavailable => "(unavailable)".to_string(),
    }
}

fn selected_text(selected: Option<&DeviceId>, devices: &[DeviceInfo]) -> String {
    match selected {
        None => "(not set)".to_string(),
        Some(id) if devices.iter().any(|d| d.id == *id) => id.to_string(),
        Some(id) => format!("{id} (not found)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(id),
            name: name.to_string(),
            channels: 2,
            sample_rates: vec![44_100, 48_000],
        }
    }

    fn report() -> DeviceReport {
        DeviceReport {
            inputs: vec![device("alsa:default", "Default In")],
            outputs: vec![
                device("alsa:default", "Default Out"),
                device("alsa:pulse", "Pulse"),
            ],
            default_input: DefaultDevice::Found(DeviceId::new("alsa:default")),
            default_output: DefaultDevice::Found(DeviceId::new("alsa:default")),
            selected_input: None,
            selected_output: Some(DeviceId::new("alsa:pulse")),
        }
    }

    #[test]
    fn full_output() {
        assert_eq!(
            format_devices(&report()),
            "\
Input devices:
  Default In  [default]
    id:           alsa:default
    channels:     2
    sample rates: 44100, 48000

Output devices:
  Default Out  [default]
    id:           alsa:default
    channels:     2
    sample rates: 44100, 48000
  Pulse  [selected]
    id:           alsa:pulse
    channels:     2
    sample rates: 44100, 48000

Default input:   alsa:default
Selected input:  (not set)
Default output:  alsa:default
Selected output: alsa:pulse
"
        );
    }

    #[test]
    fn default_and_selected_on_the_same_device() {
        let mut r = report();
        r.selected_output = Some(DeviceId::new("alsa:default"));
        assert!(format_devices(&r).contains("  Default Out  [default]  [selected]\n"));
    }

    #[test]
    fn selected_device_missing_from_the_list() {
        let mut r = report();
        r.selected_input = Some(DeviceId::new("alsa:sysdefault:CARD=GONE"));
        let text = format_devices(&r);
        assert!(text.contains("Selected input:  alsa:sysdefault:CARD=GONE (not found)\n"));
        let input_section = text.split("Output devices:").next().unwrap();
        assert!(!input_section.contains("[selected]"));
    }

    #[test]
    fn no_default_and_unavailable_default() {
        let mut r = report();
        r.default_input = DefaultDevice::None;
        r.default_output = DefaultDevice::Unavailable;
        let text = format_devices(&r);
        assert!(text.contains("Default input:   (none)\n"));
        assert!(text.contains("Default output:  (unavailable)\n"));
        assert!(!text.contains("[default]"));
    }

    #[test]
    fn empty_lists() {
        let r = DeviceReport {
            inputs: Vec::new(),
            outputs: Vec::new(),
            default_input: DefaultDevice::None,
            default_output: DefaultDevice::None,
            selected_input: None,
            selected_output: None,
        };
        assert_eq!(
            format_devices(&r),
            "\
Input devices:
  (none)

Output devices:
  (none)

Default input:   (none)
Selected input:  (not set)
Default output:  (none)
Selected output: (not set)
"
        );
    }
}
