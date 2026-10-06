//! [`AudioBackend`] implementation based on `cpal` (see ADR-0002).
//!
//! This is the only module that uses `cpal`. No `cpal` type appears in its
//! public API.

use std::collections::BTreeSet;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, I24, Sample, SampleFormat, SizedSample, U24};
use tracing::debug;

use crate::stream::StreamStatus;
use crate::{
    AudioBackend, BackendError, DeviceId, DeviceInfo, InputCallback, InputStream,
    MAX_FRAMES_PER_CALLBACK, Result, StreamConfig, StreamError,
};

/// Audio backend using the OS default `cpal` host
/// (Linux: ALSA, Windows: WASAPI).
pub struct CpalBackend {
    host: cpal::Host,
}

impl CpalBackend {
    /// Creates a backend on the OS default host.
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }

    fn enumerate(&self, direction: Direction) -> Result<Vec<DeviceInfo>> {
        let devices = self
            .host
            .devices()
            .map_err(|e| BackendError::HostUnavailable(e.to_string()))?;

        let mut infos = Vec::new();
        for device in devices {
            let id = match device.id() {
                Ok(id) => id.to_string(),
                Err(e) => {
                    debug!("skipping audio device without an ID: {e}");
                    continue;
                }
            };
            if !is_listed(&id) {
                debug!("skipping audio device not in the list: {id}");
                continue;
            }
            if !direction.supported_by(&device) {
                continue;
            }
            match device_info(&device, id, direction) {
                Ok(Some(info)) => infos.push(info),
                // No configuration in this direction: not a device of this kind.
                Ok(None) => {}
                Err(e) => debug!("skipping audio device: {e}"),
            }
        }
        Ok(infos)
    }

    fn default_device(&self, direction: Direction) -> Result<Option<DeviceInfo>> {
        let device = match direction {
            Direction::Input => self.host.default_input_device(),
            Direction::Output => self.host.default_output_device(),
        };
        let Some(device) = device else {
            return Ok(None);
        };
        let id = device
            .id()
            .map_err(|e| BackendError::DeviceUnavailable(e.to_string()))?
            .to_string();
        let Some(mut info) = device_info(&device, id, direction)? else {
            return Ok(None);
        };
        // cpal may name the default device differently from the same device
        // in the list. If no listed device has this ID, keep the default's name.
        if let Some(name) = self.listed_name(info.id.as_str(), direction) {
            info.name = name;
        }
        Ok(Some(info))
    }

    /// Finds a device by the ID that the device lists report.
    ///
    /// Compares with the same string the lists use. cpal's `device_by_id`
    /// rewrites ALSA IDs (`sysdefault:CARD=HID` becomes
    /// `sysdefault:CARD=HID,DEV=0`) and then misses devices that the list
    /// reports without `DEV`.
    fn find_device(&self, id: &DeviceId) -> Result<cpal::Device> {
        let devices = self
            .host
            .devices()
            .map_err(|e| BackendError::HostUnavailable(e.to_string()))?;
        devices
            .into_iter()
            .find(|device| device.id().is_ok_and(|d| d.to_string() == id.as_str()))
            .ok_or_else(|| BackendError::DeviceUnavailable(format!("{id}: not found")))
    }

    fn open_input_stream(
        &self,
        id: &DeviceId,
        config: StreamConfig,
        callback: InputCallback,
    ) -> Result<InputStream> {
        let device = self.find_device(id)?;
        let ranges: Vec<FormatRange> = device
            .supported_input_configs()
            .map_err(|e| BackendError::DeviceUnavailable(format!("{id}: {e}")))?
            .map(|r| FormatRange {
                channels: r.channels(),
                min_rate: r.min_sample_rate(),
                max_rate: r.max_sample_rate(),
                format: r.sample_format(),
            })
            .collect();
        let format = choose_format(&ranges, config).ok_or_else(|| {
            BackendError::UnsupportedConfig(format!(
                "{id}: {} Hz, {} ch is not supported",
                config.sample_rate, config.channels
            ))
        })?;

        let stream_config = cpal::StreamConfig {
            channels: config.channels,
            sample_rate: config.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let status = Arc::new(StreamStatus::new());
        let channels = usize::from(config.channels);
        let s = Arc::clone(&status);

        let stream = match format {
            SampleFormat::F32 => build_f32(&device, &stream_config, channels, s, callback),
            SampleFormat::F64 => build::<f64>(&device, &stream_config, channels, s, callback),
            SampleFormat::I8 => build::<i8>(&device, &stream_config, channels, s, callback),
            SampleFormat::I16 => build::<i16>(&device, &stream_config, channels, s, callback),
            SampleFormat::I24 => build::<I24>(&device, &stream_config, channels, s, callback),
            SampleFormat::I32 => build::<i32>(&device, &stream_config, channels, s, callback),
            SampleFormat::I64 => build::<i64>(&device, &stream_config, channels, s, callback),
            SampleFormat::U8 => build::<u8>(&device, &stream_config, channels, s, callback),
            SampleFormat::U16 => build::<u16>(&device, &stream_config, channels, s, callback),
            SampleFormat::U24 => build::<U24>(&device, &stream_config, channels, s, callback),
            SampleFormat::U32 => build::<u32>(&device, &stream_config, channels, s, callback),
            SampleFormat::U64 => build::<u64>(&device, &stream_config, channels, s, callback),
            // `choose_format` only returns formats from `PREFERRED_FORMATS`.
            other => {
                return Err(BackendError::UnsupportedConfig(format!(
                    "{id}: sample format {other} is not supported"
                )));
            }
        }
        .map_err(|e| open_error(id, &e))?;

        stream.play().map_err(|e| open_error(id, &e))?;
        debug!(
            "input stream opened: {id}, {} Hz, {} ch, {format}",
            config.sample_rate, config.channels
        );
        Ok(InputStream::new(
            Box::new(stream),
            status,
            config,
            format_name(format),
        ))
    }

    /// Returns the name that the device list shows for `id`.
    ///
    /// Reads only IDs and names; it does not open devices or query their
    /// configurations.
    fn listed_name(&self, id: &str, direction: Direction) -> Option<String> {
        let devices = self.host.devices().ok()?;
        let candidates = devices.filter_map(|device| {
            let device_id = device.id().ok()?.to_string();
            if device_id != id || !direction.supported_by(&device) {
                return None;
            }
            let name = device.description().ok()?.name().to_string();
            Some((device_id, name))
        });
        pick_listed_name(id, candidates)
    }
}

/// Picks the name of the listed device whose ID is `id` from
/// `(device ID, name)` pairs.
fn pick_listed_name(
    id: &str,
    candidates: impl IntoIterator<Item = (String, String)>,
) -> Option<String> {
    candidates
        .into_iter()
        .find(|(device_id, _)| device_id == id && is_listed(device_id))
        .map(|(_, name)| name)
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBackend for CpalBackend {
    fn enumerate_input_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.enumerate(Direction::Input)
    }

    fn enumerate_output_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.enumerate(Direction::Output)
    }

    fn default_input(&self) -> Result<Option<DeviceInfo>> {
        self.default_device(Direction::Input)
    }

    fn default_output(&self) -> Result<Option<DeviceInfo>> {
        self.default_device(Direction::Output)
    }

    fn open_input(
        &self,
        device: &DeviceId,
        config: StreamConfig,
        callback: InputCallback,
    ) -> Result<InputStream> {
        self.open_input_stream(device, config, callback)
    }
}

/// One supported configuration range of a device, reduced to what is needed
/// to choose a sample format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FormatRange {
    channels: u16,
    min_rate: u32,
    max_rate: u32,
    format: SampleFormat,
}

/// Sample formats that can be opened, in order of preference: f32 needs no
/// conversion, then the wider integer formats.
const PREFERRED_FORMATS: [SampleFormat; 12] = [
    SampleFormat::F32,
    SampleFormat::I32,
    SampleFormat::I24,
    SampleFormat::I16,
    SampleFormat::F64,
    SampleFormat::I64,
    SampleFormat::U32,
    SampleFormat::U24,
    SampleFormat::U16,
    SampleFormat::U64,
    SampleFormat::I8,
    SampleFormat::U8,
];

/// Chooses the preferred sample format among the ranges that support the
/// channel count and sample rate of `config`. `None` if no range does.
fn choose_format(ranges: &[FormatRange], config: StreamConfig) -> Option<SampleFormat> {
    if config.channels == 0 {
        return None;
    }
    let fits = |r: &&FormatRange| {
        r.channels == config.channels && (r.min_rate..=r.max_rate).contains(&config.sample_rate)
    };
    PREFERRED_FORMATS
        .into_iter()
        .find(|format| ranges.iter().filter(fits).any(|r| r.format == *format))
}

/// Returns a short name for a sample format, such as `"f32"` or `"i16"`,
/// so that no `cpal` type leaves this module.
fn format_name(format: SampleFormat) -> &'static str {
    match format {
        SampleFormat::F32 => "f32",
        SampleFormat::F64 => "f64",
        SampleFormat::I8 => "i8",
        SampleFormat::I16 => "i16",
        SampleFormat::I24 => "i24",
        SampleFormat::I32 => "i32",
        SampleFormat::I64 => "i64",
        SampleFormat::U8 => "u8",
        SampleFormat::U16 => "u16",
        SampleFormat::U24 => "u24",
        SampleFormat::U32 => "u32",
        SampleFormat::U64 => "u64",
        _ => "other",
    }
}

/// Builds an input stream for f32 samples, which are passed on unchanged.
fn build_f32(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    status: Arc<StreamStatus>,
    mut callback: InputCallback,
) -> std::result::Result<cpal::Stream, cpal::Error> {
    let error_status = Arc::clone(&status);
    device.build_input_stream::<f32, _, _>(
        *config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // Audio thread: no allocation, locking, or logging.
            status.record_callback(data.len() / channels);
            if !status.has_error() {
                deliver_f32_chunks(data, channels, &mut callback);
            }
        },
        move |error: cpal::Error| handle_stream_error(&error_status, error.kind()),
        None,
    )
}

/// Builds an input stream for samples of type `T`, converted to f32.
fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    status: Arc<StreamStatus>,
    mut callback: InputCallback,
) -> std::result::Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    // Allocated here, outside the audio thread, and moved into the callback.
    let mut buffer = vec![0.0_f32; MAX_FRAMES_PER_CALLBACK * channels];
    let error_status = Arc::clone(&status);
    device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            // Audio thread: no allocation, locking, or logging.
            status.record_callback(data.len() / channels);
            if !status.has_error() {
                deliver_chunks(data, channels, &mut buffer, &mut callback);
            }
        },
        move |error: cpal::Error| handle_stream_error(&error_status, error.kind()),
        None,
    )
}

/// Passes interleaved f32 `data` to `callback` in chunks of at most
/// [`MAX_FRAMES_PER_CALLBACK`] frames.
fn deliver_f32_chunks(data: &[f32], channels: usize, callback: &mut dyn FnMut(&[f32])) {
    for chunk in data.chunks(MAX_FRAMES_PER_CALLBACK * channels) {
        callback(chunk);
    }
}

/// Converts interleaved `data` to f32 in `buffer` and passes it to
/// `callback` in chunks of at most `buffer.len() / channels` frames.
///
/// Does not allocate: `buffer` is prepared by the caller.
fn deliver_chunks<T>(
    data: &[T],
    channels: usize,
    buffer: &mut [f32],
    callback: &mut dyn FnMut(&[f32]),
) where
    T: Copy,
    f32: FromSample<T>,
{
    let chunk_len = buffer.len() / channels * channels;
    if chunk_len == 0 {
        return;
    }
    for chunk in data.chunks(chunk_len) {
        let out = &mut buffer[..chunk.len()];
        for (o, &sample) in out.iter_mut().zip(chunk) {
            *o = f32::from_sample(sample);
        }
        callback(out);
    }
}

/// What to do with an error reported by a running stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorAction {
    /// The stream cannot continue.
    Fatal(StreamError),
    /// A buffer overrun; count it and continue.
    Xrun,
    /// Realtime scheduling was refused; flag it and continue.
    RealtimeDenied,
    /// Nothing to do.
    Ignore,
}

fn classify_error(kind: cpal::ErrorKind) -> ErrorAction {
    use cpal::ErrorKind;
    match kind {
        ErrorKind::Xrun => ErrorAction::Xrun,
        ErrorKind::RealtimeDenied => ErrorAction::RealtimeDenied,
        // The stream was opened for a specific device, so it does not follow
        // a new default device; this is only informational.
        ErrorKind::DeviceChanged => ErrorAction::Ignore,
        ErrorKind::DeviceNotAvailable => ErrorAction::Fatal(StreamError::DeviceUnavailable),
        ErrorKind::PermissionDenied => ErrorAction::Fatal(StreamError::PermissionDenied),
        _ => ErrorAction::Fatal(StreamError::Other),
    }
}

/// Error callback of a running stream. May run on the audio thread, so it
/// only updates atomics: no logging and no string formatting.
fn handle_stream_error(status: &StreamStatus, kind: cpal::ErrorKind) {
    match classify_error(kind) {
        ErrorAction::Fatal(error) => status.set_error(error),
        ErrorAction::Xrun => status.add_xrun(),
        ErrorAction::RealtimeDenied => status.set_realtime_denied(),
        ErrorAction::Ignore => {}
    }
}

/// Converts an error from building or starting a stream.
fn open_error(id: &DeviceId, error: &cpal::Error) -> BackendError {
    use cpal::ErrorKind;
    let message = format!("{id}: {error}");
    match error.kind() {
        ErrorKind::UnsupportedConfig => BackendError::UnsupportedConfig(message),
        ErrorKind::DeviceNotAvailable | ErrorKind::DeviceBusy | ErrorKind::PermissionDenied => {
            BackendError::DeviceUnavailable(message)
        }
        _ => BackendError::Other(message),
    }
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    Input,
    Output,
}

impl Direction {
    /// Cheap check that does not open the device.
    fn supported_by(self, device: &cpal::Device) -> bool {
        match self {
            Self::Input => device.supports_input(),
            Self::Output => device.supports_output(),
        }
    }
}

/// Builds the device information for one direction.
///
/// Returns `Ok(None)` when the device has no configuration in that direction.
fn device_info(
    device: &cpal::Device,
    id: String,
    direction: Direction,
) -> Result<Option<DeviceInfo>> {
    let unavailable = |e: cpal::Error| BackendError::DeviceUnavailable(format!("{id}: {e}"));

    let ranges: Vec<_> = match direction {
        Direction::Input => device
            .supported_input_configs()
            .map_err(unavailable)?
            .collect(),
        Direction::Output => device
            .supported_output_configs()
            .map_err(unavailable)?
            .collect(),
    };
    let Some(channels) = ranges.iter().map(|r| r.channels()).max() else {
        return Ok(None);
    };
    let sample_rates = expand_sample_rates(
        ranges
            .iter()
            .map(|r| (r.min_sample_rate(), r.max_sample_rate())),
    );

    let name = match device.description() {
        Ok(description) => description.name().to_string(),
        Err(_) => id.clone(),
    };

    Ok(Some(DeviceInfo {
        id: DeviceId::new(id),
        name,
        channels,
        sample_rates,
    }))
}

/// Whether a device is shown in the device lists, judged by its `cpal`
/// device ID string (`"<host>:<device>"`).
///
/// ALSA reports many plugin and virtual PCMs (`null`, `dmix`, `surround51`,
/// ...). Only the sound-server entries and per-card defaults are listed; the
/// check runs before the device is opened. Other hosts list every device.
///
/// Kept as a separate function so that a "show all devices" option can be
/// added later by bypassing it.
fn is_listed(device_id: &str) -> bool {
    let Some(pcm) = device_id.strip_prefix("alsa:") else {
        return true;
    };
    matches!(pcm, "default" | "pipewire" | "pulse") || pcm.starts_with("sysdefault:CARD=")
}

/// Lowest and highest sample rates (Hz) reported in a device's list.
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 384_000;

/// Standard rates reported when they fall inside a supported range.
const STANDARD_SAMPLE_RATES: [u32; 13] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
    352_800, 384_000,
];

/// Expands supported sample-rate ranges (`(min, max)` in Hz, inclusive) into a
/// sorted list without duplicates.
///
/// Each range contributes its own `min` and `max` (only when they lie within
/// 8000–384000 Hz) and every standard rate it contains.
fn expand_sample_rates(ranges: impl IntoIterator<Item = (u32, u32)>) -> Vec<u32> {
    let in_bounds = |rate: &u32| (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(rate);
    let mut rates = BTreeSet::new();
    for (min, max) in ranges {
        rates.extend([min, max].into_iter().filter(in_bounds));
        rates.extend(
            STANDARD_SAMPLE_RATES
                .into_iter()
                .filter(|rate| (min..=max).contains(rate)),
        );
    }
    rates.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_rate_range() {
        assert_eq!(expand_sample_rates([(48_000, 48_000)]), vec![48_000]);
    }

    #[test]
    fn narrow_range_keeps_min_and_max() {
        assert_eq!(
            expand_sample_rates([(44_100, 48_000)]),
            vec![44_100, 48_000]
        );
        assert_eq!(
            expand_sample_rates([(45_000, 47_000)]),
            vec![45_000, 47_000]
        );
    }

    #[test]
    fn wide_range_includes_standard_rates() {
        assert_eq!(
            expand_sample_rates([(8_000, 192_000)]),
            vec![
                8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
                192_000
            ]
        );
    }

    #[test]
    fn multiple_ranges_are_merged_sorted_and_deduplicated() {
        assert_eq!(
            expand_sample_rates([(96_000, 96_000), (44_100, 48_000), (48_000, 48_000)]),
            vec![44_100, 48_000, 96_000]
        );
    }

    #[test]
    fn min_and_max_outside_bounds_are_dropped() {
        assert_eq!(
            expand_sample_rates([(1, 4_000_000)]),
            STANDARD_SAMPLE_RATES.to_vec()
        );
        assert_eq!(expand_sample_rates([(4_000, 4_000)]), Vec::<u32>::new());
        assert_eq!(expand_sample_rates([(4_000, 9_000)]), vec![8_000, 9_000]);
    }

    #[test]
    fn alsa_allow_list() {
        for id in [
            "alsa:default",
            "alsa:pulse",
            "alsa:pipewire",
            "alsa:sysdefault:CARD=HID",
        ] {
            assert!(is_listed(id), "{id} should be listed");
        }
        for id in [
            "alsa:null",
            "alsa:hw:CARD=PCH,DEV=3",
            "alsa:plughw:CARD=HID,DEV=0",
            "alsa:dmix:CARD=HID,DEV=0",
            "alsa:surround51:CARD=HID,DEV=0",
            "alsa:sysdefault",
            "alsa:jack",
        ] {
            assert!(!is_listed(id), "{id} should not be listed");
        }
    }

    fn candidates(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect()
    }

    #[test]
    fn default_takes_the_name_from_the_list() {
        let found = pick_listed_name(
            "alsa:default",
            candidates(&[
                ("alsa:pulse", "PulseAudio Sound Server"),
                ("alsa:default", "Through the sound server"),
            ]),
        );
        assert_eq!(found.as_deref(), Some("Through the sound server"));
    }

    #[test]
    fn default_keeps_its_name_when_not_listed() {
        let others = candidates(&[("alsa:pulse", "PulseAudio Sound Server")]);
        assert_eq!(pick_listed_name("alsa:default", others), None);
        assert_eq!(pick_listed_name("alsa:default", Vec::new()), None);
        // A device that the list leaves out does not lend its name either.
        let hidden = candidates(&[("alsa:hw:CARD=PCH,DEV=3", "HDA Intel PCH")]);
        assert_eq!(pick_listed_name("alsa:hw:CARD=PCH,DEV=3", hidden), None);
    }

    #[test]
    fn other_hosts_list_everything() {
        assert!(is_listed("wasapi:{0.0.0.00000000}.{abc}"));
    }

    const STEREO_48K: StreamConfig = StreamConfig {
        sample_rate: 48_000,
        channels: 2,
    };

    fn range(channels: u16, min_rate: u32, max_rate: u32, format: SampleFormat) -> FormatRange {
        FormatRange {
            channels,
            min_rate,
            max_rate,
            format,
        }
    }

    #[test]
    fn f32_is_preferred() {
        let ranges = [
            range(2, 44_100, 48_000, SampleFormat::I16),
            range(2, 8_000, 192_000, SampleFormat::F32),
        ];
        assert_eq!(choose_format(&ranges, STEREO_48K), Some(SampleFormat::F32));
    }

    #[test]
    fn integer_formats_follow_preference_order() {
        let ranges = [
            range(2, 48_000, 48_000, SampleFormat::U8),
            range(2, 48_000, 48_000, SampleFormat::I16),
            range(2, 48_000, 48_000, SampleFormat::I24),
        ];
        assert_eq!(choose_format(&ranges, STEREO_48K), Some(SampleFormat::I24));

        let only_u16 = [range(2, 48_000, 48_000, SampleFormat::U16)];
        assert_eq!(
            choose_format(&only_u16, STEREO_48K),
            Some(SampleFormat::U16)
        );
    }

    #[test]
    fn format_must_fit_rate_and_channels() {
        // F32 only in mono; I16 in stereo: stereo gets I16.
        let ranges = [
            range(1, 8_000, 192_000, SampleFormat::F32),
            range(2, 8_000, 192_000, SampleFormat::I16),
        ];
        assert_eq!(choose_format(&ranges, STEREO_48K), Some(SampleFormat::I16));

        let only_44k = [range(2, 44_100, 44_100, SampleFormat::F32)];
        assert_eq!(choose_format(&only_44k, STEREO_48K), None);

        let mono_only = [range(1, 48_000, 48_000, SampleFormat::F32)];
        assert_eq!(choose_format(&mono_only, STEREO_48K), None);

        let zero_channels = StreamConfig {
            sample_rate: 48_000,
            channels: 0,
        };
        assert_eq!(choose_format(&mono_only, zero_channels), None);
    }

    #[test]
    fn every_preferred_format_has_a_name() {
        assert_eq!(format_name(SampleFormat::F32), "f32");
        assert_eq!(format_name(SampleFormat::I16), "i16");
        assert_eq!(format_name(SampleFormat::U24), "u24");
        for format in PREFERRED_FORMATS {
            assert_ne!(format_name(format), "other", "{format}");
        }
    }

    #[test]
    fn dsd_formats_are_never_chosen() {
        let ranges = [range(2, 48_000, 48_000, SampleFormat::DsdU8)];
        assert_eq!(choose_format(&ranges, STEREO_48K), None);
    }

    /// Collects the frame counts and samples passed to the callback.
    fn collect(
        channels: usize,
        run: impl FnOnce(&mut dyn FnMut(&[f32])),
    ) -> (Vec<usize>, Vec<f32>) {
        let mut frames = Vec::new();
        let mut samples = Vec::new();
        run(&mut |chunk: &[f32]| {
            frames.push(chunk.len() / channels);
            samples.extend_from_slice(chunk);
        });
        (frames, samples)
    }

    #[test]
    fn large_block_is_split_without_growing_buffer() {
        let channels = 2;
        let data = vec![1000_i16; 10_000 * channels];
        let mut buffer = vec![0.0_f32; MAX_FRAMES_PER_CALLBACK * channels];
        let capacity = buffer.capacity();

        let (frames, samples) = collect(channels, |cb| {
            deliver_chunks(&data, channels, &mut buffer, cb);
        });

        assert_eq!(frames, vec![4096, 4096, 1808]);
        assert_eq!(samples.len(), data.len());
        assert!(samples.iter().all(|&s| s == f32::from_sample(1000_i16)));
        assert_eq!(buffer.capacity(), capacity);
    }

    #[test]
    fn integer_samples_are_converted_to_f32() {
        let mut buffer = vec![0.0_f32; 8];
        let (_, s) = collect(1, |cb| {
            deliver_chunks(&[i16::MIN, 0, i16::MAX], 1, &mut buffer, cb);
        });
        assert_eq!(s[0], -1.0);
        assert_eq!(s[1], 0.0);
        assert!((s[2] - 1.0).abs() < 1e-4, "{}", s[2]);

        let (_, s) = collect(1, |cb| {
            deliver_chunks(&[0_u16, 32_768, u16::MAX], 1, &mut buffer, cb);
        });
        assert_eq!(s[0], -1.0);
        assert_eq!(s[1], 0.0);
        assert!((s[2] - 1.0).abs() < 1e-4, "{}", s[2]);
    }

    #[test]
    fn f32_samples_pass_unchanged_in_chunks() {
        let channels = 1;
        let data: Vec<f32> = (0..5000).map(|i| i as f32 / 5000.0).collect();
        let (frames, samples) = collect(channels, |cb| {
            deliver_f32_chunks(&data, channels, cb);
        });
        assert_eq!(frames, vec![4096, 904]);
        assert_eq!(samples, data);
    }

    #[test]
    fn stream_errors_are_classified() {
        use cpal::ErrorKind;
        let fatal = |e| ErrorAction::Fatal(e);
        assert_eq!(
            classify_error(ErrorKind::DeviceNotAvailable),
            fatal(StreamError::DeviceUnavailable)
        );
        assert_eq!(
            classify_error(ErrorKind::PermissionDenied),
            fatal(StreamError::PermissionDenied)
        );
        assert_eq!(
            classify_error(ErrorKind::StreamInvalidated),
            fatal(StreamError::Other)
        );
        assert_eq!(
            classify_error(ErrorKind::BackendError),
            fatal(StreamError::Other)
        );
        assert_eq!(classify_error(ErrorKind::Xrun), ErrorAction::Xrun);
        assert_eq!(
            classify_error(ErrorKind::RealtimeDenied),
            ErrorAction::RealtimeDenied
        );
        assert_eq!(
            classify_error(ErrorKind::DeviceChanged),
            ErrorAction::Ignore
        );
    }

    #[test]
    fn stream_errors_update_status() {
        use cpal::ErrorKind;
        let status = StreamStatus::new();
        handle_stream_error(&status, ErrorKind::Xrun);
        handle_stream_error(&status, ErrorKind::RealtimeDenied);
        handle_stream_error(&status, ErrorKind::DeviceChanged);
        assert_eq!(status.xruns(), 1);
        assert!(status.realtime_denied());
        assert_eq!(status.error(), None);

        handle_stream_error(&status, ErrorKind::DeviceNotAvailable);
        handle_stream_error(&status, ErrorKind::BackendError);
        assert_eq!(status.error(), Some(StreamError::DeviceUnavailable));
    }
}
