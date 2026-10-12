//! `ink-mixer run`: play MIC -> Gain -> MASTER -> output (#38, ADR-0007).
//!
//! Opens the output first, then the input, and connects them with a ring
//! buffer (`audio_link`). The output callback runs a `MicGraph`. The sample
//! rate and channel counts are chosen by actually opening the devices,
//! because the numbers in `DeviceInfo` cannot be trusted (ALSA `sysdefault`
//! reports that it supports everything).
//!
//! stdout gets only the levels and the results; logs (warnings, errors) go
//! to stderr (AGENTS.md §10).

use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ink_backend::{
    AudioBackend, BackendError, DeviceId, DeviceInfo, InputStream, MAX_FRAMES_PER_CALLBACK,
    OutputStream, StreamConfig,
};
use ink_core::Config;
use ink_core::graph::{
    LinkStats, MicGraph, MicrophoneSource, MixerControl, SILENCE_DB, audio_link, gain_to_db,
};
use tracing::{debug, error, warn};

use crate::cli::RunArgs;

/// How often the streams are checked.
const POLL: Duration = Duration::from_millis(100);
/// How often the levels are printed.
const DISPLAY: Duration = Duration::from_millis(500);
/// A stream without a callback for this long is considered stopped.
const STALL: Duration = Duration::from_secs(1);
/// Sample rates tried first (ADR-0007), if both devices report them.
const PREFERRED_RATES: [u32; 3] = [48_000, 44_100, 96_000];
/// The input channel used as the MIC (the first one, ADR-0007).
const MIC_CHANNEL: usize = 0;

/// The open input and output streams and the statistics of their link.
struct Opened {
    input: InputStream,
    output: OutputStream,
    stats: Arc<LinkStats>,
}

/// Why one configuration could not be opened.
enum OpenError {
    /// The configuration is not supported; try the next candidate.
    Unsupported(BackendError),
    /// Anything else; other candidates would fail too, so give up.
    Failed(BackendError),
}

impl From<BackendError> for OpenError {
    fn from(e: BackendError) -> Self {
        match e {
            BackendError::UnsupportedConfig(_) => Self::Unsupported(e),
            _ => Self::Failed(e),
        }
    }
}

/// Runs `ink-mixer run`. Exits with 0 when it finishes, or 1 when the
/// streams cannot be opened or stop while running.
pub fn run(backend: &dyn AudioBackend, config: &Config, args: &RunArgs) -> ExitCode {
    warn!(
        "the microphone is played through the output as is; \
         use headphones or a headset (speakers cause feedback)"
    );

    let input = match resolve_device(
        "input",
        args.input.as_deref(),
        config.audio.input_device.as_ref(),
        backend.enumerate_input_devices(),
        || backend.default_input(),
    ) {
        Ok(device) => device,
        Err(e) => return fail(&e),
    };
    let output = match resolve_device(
        "output",
        args.output.as_deref(),
        config.audio.output_device.as_ref(),
        backend.enumerate_output_devices(),
        || backend.default_output(),
    ) {
        Ok(device) => device,
        Err(e) => return fail(&e),
    };

    let control = Arc::new(MixerControl::new());
    control.mic.set_gain_db(args.gain);
    control.master.set_gain_db(args.master);

    let opened = match open_streams(backend, &input, &output, &control) {
        Ok(opened) => opened,
        Err(e) => return fail(&e),
    };
    print_opened(&input, &opened.input, "input");
    print_opened(&output, &opened.output, "output");
    println!(
        "levels:   gain {:.1} dB, master {:.1} dB, {} s",
        control.mic.gain_db(),
        control.master.gain_db(),
        args.seconds
    );

    let result = monitor(&opened, &control, Duration::from_secs(args.seconds));
    print_summary(&opened);
    // Stop the input first, so nothing writes to the ring buffer, then the
    // output.
    let Opened { input, output, .. } = opened;
    drop(input);
    drop(output);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(message: &str) -> ExitCode {
    error!("{message}");
    ExitCode::FAILURE
}

/// Chooses the device: the argument, then the configuration, then the
/// default.
///
/// A requested ID that is not listed is an error. A configured ID that is
/// not listed gives a warning, and the default is used.
fn resolve_device(
    direction: &str,
    requested: Option<&str>,
    configured: Option<&DeviceId>,
    list: ink_backend::Result<Vec<DeviceInfo>>,
    default: impl FnOnce() -> ink_backend::Result<Option<DeviceInfo>>,
) -> Result<DeviceInfo, String> {
    let list = list.map_err(|e| format!("failed to list {direction} devices: {e}"))?;
    if let Some(id) = requested {
        return list
            .into_iter()
            .find(|d| d.id.as_str() == id)
            .ok_or(format!(
                "{direction} device not found: {id} (see `ink-mixer devices`)"
            ));
    }
    if let Some(id) = configured {
        match list.into_iter().find(|d| d.id == *id) {
            Some(device) => return Ok(device),
            None => warn!("configured {direction} device not found: {id}; using the default"),
        }
    }
    default()
        .map_err(|e| format!("failed to get the default {direction} device: {e}"))?
        .ok_or(format!("no default {direction} device"))
}

/// Opens both streams, trying the candidate configurations in order.
fn open_streams(
    backend: &dyn AudioBackend,
    input: &DeviceInfo,
    output: &DeviceInfo,
    control: &Arc<MixerControl>,
) -> Result<Opened, String> {
    let rates = rate_candidates(&input.sample_rates, &output.sample_rates);
    if rates.is_empty() {
        return Err(format!(
            "{} and {} have no sample rate in common",
            input.id, output.id
        ));
    }
    let in_channels = channel_candidates(&[1, 2], input.channels);
    let out_channels = channel_candidates(&[2, 1], output.channels);

    let mut last_error = None;
    for &rate in &rates {
        'output: for &out_ch in &out_channels {
            for &in_ch in &in_channels {
                debug!("trying {rate} Hz, input {in_ch} ch, output {out_ch} ch");
                match try_open(backend, input, output, control, rate, in_ch, out_ch) {
                    Ok(opened) => return Ok(opened),
                    Err((side, OpenError::Unsupported(e))) => {
                        debug!("{side} not supported: {e}");
                        last_error = Some(e);
                        if side == "output" {
                            continue 'output;
                        }
                    }
                    Err((side, OpenError::Failed(e))) => {
                        return Err(format!("failed to open the {side}: {e}"));
                    }
                }
            }
        }
    }
    Err(match last_error {
        Some(e) => format!("no working configuration found (last error: {e})"),
        None => "no working configuration found".to_string(),
    })
}

/// Opens one configuration: the output first, then the input, so that the
/// input does not run alone and overflow the ring buffer. On failure, returns
/// which side could not be opened.
fn try_open(
    backend: &dyn AudioBackend,
    input: &DeviceInfo,
    output: &DeviceInfo,
    control: &Arc<MixerControl>,
    rate: u32,
    in_channels: u16,
    out_channels: u16,
) -> Result<Opened, (&'static str, OpenError)> {
    let (mut writer, reader, stats) = audio_link(rate, MAX_FRAMES_PER_CALLBACK);
    let mut graph = MicGraph::new(
        MicrophoneSource::new(reader),
        Arc::clone(control),
        rate,
        MAX_FRAMES_PER_CALLBACK,
    );

    let out_config = StreamConfig {
        sample_rate: rate,
        channels: out_channels,
    };
    let out_ch = usize::from(out_channels);
    let output_stream = backend
        .open_output(
            &output.id,
            out_config,
            // Audio thread: only runs the graph.
            Box::new(move |out: &mut [f32]| graph.render(out, out_ch)),
        )
        .map_err(|e| ("output", e.into()))?;

    let in_config = StreamConfig {
        sample_rate: rate,
        channels: in_channels,
    };
    let in_ch = usize::from(in_channels);
    let input_stream = backend
        .open_input(
            &input.id,
            in_config,
            // Audio thread: only writes the first channel to the ring buffer.
            Box::new(move |samples: &[f32]| writer.push_channel(samples, in_ch, MIC_CHANNEL)),
        )
        .map_err(|e| ("input", e.into()))?;

    Ok(Opened {
        input: input_stream,
        output: output_stream,
        stats,
    })
}

/// Returns the sample rates to try: of the rates both devices report, 48000,
/// 44100, and 96000 first, then the rest in ascending order. Empty if there
/// is no common rate.
fn rate_candidates(input: &[u32], output: &[u32]) -> Vec<u32> {
    let common: Vec<u32> = input
        .iter()
        .copied()
        .filter(|r| output.contains(r))
        .collect();
    let mut rates: Vec<u32> = PREFERRED_RATES
        .into_iter()
        .filter(|r| common.contains(r))
        .collect();
    let mut rest: Vec<u32> = common
        .into_iter()
        .filter(|r| !PREFERRED_RATES.contains(r))
        .collect();
    rest.sort_unstable();
    rest.dedup();
    rates.extend(rest);
    rates
}

/// Returns the channel counts to try: `preferred` first, then the device
/// maximum, without duplicates and never above the maximum.
fn channel_candidates(preferred: &[u16], max: u16) -> Vec<u16> {
    let mut channels = Vec::new();
    for &ch in preferred.iter().chain(std::iter::once(&max)) {
        if ch >= 1 && ch <= max.max(1) && !channels.contains(&ch) {
            channels.push(ch);
        }
    }
    channels
}

/// Converts frames to milliseconds.
fn frames_to_ms(frames: f64, sample_rate: u32) -> f64 {
    frames * 1000.0 / f64::from(sample_rate)
}

/// Estimated latency in milliseconds: input block + average buffer + output
/// block (ADR-0007).
fn estimated_latency_ms(in_block: u32, average_fill: f64, out_block: u32, sample_rate: u32) -> f64 {
    frames_to_ms(
        f64::from(in_block) + average_fill + f64::from(out_block),
        sample_rate,
    )
}

/// Formats a level (linear) in dBFS; -60 dB and below show as `-inf`.
fn format_level(peak: f32) -> String {
    let db = gain_to_db(peak);
    if db > SILENCE_DB {
        format!("{db:6.1}")
    } else {
        "  -inf".to_string()
    }
}

fn print_opened<D>(device: &DeviceInfo, stream: &ink_backend::AudioStream<D>, side: &str) {
    let config = stream.config();
    println!(
        "{side:<9} {} ({}): {} Hz, {} ch, {}",
        device.name,
        device.id,
        config.sample_rate,
        config.channels,
        stream.sample_format()
    );
}

/// Watches the streams and prints the levels for `duration`. Returns an
/// error when either stream stops.
fn monitor(opened: &Opened, control: &MixerControl, duration: Duration) -> Result<(), String> {
    let start = Instant::now();
    let mut last_display = start;
    let mut watch_in = Watch::new();
    let mut watch_out = Watch::new();

    while start.elapsed() < duration {
        thread::sleep(POLL);
        watch_in.check("input", opened.input.error(), opened.input.callbacks())?;
        watch_out.check("output", opened.output.error(), opened.output.callbacks())?;

        if last_display.elapsed() >= DISPLAY {
            last_display = Instant::now();
            let rate = opened.output.config().sample_rate;
            println!(
                "MIC {} dBFS | MASTER {} dBFS | buffer {:5.1} ms | under {} over {}",
                format_level(control.mic.meter().take()),
                format_level(control.master.meter().take()),
                frames_to_ms(f64::from(opened.stats.fill_frames()), rate),
                opened.stats.underruns(),
                opened.stats.overruns()
            );
        }
    }
    Ok(())
}

/// Watches one stream: its error and whether its callbacks keep coming.
struct Watch {
    callbacks: u64,
    progress: Instant,
}

impl Watch {
    fn new() -> Self {
        Self {
            callbacks: 0,
            progress: Instant::now(),
        }
    }

    fn check(
        &mut self,
        side: &str,
        error: Option<ink_backend::StreamError>,
        callbacks: u64,
    ) -> Result<(), String> {
        if let Some(e) = error {
            return Err(format!("{side} stream stopped: {e}"));
        }
        if callbacks != self.callbacks {
            self.callbacks = callbacks;
            self.progress = Instant::now();
        } else if self.progress.elapsed() >= STALL {
            return Err(format!(
                "{side} stopped: no callback for {} s (device unplugged?)",
                STALL.as_secs()
            ));
        }
        Ok(())
    }
}

/// Prints the values that ADR-0007 asks to record.
fn print_summary(opened: &Opened) {
    let rate = opened.output.config().sample_rate;
    let stats = &opened.stats;
    let in_block = opened.input.callback_frames();
    let out_block = opened.output.callback_frames();
    let target = stats.target_frames();
    let average = stats.average_fill_frames();
    println!("summary:");
    println!("  sample rate:     {rate} Hz");
    println!(
        "  callback:        input {in_block} frames ({:.1} ms), output {out_block} frames ({:.1} ms)",
        frames_to_ms(f64::from(in_block), rate),
        frames_to_ms(f64::from(out_block), rate)
    );
    println!(
        "  target:          {target} frames ({:.1} ms)",
        frames_to_ms(f64::from(target), rate)
    );
    println!(
        "  average buffer:  {average:.0} frames ({:.1} ms)",
        frames_to_ms(average, rate)
    );
    println!(
        "  latency (est.):  {:.1} ms (input block + average buffer + output block)",
        estimated_latency_ms(in_block, average, out_block, rate)
    );
    println!(
        "  underruns:       {}\n  overruns:        {}",
        stats.underruns(),
        stats.overruns()
    );
    println!(
        "  backend xruns:   input {}, output {}",
        opened.input.xruns(),
        opened.output.xruns()
    );
    for (side, denied) in [
        ("input", opened.input.realtime_denied()),
        ("output", opened.output.realtime_denied()),
    ] {
        if denied {
            warn!("realtime scheduling was denied for the {side} stream");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_prefer_48k_then_44_1k_then_96k() {
        let all = [8_000, 44_100, 48_000, 96_000, 192_000];
        assert_eq!(
            rate_candidates(&all, &all),
            vec![48_000, 44_100, 96_000, 8_000, 192_000]
        );
    }

    #[test]
    fn rates_must_be_common_to_both_devices() {
        assert_eq!(
            rate_candidates(&[44_100, 48_000], &[44_100, 96_000]),
            vec![44_100]
        );
        assert_eq!(
            rate_candidates(&[32_000, 22_050], &[22_050, 32_000]),
            vec![22_050, 32_000]
        );
        assert_eq!(rate_candidates(&[48_000], &[44_100]), Vec::<u32>::new());
        assert_eq!(rate_candidates(&[], &[48_000]), Vec::<u32>::new());
    }

    #[test]
    fn input_channels_try_mono_first() {
        assert_eq!(channel_candidates(&[1, 2], 64), vec![1, 2, 64]);
        assert_eq!(channel_candidates(&[1, 2], 2), vec![1, 2]);
        assert_eq!(channel_candidates(&[1, 2], 1), vec![1]);
    }

    #[test]
    fn output_channels_try_stereo_first() {
        assert_eq!(channel_candidates(&[2, 1], 64), vec![2, 1, 64]);
        assert_eq!(channel_candidates(&[2, 1], 2), vec![2, 1]);
        assert_eq!(channel_candidates(&[2, 1], 1), vec![1]);
        // A device that reports no channels still gets one attempt with 1.
        assert_eq!(channel_candidates(&[2, 1], 0), vec![1]);
    }

    #[test]
    fn latency_adds_blocks_and_average_buffer() {
        assert_eq!(frames_to_ms(480.0, 48_000), 10.0);
        // 1024 + 512 + 1024 frames at 48 kHz.
        let ms = estimated_latency_ms(1024, 512.0, 1024, 48_000);
        assert!((ms - 53.333).abs() < 0.01, "{ms}");
    }

    #[test]
    fn levels_below_silence_show_as_minus_infinity() {
        assert_eq!(format_level(1.0), "   0.0");
        assert_eq!(format_level(0.5), "  -6.0");
        assert_eq!(format_level(0.0), "  -inf");
        assert_eq!(format_level(0.0005), "  -inf");
    }

    #[test]
    fn requested_device_must_be_listed() {
        let mic = DeviceInfo {
            id: DeviceId::new("test:mic"),
            name: "Mic".to_string(),
            channels: 1,
            sample_rates: vec![48_000],
        };
        let list = || Ok(vec![mic.clone()]);
        let no_default = || Ok(None);

        let found = resolve_device("input", Some("test:mic"), None, list(), no_default);
        assert_eq!(found.unwrap().id, mic.id);

        let missing = resolve_device("input", Some("test:none"), None, list(), no_default);
        assert!(missing.unwrap_err().contains("not found"));

        // A configured device that is gone falls back to the default.
        let gone = DeviceId::new("test:gone");
        let fallback = resolve_device("input", None, Some(&gone), list(), || Ok(Some(mic.clone())));
        assert_eq!(fallback.unwrap().id, mic.id);

        let none = resolve_device("input", None, None, list(), no_default);
        assert!(none.unwrap_err().contains("no default"));
    }
}
