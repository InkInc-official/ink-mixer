//! An audio session: the input and output streams that play
//! MIC (+ SE) -> Gain -> MASTER -> output (ADR-0007, ADR-0009). Used by
//! `ink-mixer run` and by the window.
//!
//! The output is opened first, then the input, and the two are connected
//! with a ring buffer (`audio_link`). The output callback runs a `MicGraph`
//! that has the sound effects (SE) channel; the session keeps the
//! [`SeController`] for it.
//! The sample rate and channel counts are chosen by actually opening the
//! devices, because the numbers in `DeviceInfo` cannot be trusted (ALSA
//! `sysdefault` reports that it supports everything).

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ink_backend::{
    AudioBackend, BackendError, DeviceId, DeviceInfo, InputStream, MAX_FRAMES_PER_CALLBACK,
    OutputStream, StreamConfig, StreamError,
};
use ink_core::Config;
use ink_core::graph::{
    LinkStats, LinkWriter, MicGraph, MicrophoneSource, MixerControl, SeController, audio_link,
    se_channel,
};
use tracing::{debug, warn};

/// A stream without a callback for this long is considered stopped.
const STALL: Duration = Duration::from_secs(1);
/// Sample rates tried first (ADR-0007), if both devices report them.
const PREFERRED_RATES: [u32; 3] = [48_000, 44_100, 96_000];
/// The input channel used as the MIC (the first one, ADR-0007).
const MIC_CHANNEL: usize = 0;

/// One side of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Input,
    Output,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }

    fn name_ja(self) -> &'static str {
        match self {
            Self::Input => "入力",
            Self::Output => "出力",
        }
    }
}

/// Why a running session has to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// The backend reported a fatal error of the stream.
    Stream { side: Side, error: StreamError },
    /// The stream delivered no callback for [`STALL`] (some systems report
    /// nothing when a device is unplugged).
    Stalled { side: Side },
}

impl SessionError {
    /// A one-line explanation in Japanese for the window. The English
    /// [`Display`](fmt::Display) text is meant for logs and the CLI.
    pub fn message_ja(&self) -> String {
        match self {
            Self::Stream { side, error } => {
                let side = side.name_ja();
                match error {
                    StreamError::DeviceUnavailable => {
                        format!("{side}のデバイスが使えなくなりました（USB を抜きましたか？）")
                    }
                    StreamError::PermissionDenied => format!(
                        "{side}のデバイスへのアクセスが拒否されました（OS のプライバシー設定を確認してください）"
                    ),
                    _ => format!("{side}のストリームが止まりました"),
                }
            }
            Self::Stalled { side } => format!(
                "{}が止まりました（{} 秒以上、音が来ません）",
                side.name_ja(),
                STALL.as_secs()
            ),
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream { side, error } => write!(f, "{} stream stopped: {error}", side.name()),
            Self::Stalled { side } => write!(
                f,
                "{} stopped: no callback for {} s (device unplugged?)",
                side.name(),
                STALL.as_secs()
            ),
        }
    }
}

impl std::error::Error for SessionError {}

/// Lets `?` turn the error into the `String` errors of the CLI.
impl From<SessionError> for String {
    fn from(e: SessionError) -> Self {
        e.to_string()
    }
}

/// Open input and output streams playing the MIC through the mixer.
///
/// Dropping it stops both streams. The fields drop in order, so the input
/// stops first and nothing writes to the ring buffer while the output stops.
/// The SE controller drops after the output stream, so the player (in the
/// output callback) is gone before the queues it shares are released.
pub struct AudioSession {
    input: InputStream,
    output: OutputStream,
    se: SeController,
    stats: Arc<LinkStats>,
    input_device: DeviceInfo,
    output_device: DeviceInfo,
    watch_input: Watch,
    watch_output: Watch,
}

impl AudioSession {
    /// Opens `input` and `output`, trying the candidate configurations in
    /// order, and starts playing through a graph that follows `control`.
    pub fn start(
        backend: &dyn AudioBackend,
        input: DeviceInfo,
        output: DeviceInfo,
        control: &Arc<MixerControl>,
    ) -> Result<Self, String> {
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
                    match try_open(backend, &input, &output, control, rate, in_ch, out_ch) {
                        Ok(Opened {
                            input: input_stream,
                            output: output_stream,
                            stats,
                            se,
                        }) => {
                            let now = Instant::now();
                            return Ok(Self {
                                input: input_stream,
                                output: output_stream,
                                se,
                                stats,
                                input_device: input,
                                output_device: output,
                                watch_input: Watch::new(now),
                                watch_output: Watch::new(now),
                            });
                        }
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

    /// Checks both streams at `now`: an error, or no callback for a second
    /// (some systems report nothing when a device is unplugged).
    pub fn check(&mut self, now: Instant) -> Result<(), SessionError> {
        self.watch_input
            .check(Side::Input, self.input.error(), self.input.callbacks(), now)?;
        self.watch_output.check(
            Side::Output,
            self.output.error(),
            self.output.callbacks(),
            now,
        )
    }

    /// Returns the input stream.
    pub fn input(&self) -> &InputStream {
        &self.input
    }

    /// Returns the output stream.
    pub fn output(&self) -> &OutputStream {
        &self.output
    }

    /// Returns the control side of the sound effects (SE) player.
    pub fn se(&self) -> &SeController {
        &self.se
    }

    /// Returns the control side of the sound effects (SE) player, to put
    /// sounds in its slots and play them. Call it from the control thread
    /// only (the main thread of the CLI, the UI thread of the GUI).
    pub fn se_mut(&mut self) -> &mut SeController {
        &mut self.se
    }

    /// Returns the statistics of the ring buffer between the streams.
    pub fn stats(&self) -> &LinkStats {
        &self.stats
    }

    /// Returns the input device.
    pub fn input_device(&self) -> &DeviceInfo {
        &self.input_device
    }

    /// Returns the output device.
    pub fn output_device(&self) -> &DeviceInfo {
        &self.output_device
    }

    /// Returns the sample rate both streams run at.
    pub fn sample_rate(&self) -> u32 {
        self.output.config().sample_rate
    }
}

/// Chooses the input and output devices: the requested IDs, then the
/// configuration, then the defaults (see [`resolve_device`]).
pub fn choose_devices(
    backend: &dyn AudioBackend,
    config: &Config,
    input: Option<&str>,
    output: Option<&str>,
) -> Result<(DeviceInfo, DeviceInfo), String> {
    let input = resolve_device(
        "input",
        input,
        config.audio.input_device.as_ref(),
        backend.enumerate_input_devices(),
        || backend.default_input(),
    )?;
    let output = resolve_device(
        "output",
        output,
        config.audio.output_device.as_ref(),
        backend.enumerate_output_devices(),
        || backend.default_output(),
    )?;
    Ok((input, output))
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

/// What [`try_open`] opened.
struct Opened {
    input: InputStream,
    output: OutputStream,
    stats: Arc<LinkStats>,
    se: SeController,
}

/// The parts of the audio path for `rate`: the writer for the input
/// callback, the graph for the output callback, the statistics of the ring
/// buffer, and the control side of the SE player.
///
/// The graph and the SE player are made with the same `max_frames`
/// ([`MAX_FRAMES_PER_CALLBACK`]), because the graph renders the SE player in
/// parts of at most that many frames.
fn build_graph(
    control: &Arc<MixerControl>,
    rate: u32,
) -> (LinkWriter, MicGraph, Arc<LinkStats>, SeController) {
    let (writer, reader, stats) = audio_link(rate, MAX_FRAMES_PER_CALLBACK);
    let (se, player) = se_channel(rate, MAX_FRAMES_PER_CALLBACK);
    let graph = MicGraph::new(
        MicrophoneSource::new(reader),
        Arc::clone(control),
        rate,
        MAX_FRAMES_PER_CALLBACK,
    )
    .with_se(player);
    (writer, graph, stats, se)
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
    let (mut writer, mut graph, stats, se) = build_graph(control, rate);

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
        se,
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

/// Watches one stream: its error and whether its callbacks keep coming.
#[derive(Debug)]
struct Watch {
    callbacks: u64,
    progress: Instant,
}

impl Watch {
    fn new(now: Instant) -> Self {
        Self {
            callbacks: 0,
            progress: now,
        }
    }

    fn check(
        &mut self,
        side: Side,
        error: Option<StreamError>,
        callbacks: u64,
        now: Instant,
    ) -> Result<(), SessionError> {
        if let Some(error) = error {
            return Err(SessionError::Stream { side, error });
        }
        if callbacks != self.callbacks {
            self.callbacks = callbacks;
            self.progress = now;
        } else if now.saturating_duration_since(self.progress) >= STALL {
            return Err(SessionError::Stalled { side });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ink_core::graph::{SePolicy, SoundData};

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
    fn the_graph_plays_the_se_through_the_controller() {
        let control = Arc::new(MixerControl::new());
        let (_writer, mut graph, _stats, mut se) = build_graph(&control, 48_000);
        // More frames than one callback part (MAX_FRAMES_PER_CALLBACK), so
        // the graph splits the block and the SE player is called in parts.
        let frames = MAX_FRAMES_PER_CALLBACK * 2 + 100;
        let sound = SoundData::new(48_000, vec![0.25; frames + 50], vec![-0.25; frames + 50]);
        se.set_sound(0, Arc::new(sound)).unwrap();
        se.play(0, 1.0, SePolicy::Overlap).unwrap();

        let mut out = vec![0.0_f32; frames * 2];
        graph.render(&mut out, 2);
        assert!(out.chunks(2).all(|frame| frame == [0.25, -0.25]));
        assert!(control.se.meter().take() > 0.24);
    }

    #[test]
    fn the_graph_without_a_sound_is_silent() {
        let control = Arc::new(MixerControl::new());
        let (_writer, mut graph, _stats, se) = build_graph(&control, 44_100);
        let mut out = vec![-1.0_f32; 2 * 480];
        graph.render(&mut out, 2);
        assert!(out.iter().all(|&sample| sample == 0.0));
        assert_eq!(se.sample_rate(), 44_100);
        assert_eq!(se.outstanding(), 0);
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

    #[test]
    fn watch_reports_a_stream_error() {
        let t0 = Instant::now();
        let mut watch = Watch::new(t0);
        let result = watch.check(Side::Input, Some(StreamError::DeviceUnavailable), 5, t0);
        assert_eq!(
            result.unwrap_err(),
            SessionError::Stream {
                side: Side::Input,
                error: StreamError::DeviceUnavailable
            }
        );
    }

    #[test]
    fn watch_accepts_callbacks_that_keep_coming() {
        let t0 = Instant::now();
        let mut watch = Watch::new(t0);
        for k in 1..=30u64 {
            let now = t0 + Duration::from_millis(100 * k);
            assert!(watch.check(Side::Output, None, k, now).is_ok());
        }
    }

    #[test]
    fn watch_reports_callbacks_that_stopped_for_a_second() {
        let t0 = Instant::now();
        let mut watch = Watch::new(t0);
        assert!(watch.check(Side::Output, None, 10, t0).is_ok());
        let soon = t0 + Duration::from_millis(900);
        assert!(watch.check(Side::Output, None, 10, soon).is_ok());
        let late = t0 + Duration::from_secs(1);
        let result = watch.check(Side::Output, None, 10, late);
        assert_eq!(
            result.unwrap_err(),
            SessionError::Stalled { side: Side::Output }
        );
    }

    #[test]
    fn errors_display_in_english_for_logs_and_the_cli() {
        let lost = SessionError::Stream {
            side: Side::Input,
            error: StreamError::DeviceUnavailable,
        };
        assert_eq!(
            lost.to_string(),
            "input stream stopped: the audio device is no longer available"
        );
        let stalled = SessionError::Stalled { side: Side::Output };
        assert_eq!(
            stalled.to_string(),
            "output stopped: no callback for 1 s (device unplugged?)"
        );
        assert_eq!(String::from(stalled), stalled.to_string());
    }

    #[test]
    fn errors_have_a_japanese_line_for_each_kind() {
        let stream = |side, error| SessionError::Stream { side, error };
        assert_eq!(
            stream(Side::Input, StreamError::DeviceUnavailable).message_ja(),
            "入力のデバイスが使えなくなりました（USB を抜きましたか？）"
        );
        assert!(
            stream(Side::Output, StreamError::PermissionDenied)
                .message_ja()
                .starts_with("出力のデバイスへのアクセスが拒否されました")
        );
        assert_eq!(
            stream(Side::Output, StreamError::Other).message_ja(),
            "出力のストリームが止まりました"
        );
        assert_eq!(
            SessionError::Stalled { side: Side::Input }.message_ja(),
            "入力が止まりました（1 秒以上、音が来ません）"
        );
    }
}
