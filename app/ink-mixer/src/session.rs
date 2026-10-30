//! An audio session: the input and output streams that play
//! MIC (+ BGM) (+ SE) -> Gain -> MASTER -> output (ADR-0007, ADR-0009). Used
//! by `ink-mixer run` and by the window.
//!
//! The output is opened first, then the input, and the two are connected
//! with a ring buffer (`audio_link`). The output callback runs a `MicGraph`
//! that has the sound effects (SE) channel; the session keeps the
//! [`SeController`] for it. With [`SessionOptions::bgm`], the graph also has
//! the background music (BGM) channel, and the session keeps a [`Bgm`] (the
//! controller and the worker thread that fills its ring buffer).
//! The sample rate and channel counts are chosen by actually opening the
//! devices, because the numbers in `DeviceInfo` cannot be trusted (ALSA
//! `sysdefault` reports that it supports everything).

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ink_backend::{
    AudioBackend, AudioStream, BackendError, DeviceId, DeviceInfo, ErrorDetail, Failure,
    InputStream, MAX_FRAMES_PER_CALLBACK, OutputStream, StreamConfig, StreamError,
};
use ink_core::Config;
use ink_core::graph::{
    BgmController, BgmFeeder, LinkStats, LinkWriter, MicGraph, MicrophoneSource, MixerControl,
    SeController, audio_link, bgm_channel, se_channel,
};
use tracing::{debug, warn};

use crate::bgm_worker::Bgm;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The backend reported a fatal error of the stream. `detail` is what the
    /// backend reported and `note` its own message, when it gave one (both
    /// for the log line). `detail` is `Some` for every error of a real
    /// stream.
    Stream {
        side: Side,
        error: StreamError,
        detail: Option<ErrorDetail>,
        note: Option<String>,
    },
    /// The stream delivered no callback for [`STALL`] (some systems report
    /// nothing when a device is unplugged).
    Stalled { side: Side },
}

impl SessionError {
    /// A one-line explanation in Japanese for the window. The English
    /// [`Display`](fmt::Display) text is meant for logs and the CLI.
    pub fn message_ja(&self) -> String {
        match self {
            Self::Stream { side, error, .. } => {
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
            Self::Stream {
                side,
                error,
                detail,
                note,
            } => {
                write!(f, "{} stream stopped: {error}", side.name())?;
                match (detail, note) {
                    (Some(detail), Some(note)) => write!(f, " ({detail}: {note})"),
                    (Some(detail), None) => write!(f, " ({detail})"),
                    (None, Some(note)) => write!(f, " ({note})"),
                    (None, None) => Ok(()),
                }
            }
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

/// What a session is made of besides the MIC and the sound effects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionOptions {
    /// Add the background music (BGM) channel and its worker thread. Without
    /// it nothing of the BGM is made: no ring buffer, no player, no thread.
    pub bgm: bool,
}

/// Open input and output streams playing the MIC through the mixer.
///
/// Dropping it stops both streams. The fields drop in the order they are
/// declared, and the order matters:
///
/// 1. `input` stops first, so nothing writes to the ring buffer while the
///    output stops.
/// 2. `output` stops; the graph in its callback, with the SE player and the
///    BGM player, is dropped. The BGM player hands the reading end of its
///    ring buffer to the controller (it frees nothing).
/// 3. `se` drops after the output stream, so the SE player is gone before
///    the queues it shares are released.
/// 4. `bgm` drops after the output stream, for the same reason: its worker
///    thread is joined first, and then its controller is dropped, which
///    frees the ring buffer on the control side, never on the audio thread.
/// 5. `stats` and the rest hold nothing that the streams use.
///
/// Do not reorder the fields without reading `ink-core/tests/realtime_alloc.rs`
/// (the tests of the drop orders).
pub struct AudioSession {
    input: InputStream,
    output: OutputStream,
    se: SeController,
    bgm: Option<Bgm>,
    stats: Arc<LinkStats>,
    input_device: DeviceInfo,
    output_device: DeviceInfo,
    watch_input: Watch,
    watch_output: Watch,
}

impl AudioSession {
    /// Opens `input` and `output`, trying the candidate configurations in
    /// order, and starts playing through a graph that follows `control`. With
    /// `options.bgm` the session also has the BGM (see [`SessionOptions`]).
    ///
    /// # Errors
    ///
    /// Fails if the devices have no sample rate in common, if no candidate
    /// configuration can be opened, or if the BGM worker thread cannot be
    /// started. The worker is started before the streams are opened, so
    /// nothing is playing when it fails.
    pub fn start_with(
        backend: &dyn AudioBackend,
        input: DeviceInfo,
        output: DeviceInfo,
        control: &Arc<MixerControl>,
        options: SessionOptions,
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
                    let attempt = Attempt {
                        rate,
                        in_channels: in_ch,
                        out_channels: out_ch,
                        bgm: options.bgm,
                    };
                    match try_open(backend, &input, &output, control, attempt) {
                        Ok(Opened {
                            input: input_stream,
                            output: output_stream,
                            stats,
                            se,
                            bgm,
                        }) => {
                            let now = Instant::now();
                            return Ok(Self {
                                input: input_stream,
                                output: output_stream,
                                se,
                                bgm,
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
                        Err((_, OpenError::BgmWorker(e))) => {
                            return Err(format!("failed to start the BGM worker: {e}"));
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
            .check_stream(Side::Input, &self.input, now)?;
        self.watch_output
            .check_stream(Side::Output, &self.output, now)
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

    /// Returns the background music (BGM) player, if the session was started
    /// with [`SessionOptions::bgm`]. Its methods take `&self`, so it can be
    /// used from the control thread (the main thread of the CLI) without a
    /// mutable borrow.
    pub fn bgm(&self) -> Option<&Bgm> {
        self.bgm.as_ref()
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
    /// The BGM worker thread could not be started.
    BgmWorker(io::Error),
}

impl From<BackendError> for OpenError {
    fn from(e: BackendError) -> Self {
        match e {
            BackendError::UnsupportedConfig(_) => Self::Unsupported(e),
            _ => Self::Failed(e),
        }
    }
}

/// What [`try_open`] opened. The fields drop in the order of
/// [`AudioSession`]: the streams first, then the controllers.
struct Opened {
    input: InputStream,
    output: OutputStream,
    se: SeController,
    bgm: Option<Bgm>,
    stats: Arc<LinkStats>,
}

/// One configuration to try.
#[derive(Debug, Clone, Copy)]
struct Attempt {
    rate: u32,
    in_channels: u16,
    out_channels: u16,
    /// Whether the graph has the BGM channel.
    bgm: bool,
}

/// The parts of the audio path for one rate.
struct Built {
    /// For the input callback.
    writer: LinkWriter,
    /// For the output callback.
    graph: MicGraph,
    /// The statistics of the ring buffer between the streams.
    stats: Arc<LinkStats>,
    /// The control side of the SE player.
    se: SeController,
    /// The control side and the worker side of the BGM player, if the graph
    /// has the BGM channel.
    bgm: Option<(BgmController, BgmFeeder)>,
}

/// The parts of the audio path for `rate`. With `bgm`, the graph also has
/// the BGM channel; without it, no BGM part is made at all.
///
/// The graph, the SE player and the BGM player are made with the same
/// `max_frames` ([`MAX_FRAMES_PER_CALLBACK`]), because the graph renders them
/// in parts of at most that many frames.
fn build_graph(control: &Arc<MixerControl>, rate: u32, bgm: bool) -> Built {
    let (writer, reader, stats) = audio_link(rate, MAX_FRAMES_PER_CALLBACK);
    let (se, player) = se_channel(rate, MAX_FRAMES_PER_CALLBACK);
    let graph = MicGraph::new(
        MicrophoneSource::new(reader),
        Arc::clone(control),
        rate,
        MAX_FRAMES_PER_CALLBACK,
    )
    .with_se(player);
    let (graph, bgm) = if bgm {
        let (controller, feeder, bgm_player) = bgm_channel(rate, MAX_FRAMES_PER_CALLBACK);
        (graph.with_bgm(bgm_player), Some((controller, feeder)))
    } else {
        (graph, None)
    };
    Built {
        writer,
        graph,
        stats,
        se,
        bgm,
    }
}

/// Opens one configuration: the BGM worker (if there is a BGM), then the
/// output, then the input, so that the input does not run alone and overflow
/// the ring buffer. On failure, returns which side could not be opened.
///
/// The worker thread is started before any stream is opened. The locals drop
/// in the reverse order of their declaration, so when a stream fails to open
/// the streams that were opened go first and the BGM controller goes last,
/// which is the order of [`AudioSession`]. If the thread cannot be started,
/// no stream exists yet and nothing can be playing.
fn try_open(
    backend: &dyn AudioBackend,
    input: &DeviceInfo,
    output: &DeviceInfo,
    control: &Arc<MixerControl>,
    attempt: Attempt,
) -> Result<Opened, (&'static str, OpenError)> {
    let Attempt {
        rate,
        in_channels,
        out_channels,
        bgm,
    } = attempt;
    let Built {
        mut writer,
        mut graph,
        stats,
        se,
        bgm: bgm_parts,
    } = build_graph(control, rate, bgm);

    let bgm = match bgm_parts {
        Some((controller, feeder)) => {
            Some(Bgm::start(controller, feeder).map_err(|e| ("bgm", OpenError::BgmWorker(e)))?)
        }
        None => None,
    };

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
        se,
        bgm,
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

    /// Checks one stream. Both sides go through here, and the failure is
    /// read once, with its detail and message, so nothing of it is left out
    /// for the input or the output.
    fn check_stream<D>(
        &mut self,
        side: Side,
        stream: &AudioStream<D>,
        now: Instant,
    ) -> Result<(), SessionError> {
        self.check(side, stream.failure(), stream.callbacks(), now)
    }

    fn check(
        &mut self,
        side: Side,
        failure: Option<Failure>,
        callbacks: u64,
        now: Instant,
    ) -> Result<(), SessionError> {
        if let Some(Failure {
            error,
            detail,
            note,
        }) = failure
        {
            return Err(SessionError::Stream {
                side,
                error,
                detail: Some(detail),
                note,
            });
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
    use ink_core::graph::{SePolicy, SoundData, bgm_prime_frames, bgm_ring_frames};

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
        let Built {
            mut graph, mut se, ..
        } = build_graph(&control, 48_000, false);
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
        let Built { mut graph, se, .. } = build_graph(&control, 44_100, false);
        let mut out = vec![-1.0_f32; 2 * 480];
        graph.render(&mut out, 2);
        assert!(out.iter().all(|&sample| sample == 0.0));
        assert_eq!(se.sample_rate(), 44_100);
        assert_eq!(se.outstanding(), 0);
    }

    /// Writes `frames` frames of `value` on both channels, as the worker does.
    fn feed(feeder: &mut BgmFeeder, value: f32, frames: usize) {
        let samples = vec![value; frames];
        assert_eq!(feeder.write(&samples, &samples), frames);
    }

    #[test]
    fn a_session_has_no_bgm_unless_asked_for() {
        assert!(!SessionOptions::default().bgm);
        assert!(SessionOptions { bgm: true }.bgm);
    }

    #[test]
    fn the_graph_without_bgm_makes_no_bgm_part() {
        let control = Arc::new(MixerControl::new());
        let built = build_graph(&control, 48_000, false);
        assert!(built.bgm.is_none());
        let mut graph = built.graph;
        let mut out = vec![-1.0_f32; 2 * 480];
        graph.render(&mut out, 2);
        assert!(out.iter().all(|&sample| sample == 0.0));
        assert_eq!(control.bgm.meter().take(), 0.0);
    }

    #[test]
    fn the_graph_plays_the_bgm_through_the_controller() {
        let control = Arc::new(MixerControl::new());
        let Built { mut graph, bgm, .. } = build_graph(&control, 48_000, true);
        let (controller, mut feeder) = bgm.expect("the BGM parts");
        feed(&mut feeder, 0.25, 12_000);
        controller.play();

        // More frames than one callback part, so the graph splits the block
        // and the BGM player is called in parts.
        let frames = MAX_FRAMES_PER_CALLBACK * 2 + 100;
        let mut out = vec![0.0_f32; frames * 2];
        graph.render(&mut out, 2);

        // After the priming and the fade-in the song is heard as it is.
        for frame in out.chunks(2).skip(5_000) {
            assert!((frame[0] - 0.25).abs() < 1e-6, "{frame:?}");
            assert!((frame[1] - 0.25).abs() < 1e-6, "{frame:?}");
        }
        assert!(control.bgm.meter().take() > 0.24);
        assert!(controller.stats().played_frames > 0);
    }

    #[test]
    fn the_graph_mixes_the_bgm_and_the_se() {
        let control = Arc::new(MixerControl::new());
        let Built {
            mut graph,
            mut se,
            bgm,
            ..
        } = build_graph(&control, 48_000, true);
        let (controller, mut feeder) = bgm.expect("the BGM parts");
        feed(&mut feeder, 0.25, 12_000);
        controller.play();
        let frames = MAX_FRAMES_PER_CALLBACK * 2 + 100;
        let sound = SoundData::new(48_000, vec![0.25; frames + 50], vec![-0.25; frames + 50]);
        se.set_sound(0, Arc::new(sound)).unwrap();
        se.play(0, 1.0, SePolicy::Overlap).unwrap();

        let mut out = vec![0.0_f32; frames * 2];
        graph.render(&mut out, 2);

        // BGM (0.25, 0.25) + SE (0.25, -0.25).
        for frame in out.chunks(2).skip(5_000) {
            assert!((frame[0] - 0.5).abs() < 1e-6, "{frame:?}");
            assert!(frame[1].abs() < 1e-6, "{frame:?}");
        }
        assert!(control.bgm.meter().take() > 0.24);
        assert!(control.se.meter().take() > 0.24);
    }

    #[test]
    fn the_bgm_parts_use_the_sample_rate_of_the_graph() {
        let control = Arc::new(MixerControl::new());
        let (controller, feeder) = build_graph(&control, 44_100, true).bgm.unwrap();
        assert_eq!(controller.sample_rate(), 44_100);
        assert_eq!(feeder.sample_rate(), 44_100);
        assert_eq!(controller.ring_capacity_frames(), 44_100);
    }

    #[test]
    fn the_bgm_ring_buffer_and_priming_for_the_block_size_of_the_product() {
        // (rate, ring frames, ring bytes, priming frames)
        let cases = [
            (44_100_u32, 44_100_usize, 352_800_usize, 4_537_usize),
            (48_000, 48_000, 384_000, 4_800),
            (96_000, 96_000, 768_000, 9_600),
        ];
        for (rate, ring, bytes, prime) in cases {
            assert_eq!(bgm_ring_frames(rate, MAX_FRAMES_PER_CALLBACK), ring);
            assert_eq!(
                bgm_ring_frames(rate, MAX_FRAMES_PER_CALLBACK) * 2 * size_of::<f32>(),
                bytes
            );
            assert_eq!(bgm_prime_frames(rate, MAX_FRAMES_PER_CALLBACK), prime);
            // The ring buffer holds the priming and one more block.
            assert!(ring >= prime + MAX_FRAMES_PER_CALLBACK, "{rate}");
        }
    }

    /// A backend whose streams cannot be opened, with the error it gives.
    struct Refusing(fn() -> BackendError);

    impl AudioBackend for Refusing {
        fn enumerate_input_devices(&self) -> ink_backend::Result<Vec<DeviceInfo>> {
            Ok(Vec::new())
        }

        fn enumerate_output_devices(&self) -> ink_backend::Result<Vec<DeviceInfo>> {
            Ok(Vec::new())
        }

        fn default_input(&self) -> ink_backend::Result<Option<DeviceInfo>> {
            Ok(None)
        }

        fn default_output(&self) -> ink_backend::Result<Option<DeviceInfo>> {
            Ok(None)
        }

        fn open_input(
            &self,
            _device: &DeviceId,
            _config: StreamConfig,
            _callback: ink_backend::InputCallback,
        ) -> ink_backend::Result<InputStream> {
            Err((self.0)())
        }

        fn open_output(
            &self,
            _device: &DeviceId,
            _config: StreamConfig,
            _callback: ink_backend::OutputCallback,
        ) -> ink_backend::Result<OutputStream> {
            Err((self.0)())
        }
    }

    #[test]
    fn a_session_that_cannot_open_drops_the_bgm_parts_cleanly() {
        let device = |id: &str| DeviceInfo {
            id: DeviceId::new(id),
            name: id.to_string(),
            channels: 2,
            sample_rates: vec![48_000, 44_100],
        };
        let control = Arc::new(MixerControl::new());
        type MakeError = fn() -> BackendError;
        let cases: [(MakeError, &str); 2] = [
            // Every configuration is tried: a worker is started and dropped
            // for each of them.
            (
                || BackendError::UnsupportedConfig("fake".to_string()),
                "no working configuration found",
            ),
            (
                || BackendError::Other("fake".to_string()),
                "failed to open the output",
            ),
        ];
        for (error, message) in cases {
            for bgm in [false, true] {
                let start = Instant::now();
                let result = AudioSession::start_with(
                    &Refusing(error),
                    device("in"),
                    device("out"),
                    &control,
                    SessionOptions { bgm },
                );
                let text = result.err().expect("the open should fail");
                assert!(text.contains(message), "{text}");
                assert!(
                    start.elapsed() < Duration::from_secs(1),
                    "the BGM worker was not joined: {:?}",
                    start.elapsed()
                );
            }
        }
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

    fn failure(error: StreamError, detail: ErrorDetail, note: Option<&str>) -> Failure {
        Failure {
            error,
            detail,
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn watch_reports_a_stream_error() {
        let t0 = Instant::now();
        let mut watch = Watch::new(t0);
        let result = watch.check(
            Side::Input,
            Some(failure(
                StreamError::DeviceUnavailable,
                ErrorDetail::DeviceNotAvailable,
                None,
            )),
            5,
            t0,
        );
        assert_eq!(
            result.unwrap_err(),
            SessionError::Stream {
                side: Side::Input,
                error: StreamError::DeviceUnavailable,
                detail: Some(ErrorDetail::DeviceNotAvailable),
                note: None,
            }
        );
    }

    #[test]
    fn watch_carries_the_detail_and_the_message_for_both_sides() {
        let t0 = Instant::now();
        for side in [Side::Input, Side::Output] {
            let mut watch = Watch::new(t0);
            let result = watch.check(
                side,
                Some(failure(
                    StreamError::Other,
                    ErrorDetail::BackendError,
                    Some("snd_pcm_readi: Input/output error"),
                )),
                5,
                t0,
            );
            assert_eq!(
                result.unwrap_err(),
                SessionError::Stream {
                    side,
                    error: StreamError::Other,
                    detail: Some(ErrorDetail::BackendError),
                    note: Some("snd_pcm_readi: Input/output error".to_string()),
                }
            );
        }
    }

    #[test]
    fn watch_passes_on_a_failure_without_a_message_as_it_is() {
        let t0 = Instant::now();
        let mut watch = Watch::new(t0);
        let result = watch.check(
            Side::Output,
            Some(failure(StreamError::Other, ErrorDetail::DeviceBusy, None)),
            5,
            t0,
        );
        assert_eq!(
            result.unwrap_err(),
            SessionError::Stream {
                side: Side::Output,
                error: StreamError::Other,
                detail: Some(ErrorDetail::DeviceBusy),
                note: None,
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

    /// An error of a stream, for the tests of the texts.
    fn stream_error(
        side: Side,
        error: StreamError,
        detail: Option<ErrorDetail>,
        note: Option<&str>,
    ) -> SessionError {
        SessionError::Stream {
            side,
            error,
            detail,
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn errors_display_in_english_for_logs_and_the_cli() {
        let lost = stream_error(Side::Input, StreamError::DeviceUnavailable, None, None);
        assert_eq!(
            lost.to_string(),
            "input stream stopped: the audio device is no longer available"
        );
        let stalled = SessionError::Stalled { side: Side::Output };
        assert_eq!(
            stalled.to_string(),
            "output stopped: no callback for 1 s (device unplugged?)"
        );
        let text = stalled.to_string();
        assert_eq!(String::from(stalled), text);
    }

    #[test]
    fn a_detail_and_a_message_show_in_one_pair_of_parentheses() {
        // The detail and the message of the backend.
        let both = stream_error(
            Side::Output,
            StreamError::Other,
            Some(ErrorDetail::BackendError),
            Some("snd_pcm_readi: Input/output error"),
        );
        assert_eq!(
            both.to_string(),
            "output stream stopped: the audio stream failed \
             (backend-error: snd_pcm_readi: Input/output error)"
        );
        // Only the detail.
        let busy = stream_error(
            Side::Input,
            StreamError::Other,
            Some(ErrorDetail::DeviceBusy),
            None,
        );
        assert_eq!(
            busy.to_string(),
            "input stream stopped: the audio stream failed (device-busy)"
        );
        // A kind this version cannot name is still shown, so it is not
        // mistaken for "nothing was recorded".
        let unknown = stream_error(
            Side::Output,
            StreamError::Other,
            Some(ErrorDetail::Unknown),
            None,
        );
        assert_eq!(
            unknown.to_string(),
            "output stream stopped: the audio stream failed (unknown)"
        );
        // Nothing recorded: the text is what it was before.
        let none = stream_error(Side::Output, StreamError::Other, None, None);
        assert_eq!(
            none.to_string(),
            "output stream stopped: the audio stream failed"
        );
    }

    #[test]
    fn a_message_in_another_language_is_shown_as_it_is() {
        let japanese = stream_error(
            Side::Input,
            StreamError::DeviceUnavailable,
            Some(ErrorDetail::DeviceNotAvailable),
            Some("デバイスが無効になりました"),
        );
        assert_eq!(
            japanese.to_string(),
            "input stream stopped: the audio device is no longer available \
             (device-not-available: デバイスが無効になりました)"
        );
    }

    #[test]
    fn the_japanese_line_does_not_change_with_the_detail_or_the_message() {
        for detail in std::iter::once(None).chain(ErrorDetail::ALL.into_iter().map(Some)) {
            for note in [None, Some("snd_pcm_readi: Input/output error")] {
                let failed = stream_error(Side::Output, StreamError::Other, detail, note);
                assert_eq!(failed.message_ja(), "出力のストリームが止まりました");
            }
        }
    }

    #[test]
    fn errors_have_a_japanese_line_for_each_kind() {
        let stream = |side, error| stream_error(side, error, None, None);
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
