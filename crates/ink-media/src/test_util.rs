//! Helpers for the tests of this crate: test sounds made in memory, WAV
//! files in every format the decoder must handle (and broken ones),
//! temporary files, and a measurement of the level of one frequency.
//!
//! Test audio is made by the tests themselves from sine waves, so no audio
//! file by anybody else is needed. The few MP3 files that cannot be made
//! here are in `tests/fixtures` (see the `README.md` there).

use std::f64::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Returns `frames` samples of a sine wave of `freq` Hz and `amplitude`,
/// starting at phase 0.
pub(crate) fn sine(sample_rate: u32, freq: f64, amplitude: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| {
            let t = n as f64 / f64::from(sample_rate);
            (f64::from(amplitude) * (2.0 * PI * freq * t).sin()) as f32
        })
        .collect()
}

/// Returns the amplitude of the sine wave of `freq` Hz in `samples`
/// (Goertzel's algorithm): about `amplitude` for a sine wave of that
/// frequency and about 0 for one of a different frequency.
pub(crate) fn amplitude_at(samples: &[f32], sample_rate: u32, freq: f64) -> f64 {
    let w = 2.0 * PI * freq / f64::from(sample_rate);
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0_f64, 0.0_f64);
    for &x in samples {
        let s = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.max(0.0).sqrt() / samples.len() as f64
}

/// The sample format of a WAV file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WavFormat {
    /// 8 bit unsigned integer.
    U8,
    /// 16 bit signed integer.
    I16,
    /// 24 bit signed integer.
    I24,
    /// 32 bit signed integer.
    I32,
    /// 32 bit floating point.
    F32,
}

impl WavFormat {
    pub(crate) const ALL: [WavFormat; 5] = [Self::U8, Self::I16, Self::I24, Self::I32, Self::F32];

    pub(crate) fn bits(self) -> u16 {
        match self {
            Self::U8 => 8,
            Self::I16 => 16,
            Self::I24 => 24,
            Self::I32 | Self::F32 => 32,
        }
    }

    /// The size of one step of the format, as a fraction of full scale: the
    /// most a decoded sample may differ from the sample that was written.
    pub(crate) fn step(self) -> f32 {
        match self {
            Self::U8 => 1.0 / 127.0,
            Self::I16 => 1.0 / 32_767.0,
            Self::I24 => 1.0 / 8_388_607.0,
            Self::I32 | Self::F32 => 1.0e-6,
        }
    }

    fn is_float(self) -> bool {
        self == Self::F32
    }

    fn push_sample(self, bytes: &mut Vec<u8>, x: f32) {
        let x = f64::from(x.clamp(-1.0, 1.0));
        match self {
            Self::U8 => bytes.push((x * 127.0 + 128.0).round() as u8),
            Self::I16 => bytes.extend_from_slice(&((x * 32_767.0).round() as i16).to_le_bytes()),
            Self::I24 => {
                let v = (x * 8_388_607.0).round() as i32;
                bytes.extend_from_slice(&v.to_le_bytes()[..3]);
            }
            Self::I32 => {
                bytes.extend_from_slice(&((x * 2_147_483_647.0).round() as i32).to_le_bytes());
            }
            Self::F32 => bytes.extend_from_slice(&(x as f32).to_le_bytes()),
        }
    }
}

/// Builds the bytes of a WAV file. The header can be made to lie, to make
/// broken files.
#[derive(Debug, Clone)]
pub(crate) struct WavBuilder {
    format: WavFormat,
    sample_rate: u32,
    /// One vector of samples per channel (all of the same length).
    channels: Vec<Vec<f32>>,
    header_rate: Option<u32>,
    data_len: Option<u32>,
}

impl WavBuilder {
    pub(crate) fn new(format: WavFormat, sample_rate: u32, channels: Vec<Vec<f32>>) -> Self {
        assert!(!channels.is_empty());
        assert!(channels.iter().all(|c| c.len() == channels[0].len()));
        Self {
            format,
            sample_rate,
            channels,
            header_rate: None,
            data_len: None,
        }
    }

    /// Writes this sample rate in the header (the samples are not changed).
    pub(crate) fn header_rate(mut self, rate: u32) -> Self {
        self.header_rate = Some(rate);
        self
    }

    /// Writes this size in the `data` chunk header (the data is not
    /// changed), so that it can be larger than the data that follows.
    pub(crate) fn data_len(mut self, len: u32) -> Self {
        self.data_len = Some(len);
        self
    }

    pub(crate) fn build(&self) -> Vec<u8> {
        let channels = self.channels.len() as u16;
        let frames = self.channels[0].len();
        let bytes_per_sample = usize::from(self.format.bits() / 8);
        let block_align = channels * (self.format.bits() / 8);
        let rate = self.header_rate.unwrap_or(self.sample_rate);

        let mut data = Vec::with_capacity(frames * usize::from(block_align));
        for n in 0..frames {
            for channel in &self.channels {
                self.format.push_sample(&mut data, channel[n]);
            }
        }
        assert_eq!(
            data.len(),
            frames * usize::from(channels) * bytes_per_sample
        );

        // The format that the standard asks to be written as WAVE_FORMAT_EXTENSIBLE.
        let extensible = channels > 2 || self.format.bits() > 16;
        let format_code: u16 = if self.format.is_float() { 3 } else { 1 };

        let mut fmt = Vec::new();
        fmt.extend_from_slice(&(if extensible { 0xFFFE } else { format_code }).to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate.wrapping_mul(u32::from(block_align))).to_le_bytes());
        fmt.extend_from_slice(&block_align.to_le_bytes());
        fmt.extend_from_slice(&self.format.bits().to_le_bytes());
        if extensible {
            fmt.extend_from_slice(&22_u16.to_le_bytes());
            fmt.extend_from_slice(&self.format.bits().to_le_bytes());
            let mask: u32 = match channels {
                1 => 0x4,
                2 => 0x3,
                6 => 0x3F,
                _ => 0,
            };
            fmt.extend_from_slice(&mask.to_le_bytes());
            fmt.extend_from_slice(&format_code.to_le_bytes());
            fmt.extend_from_slice(&[
                0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
            ]);
        }

        let padding = data.len() % 2;
        let riff_len = 4 + (8 + fmt.len()) + (8 + data.len() + padding);
        let mut out = Vec::with_capacity(8 + riff_len);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(riff_len as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&self.data_len.unwrap_or(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out.extend_from_slice(&vec![0; padding]);
        out
    }
}

/// Returns `len` bytes of noise, always the same for the same `seed`.
pub(crate) fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// Returns noise that starts like a WAV file (`RIFF`, a size, `WAVE`).
pub(crate) fn wav_like_noise(seed: u64, len: usize) -> Vec<u8> {
    let mut bytes = b"RIFF\x24\x00\x00\x00WAVE".to_vec();
    bytes.extend(noise(seed, len));
    bytes
}

/// Returns noise that starts like an MP3 frame (a frame sync, MPEG-1
/// Layer III).
pub(crate) fn mp3_like_noise(seed: u64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFB, 0x90, 0x64];
    bytes.extend(noise(seed, len));
    bytes
}

/// The path of a file in `tests/fixtures`.
pub(crate) fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// A directory under the system temporary directory, removed when dropped.
/// It is unique per call, so tests that run in parallel do not meet.
#[derive(Debug)]
pub(crate) struct TempDir {
    path: PathBuf,
}

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

impl TempDir {
    pub(crate) fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ink-media-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `bytes` to a file called `name` in the directory.
    pub(crate) fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    /// Copies the file `from` to a file called `name` in the directory.
    pub(crate) fn copy_in(&self, from: &Path, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::copy(from, &path).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn a_sine_has_the_amplitude_and_frequency_it_was_made_with() {
        let samples = sine(48_000, 1_000.0, 0.5, 48_000);
        assert_eq!(samples.len(), 48_000);
        assert_eq!(samples[0], 0.0);
        let peak = samples.iter().fold(0.0_f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 0.001, "{peak}");

        assert!((amplitude_at(&samples, 48_000, 1_000.0) - 0.5).abs() < 0.005);
        assert!(amplitude_at(&samples, 48_000, 2_000.0) < 0.005);
        assert!(amplitude_at(&samples, 48_000, 440.0) < 0.005);
    }

    #[test]
    fn a_wav_header_describes_the_data_that_follows() {
        let channels = vec![sine(44_100, 440.0, 0.5, 100), sine(44_100, 660.0, 0.5, 100)];
        let bytes = WavBuilder::new(WavFormat::I16, 44_100, channels).build();

        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32_at(&bytes, 4) as usize, bytes.len() - 8);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u32_at(&bytes, 16), 16);
        assert_eq!(u16_at(&bytes, 20), 1, "PCM");
        assert_eq!(u16_at(&bytes, 22), 2, "channels");
        assert_eq!(u32_at(&bytes, 24), 44_100, "rate");
        assert_eq!(u32_at(&bytes, 28), 44_100 * 4, "bytes per second");
        assert_eq!(u16_at(&bytes, 32), 4, "block align");
        assert_eq!(u16_at(&bytes, 34), 16, "bits");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32_at(&bytes, 40), 100 * 4);
        assert_eq!(bytes.len(), 44 + 100 * 4);
    }

    /// Returns the bytes of the `data` chunk of a WAV file made by
    /// [`WavBuilder`] (the size field is read from the header).
    fn data_chunk(bytes: &[u8]) -> &[u8] {
        let fmt_len = u32_at(bytes, 16) as usize;
        let at = 12 + 8 + fmt_len;
        assert_eq!(&bytes[at..at + 4], b"data");
        let len = u32_at(bytes, at + 4) as usize;
        &bytes[at + 8..at + 8 + len]
    }

    /// Reads the samples back from the bytes of one channel, the way a
    /// decoder should.
    fn read_back(format: WavFormat, data: &[u8]) -> Vec<f32> {
        match format {
            WavFormat::U8 => data
                .iter()
                .map(|b| (f32::from(*b) - 128.0) / 127.0)
                .collect(),
            WavFormat::I16 => data
                .chunks(2)
                .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32_767.0)
                .collect(),
            WavFormat::I24 => data
                .chunks(3)
                .map(|c| {
                    let v = i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8;
                    v as f32 / 8_388_607.0
                })
                .collect(),
            WavFormat::I32 => data
                .chunks(4)
                .map(|c| {
                    (f64::from(i32::from_le_bytes([c[0], c[1], c[2], c[3]])) / 2_147_483_647.0)
                        as f32
                })
                .collect(),
            WavFormat::F32 => data
                .chunks(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        }
    }

    #[test]
    fn every_format_writes_samples_that_read_back_within_one_step() {
        let samples = vec![0.0, 0.5, -0.5, 1.0, -1.0, 0.123, -0.987];
        for format in WavFormat::ALL {
            let bytes = WavBuilder::new(format, 48_000, vec![samples.clone()]).build();
            let data = data_chunk(&bytes);
            assert_eq!(data.len(), samples.len() * usize::from(format.bits() / 8));
            let back = read_back(format, data);
            for (written, read) in samples.iter().zip(&back) {
                assert!(
                    (written - read).abs() <= format.step(),
                    "{format:?}: wrote {written}, read {read}"
                );
            }
        }
    }

    #[test]
    fn the_samples_of_the_channels_are_interleaved() {
        let bytes = WavBuilder::new(
            WavFormat::I16,
            48_000,
            vec![vec![1.0, 0.5], vec![-1.0, -0.5]],
        )
        .build();
        let back = read_back(WavFormat::I16, data_chunk(&bytes));
        // left 0, right 0, left 1, right 1
        assert_eq!(back.len(), 4);
        assert!(back[0] > 0.99 && back[1] < -0.99 && back[2] > 0.49 && back[3] < -0.49);
    }

    #[test]
    fn unsigned_8_bit_silence_is_the_middle_of_the_range() {
        let bytes = WavBuilder::new(WavFormat::U8, 48_000, vec![vec![0.0, 1.0, -1.0]]).build();
        // 3 bytes of data, then a byte of padding to an even length.
        assert_eq!(data_chunk(&bytes), [128, 255, 1]);
        assert_eq!(bytes.len() % 2, 0);
    }

    #[test]
    fn wide_and_multichannel_wav_files_use_the_extensible_header() {
        let six: Vec<Vec<f32>> = (0..6).map(|_| vec![0.0; 10]).collect();
        let bytes = WavBuilder::new(WavFormat::I16, 48_000, six).build();
        assert_eq!(u32_at(&bytes, 16), 40);
        assert_eq!(u16_at(&bytes, 20), 0xFFFE);
        assert_eq!(u16_at(&bytes, 22), 6);
        assert_eq!(u32_at(&bytes, 40), 0x3F, "channel mask of 5.1");

        let wide = WavBuilder::new(WavFormat::I24, 48_000, vec![vec![0.0; 10]]).build();
        assert_eq!(u16_at(&wide, 20), 0xFFFE);
        assert_eq!(u16_at(&wide, 34), 24);

        let float = WavBuilder::new(WavFormat::F32, 48_000, vec![vec![0.0; 10]]).build();
        assert_eq!(u16_at(&float, 20), 0xFFFE);
        assert_eq!(u16_at(&float, 44), 3, "IEEE float sub-format");

        let narrow = WavBuilder::new(WavFormat::U8, 48_000, vec![vec![0.0; 10]]).build();
        assert_eq!(u16_at(&narrow, 20), 1);
        assert_eq!(u32_at(&narrow, 16), 16);
    }

    #[test]
    fn the_header_can_be_made_to_lie() {
        let channels = vec![vec![0.25; 8], vec![0.25; 8]];
        let honest = WavBuilder::new(WavFormat::I16, 44_100, channels.clone()).build();
        let rate = WavBuilder::new(WavFormat::I16, 44_100, channels.clone())
            .header_rate(0)
            .build();
        assert_eq!(u32_at(&rate, 24), 0);
        assert_eq!(rate.len(), honest.len(), "only the header changes");

        let long = WavBuilder::new(WavFormat::I16, 44_100, channels)
            .data_len(1_000_000)
            .build();
        assert_eq!(u32_at(&long, 40), 1_000_000);
        assert_eq!(long.len(), honest.len());
    }

    #[test]
    fn noise_is_the_same_for_the_same_seed_and_starts_like_a_file() {
        assert_eq!(noise(7, 64), noise(7, 64));
        assert_ne!(noise(7, 64), noise(8, 64));
        assert_eq!(noise(7, 64).len(), 64);

        let wav = wav_like_noise(1, 32);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 12 + 32);

        let mp3 = mp3_like_noise(1, 32);
        assert_eq!(&mp3[0..2], [0xFF, 0xFB]);
        assert_eq!(mp3.len(), 4 + 32);
    }

    #[test]
    fn a_temporary_directory_is_unique_holds_files_and_goes_away() {
        let (path, file) = {
            let dir = TempDir::new();
            let other = TempDir::new();
            assert_ne!(dir.path(), other.path());

            let file = dir.write("効果音.wav", b"abc");
            assert_eq!(fs::read(&file).unwrap(), b"abc");
            let copy = dir.copy_in(&file, "copy.wav");
            assert_eq!(fs::read(copy).unwrap(), b"abc");
            (dir.path().to_path_buf(), file)
        };
        assert!(!file.exists());
        assert!(!path.exists());
    }

    #[test]
    fn a_fixture_path_points_into_the_tests_directory() {
        let path = fixture("example.mp3");
        assert!(
            path.ends_with("tests/fixtures/example.mp3")
                || path.ends_with("tests\\fixtures\\example.mp3")
        );
    }
}
