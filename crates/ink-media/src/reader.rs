//! Opening audio files and reading them a little at a time.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::Duration;

use symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Timestamp;

use crate::MediaError;

/// The kind of an audio file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundFormat {
    /// A WAV file (PCM).
    Wav,
    /// An MP3 file.
    Mp3,
}

/// What a file says about its audio, without decoding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundInfo {
    sample_rate: u32,
    channels: u16,
    frames: Option<u64>,
    format: SoundFormat,
}

impl SoundInfo {
    /// Returns the sample rate of the file in Hz (never 0).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns the number of channels in the file. The sound is always read
    /// as 2 channels (see [`SoundReader`]).
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Returns the number of frames, if the file says so. For an MP3 file
    /// with the encoder's delay and padding in its header, they are not
    /// counted: the frames are the sound that is played.
    pub fn frames(&self) -> Option<u64> {
        self.frames
    }

    /// Returns the kind of the file.
    pub fn format(&self) -> SoundFormat {
        self.format
    }

    /// Returns the length of the sound, if the file says its number of
    /// frames.
    pub fn duration(&self) -> Option<Duration> {
        self.frames
            .map(|frames| Duration::from_secs_f64(frames as f64 / f64::from(self.sample_rate)))
    }
}

/// Reads what the file at `path` says about its audio, without decoding it.
///
/// This fails in the same cases as [`SoundReader::open`].
pub fn probe(path: &Path) -> Result<SoundInfo, MediaError> {
    SoundReader::open(path).map(|reader| reader.info)
}

/// Reads an audio file as 2-channel f32 sound, a little at a time.
///
/// A mono file is read as the same sound on both channels (as the MIC is,
/// ADR-0007), and a file with more than 2 channels as its first 2.
///
/// Packets that cannot be decoded are skipped, and counted in
/// [`skipped_packets`](Self::skipped_packets); the caller, a worker thread,
/// decides whether to warn about them. A file that ends sooner than its
/// header says (a cut-off file) is read up to where it ends.
///
/// Like everything in this crate, a reader is for a control or worker
/// thread, never the audio thread.
pub struct SoundReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    info: SoundInfo,
    /// The last decoded packet, from `pending_at` on, is not yet returned.
    pending_left: Vec<f32>,
    pending_right: Vec<f32>,
    pending_at: usize,
    /// Interleaved samples of the packet being decoded.
    scratch: Vec<f32>,
    skipped_packets: u64,
    finished: bool,
}

impl SoundReader {
    /// Opens the file at `path`.
    ///
    /// # Errors
    ///
    /// - [`MediaError::Io`] if the file cannot be opened or read
    /// - [`MediaError::Empty`] if the file has no bytes, or says it has no
    ///   frames
    /// - [`MediaError::UnsupportedFormat`] if it is not a WAV or MP3 file, or
    ///   its codec is not supported
    /// - [`MediaError::NoAudioTrack`] if it has no audio
    /// - [`MediaError::InvalidSampleRate`] if its sample rate is 0
    /// - [`MediaError::Decode`] if its header is malformed
    pub fn open(path: &Path) -> Result<Self, MediaError> {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            return Err(MediaError::Empty);
        }
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(extension);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(probe_error)?;

        let (decoder, track_id, info) = {
            let track = format
                .default_track(TrackType::Audio)
                .ok_or(MediaError::NoAudioTrack)?;
            let params = track
                .codec_params
                .as_ref()
                .and_then(|params| params.audio())
                .ok_or(MediaError::NoAudioTrack)?;

            // A broken header can say 0. Stop here, so that no `SoundData` is
            // made with it (that would panic).
            let sample_rate = match params.sample_rate {
                Some(rate) if rate > 0 => rate,
                _ => return Err(MediaError::InvalidSampleRate(0)),
            };
            let channels = params
                .channels
                .as_ref()
                .map(|channels| channels.count())
                .filter(|count| *count > 0)
                .ok_or(MediaError::UnsupportedFormat)?;
            if track.num_frames == Some(0) {
                return Err(MediaError::Empty);
            }
            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())
                .map_err(media_error)?;
            let info = SoundInfo {
                sample_rate,
                channels: u16::try_from(channels).unwrap_or(u16::MAX),
                frames: track.num_frames,
                format: if params.codec == CODEC_ID_MP3 {
                    SoundFormat::Mp3
                } else {
                    SoundFormat::Wav
                },
            };
            (decoder, track.id, info)
        };

        Ok(Self {
            format,
            decoder,
            track_id,
            info,
            pending_left: Vec::new(),
            pending_right: Vec::new(),
            pending_at: 0,
            scratch: Vec::new(),
            skipped_packets: 0,
            finished: false,
        })
    }

    /// Returns what the file says about its audio.
    pub fn info(&self) -> &SoundInfo {
        &self.info
    }

    /// Returns how many packets could not be decoded and were skipped, since
    /// the file was opened (a [`rewind`](Self::rewind) does not reset it).
    pub fn skipped_packets(&self) -> u64 {
        self.skipped_packets
    }

    /// Appends up to `frames` frames to `left` and `right` and returns how
    /// many it appended. It returns fewer than `frames` only at the end of
    /// the file, and 0 once the end is reached (until a
    /// [`rewind`](Self::rewind)).
    ///
    /// # Errors
    ///
    /// Returns an error if reading or decoding fails in a way that cannot be
    /// skipped. The frames appended before the error stay in `left` and
    /// `right`.
    pub fn read(
        &mut self,
        left: &mut Vec<f32>,
        right: &mut Vec<f32>,
        frames: usize,
    ) -> Result<usize, MediaError> {
        let mut done = 0;
        while done < frames {
            if self.pending_at == self.pending_left.len() && !self.fill_pending()? {
                break;
            }
            let n = (self.pending_left.len() - self.pending_at).min(frames - done);
            let range = self.pending_at..self.pending_at + n;
            left.extend_from_slice(&self.pending_left[range.clone()]);
            right.extend_from_slice(&self.pending_right[range]);
            self.pending_at += n;
            done += n;
        }
        Ok(done)
    }

    /// Goes back to the start of the sound, for a loop.
    ///
    /// For an MP3 file with the encoder's delay in its header, the start is
    /// the first frame that is played, as it is when the file is opened.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be seeked.
    pub fn rewind(&mut self) -> Result<(), MediaError> {
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: Timestamp::new(0),
                    track_id: self.track_id,
                },
            )
            .map_err(media_error)?;
        self.decoder.reset();
        self.pending_left.clear();
        self.pending_right.clear();
        self.pending_at = 0;
        self.finished = false;
        Ok(())
    }

    /// Decodes the next packet that has sound into `pending_*`. Returns
    /// `false` at the end of the file.
    fn fill_pending(&mut self) -> Result<bool, MediaError> {
        self.pending_left.clear();
        self.pending_right.clear();
        self.pending_at = 0;
        loop {
            if self.finished {
                return Ok(false);
            }
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    self.finished = true;
                    return Ok(false);
                }
                // A file that is cut off before the end its header says.
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.finished = true;
                    return Ok(false);
                }
                Err(e) => return Err(media_error(e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buffer) => {
                    let channels = buffer.spec().channels().count();
                    if channels == 0 || buffer.frames() == 0 {
                        continue;
                    }
                    self.scratch.resize(buffer.samples_interleaved(), 0.0);
                    buffer.copy_to_slice_interleaved(&mut self.scratch);
                    for frame in self.scratch.chunks_exact(channels) {
                        self.pending_left.push(frame[0]);
                        // Mono is the same sound on both channels.
                        self.pending_right
                            .push(if channels > 1 { frame[1] } else { frame[0] });
                    }
                    return Ok(true);
                }
                Err(SymphoniaError::DecodeError(_) | SymphoniaError::IoError(_)) => {
                    self.skipped_packets += 1;
                }
                Err(e) => return Err(media_error(e)),
            }
        }
    }
}

/// The error of probing a file. A file too short to be recognized is
/// something that is not an audio file.
fn probe_error(e: SymphoniaError) -> MediaError {
    match e {
        SymphoniaError::IoError(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            MediaError::UnsupportedFormat
        }
        e => media_error(e),
    }
}

/// Turns an error of `symphonia` into ours.
fn media_error(e: SymphoniaError) -> MediaError {
    match e {
        SymphoniaError::IoError(e) => MediaError::Io(e),
        SymphoniaError::Unsupported(_) => MediaError::UnsupportedFormat,
        e => MediaError::Decode(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{
        TempDir, WavBuilder, WavFormat, amplitude_at, fixture, mp3_like_noise, noise, sine,
        wav_like_noise,
    };

    /// Reads everything, `block` frames at a time.
    fn read_all(reader: &mut SoundReader, block: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut left, mut right) = (Vec::new(), Vec::new());
        loop {
            let n = reader.read(&mut left, &mut right, block).unwrap();
            if n == 0 {
                return (left, right);
            }
        }
    }

    fn stereo_wav(format: WavFormat, rate: u32, frames: usize) -> (Vec<u8>, Vec<Vec<f32>>) {
        let channels = vec![
            sine(rate, 440.0, 0.5, frames),
            sine(rate, 660.0, 0.25, frames),
        ];
        (
            WavBuilder::new(format, rate, channels.clone()).build(),
            channels,
        )
    }

    fn open_bytes(bytes: &[u8]) -> Result<SoundReader, MediaError> {
        let dir = TempDir::new();
        let path = dir.write("sound.wav", bytes);
        SoundReader::open(&path)
    }

    // -- probe --------------------------------------------------------------

    #[test]
    fn probe_tells_the_rate_channels_frames_and_kind_of_a_wav_file() {
        let dir = TempDir::new();
        for (channels, rate) in [(1_usize, 22_050_u32), (2, 44_100), (6, 48_000)] {
            let samples: Vec<Vec<f32>> =
                (0..channels).map(|_| sine(rate, 440.0, 0.5, 480)).collect();
            let path = dir.write(
                "sound.wav",
                &WavBuilder::new(WavFormat::I16, rate, samples).build(),
            );
            let info = probe(&path).unwrap();
            assert_eq!(info.sample_rate(), rate);
            assert_eq!(usize::from(info.channels()), channels);
            assert_eq!(info.frames(), Some(480));
            assert_eq!(info.format(), SoundFormat::Wav);
            assert_eq!(
                info.duration(),
                Some(Duration::from_secs_f64(480.0 / f64::from(rate)))
            );
        }
    }

    #[test]
    fn probe_tells_the_same_for_the_mp3_files() {
        let cases = [
            ("tone-48k-stereo-gapless.mp3", 48_000, 2, 24_000),
            ("tone-44k-stereo-id3.mp3", 44_100, 2, 22_050),
            ("tone-44k-mono.mp3", 44_100, 1, 22_050),
        ];
        for (name, rate, channels, frames) in cases {
            let info = probe(&fixture(name)).unwrap();
            assert_eq!(info.sample_rate(), rate, "{name}");
            assert_eq!(info.channels(), channels, "{name}");
            assert_eq!(info.frames(), Some(frames), "{name}");
            assert_eq!(info.format(), SoundFormat::Mp3, "{name}");
        }
    }

    // -- reading ------------------------------------------------------------

    #[test]
    fn every_wav_format_reads_back_within_one_step() {
        let dir = TempDir::new();
        for format in WavFormat::ALL {
            let (bytes, channels) = stereo_wav(format, 48_000, 1_000);
            let mut reader = SoundReader::open(&dir.write("sound.wav", &bytes)).unwrap();
            let (left, right) = read_all(&mut reader, 256);
            assert_eq!(left.len(), 1_000, "{format:?}");
            assert_eq!(right.len(), 1_000, "{format:?}");
            for (n, (l, r)) in left.iter().zip(&right).enumerate() {
                assert!(
                    (l - channels[0][n]).abs() <= format.step(),
                    "{format:?} left {n}"
                );
                assert!(
                    (r - channels[1][n]).abs() <= format.step(),
                    "{format:?} right {n}"
                );
            }
            assert_eq!(reader.skipped_packets(), 0);
        }
    }

    #[test]
    fn the_blocks_can_be_of_any_size_and_give_the_same_sound() {
        let dir = TempDir::new();
        let (bytes, _) = stereo_wav(WavFormat::F32, 48_000, 5_000);
        let path = dir.write("sound.wav", &bytes);

        let whole = read_all(&mut SoundReader::open(&path).unwrap(), 1_000_000);
        assert_eq!(whole.0.len(), 5_000);
        for block in [1, 7, 1_024, 4_999, 5_000, 5_001] {
            let blocks = read_all(&mut SoundReader::open(&path).unwrap(), block);
            assert_eq!(blocks, whole, "block of {block}");
        }
    }

    #[test]
    fn a_read_returns_fewer_frames_only_at_the_end_and_then_zero() {
        let dir = TempDir::new();
        let (bytes, _) = stereo_wav(WavFormat::I16, 48_000, 100);
        let mut reader = SoundReader::open(&dir.write("sound.wav", &bytes)).unwrap();
        let (mut left, mut right) = (Vec::new(), Vec::new());

        assert_eq!(reader.read(&mut left, &mut right, 0).unwrap(), 0);
        assert!(left.is_empty(), "nothing was asked for");
        assert_eq!(reader.read(&mut left, &mut right, 60).unwrap(), 60);
        assert_eq!(reader.read(&mut left, &mut right, 60).unwrap(), 40);
        assert_eq!(reader.read(&mut left, &mut right, 60).unwrap(), 0);
        assert_eq!(reader.read(&mut left, &mut right, 60).unwrap(), 0);
        assert_eq!((left.len(), right.len()), (100, 100));
    }

    #[test]
    fn mono_is_the_same_sound_on_both_channels() {
        let dir = TempDir::new();
        let samples = vec![sine(44_100, 440.0, 0.5, 2_000)];
        let path = dir.write(
            "sound.wav",
            &WavBuilder::new(WavFormat::I16, 44_100, samples).build(),
        );
        let (left, right) = read_all(&mut SoundReader::open(&path).unwrap(), 512);
        assert_eq!(left.len(), 2_000);
        assert_eq!(left, right);
        assert!((amplitude_at(&left, 44_100, 440.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn a_file_with_more_than_two_channels_gives_the_first_two() {
        let dir = TempDir::new();
        let rate = 48_000;
        let samples: Vec<Vec<f32>> = (1..=6)
            .map(|k| sine(rate, 200.0 * f64::from(k), 0.25, 4_800))
            .collect();
        let path = dir.write(
            "sound.wav",
            &WavBuilder::new(WavFormat::I16, rate, samples).build(),
        );
        let mut reader = SoundReader::open(&path).unwrap();
        assert_eq!(reader.info().channels(), 6);
        let (left, right) = read_all(&mut reader, 1_024);
        assert!((amplitude_at(&left, rate, 200.0) - 0.25).abs() < 0.01);
        assert!((amplitude_at(&right, rate, 400.0) - 0.25).abs() < 0.01);
        assert!(
            amplitude_at(&left, rate, 600.0) < 0.01,
            "not the third channel"
        );
    }

    #[test]
    fn the_frequency_and_the_level_of_the_sound_are_kept() {
        let dir = TempDir::new();
        let (bytes, _) = stereo_wav(WavFormat::I24, 96_000, 96_000);
        let (left, right) = read_all(
            &mut SoundReader::open(&dir.write("sound.wav", &bytes)).unwrap(),
            4_096,
        );
        assert!((amplitude_at(&left, 96_000, 440.0) - 0.5).abs() < 0.005);
        assert!((amplitude_at(&right, 96_000, 660.0) - 0.25).abs() < 0.005);
        assert!(amplitude_at(&left, 96_000, 660.0) < 0.005);
    }

    // -- MP3 ----------------------------------------------------------------

    #[test]
    fn a_gapless_mp3_has_the_length_in_its_header_and_starts_at_once() {
        let mut reader = SoundReader::open(&fixture("tone-48k-stereo-gapless.mp3")).unwrap();
        let (left, right) = read_all(&mut reader, 1_024);
        assert_eq!(left.len(), 24_000, "the delay and the padding are removed");
        assert_eq!(right.len(), 24_000);
        // A cosine starts at its peak: no silence in front.
        assert!(left[0] > 0.4 && right[0] > 0.4, "{} {}", left[0], right[0]);
        assert!((amplitude_at(&left, 48_000, 440.0) - 0.5).abs() < 0.03);
        assert!((amplitude_at(&right, 48_000, 660.0) - 0.5).abs() < 0.03);
        assert!(amplitude_at(&left, 48_000, 660.0) < 0.02);
        assert_eq!(reader.skipped_packets(), 0);
    }

    #[test]
    fn an_mp3_with_an_id3_tag_is_read_past_the_tag() {
        let (left, right) = read_all(
            &mut SoundReader::open(&fixture("tone-44k-stereo-id3.mp3")).unwrap(),
            1_024,
        );
        assert_eq!(left.len(), 22_050);
        assert!((amplitude_at(&left, 44_100, 440.0) - 0.5).abs() < 0.03);
        assert!((amplitude_at(&right, 44_100, 660.0) - 0.5).abs() < 0.03);
    }

    #[test]
    fn a_mono_mp3_is_the_same_sound_on_both_channels() {
        let (left, right) = read_all(
            &mut SoundReader::open(&fixture("tone-44k-mono.mp3")).unwrap(),
            1_024,
        );
        assert_eq!(left.len(), 22_050);
        assert_eq!(left, right);
        assert!((amplitude_at(&left, 44_100, 440.0) - 0.5).abs() < 0.03);
    }

    #[test]
    fn an_mp3_without_the_header_starts_with_a_short_silence() {
        // The known limit of ADR-0009: the decoder cannot know the delay.
        let mut reader = SoundReader::open(&fixture("tone-48k-stereo-no-header.mp3")).unwrap();
        let (left, right) = read_all(&mut reader, 1_024);
        let silent = left
            .iter()
            .zip(&right)
            .take_while(|(l, r)| l.abs() < 0.01 && r.abs() < 0.01)
            .count();
        assert!(silent >= 100, "{silent} silent frames at the start");
        assert!(left.len() >= 24_000, "{} frames", left.len());
    }

    // -- rewind -------------------------------------------------------------

    #[test]
    fn after_a_rewind_the_second_time_through_is_the_same_as_the_first() {
        let dir = TempDir::new();
        let (bytes, _) = stereo_wav(WavFormat::I16, 48_000, 3_000);
        let wav = dir.write("sound.wav", &bytes);
        for path in [
            wav,
            fixture("tone-48k-stereo-gapless.mp3"),
            fixture("tone-44k-stereo-id3.mp3"),
        ] {
            let mut reader = SoundReader::open(&path).unwrap();
            let first = read_all(&mut reader, 1_000);
            assert_eq!(read_all(&mut reader, 1_000).0.len(), 0, "at the end");

            reader.rewind().unwrap();
            let second = read_all(&mut reader, 777);
            assert_eq!(first, second, "{}", path.display());
        }
    }

    #[test]
    fn a_rewind_in_the_middle_of_the_sound_starts_again_from_the_beginning() {
        let mut reader = SoundReader::open(&fixture("tone-48k-stereo-gapless.mp3")).unwrap();
        let (mut left, mut right) = (Vec::new(), Vec::new());
        reader.read(&mut left, &mut right, 10_000).unwrap();
        reader.rewind().unwrap();
        let (mut again_left, mut again_right) = (Vec::new(), Vec::new());
        reader
            .read(&mut again_left, &mut again_right, 2_000)
            .unwrap();
        assert_eq!(again_left, left[..2_000]);
        assert_eq!(again_right, right[..2_000]);
    }

    // -- errors -------------------------------------------------------------

    #[test]
    fn a_file_that_does_not_exist_is_an_io_error() {
        let dir = TempDir::new();
        let error = SoundReader::open(&dir.path().join("none.wav"))
            .err()
            .unwrap();
        assert!(matches!(&error, MediaError::Io(e) if e.kind() == io::ErrorKind::NotFound));
    }

    #[test]
    fn an_empty_file_is_empty() {
        assert!(matches!(open_bytes(b"").err().unwrap(), MediaError::Empty));
    }

    #[test]
    fn a_wav_without_frames_is_empty() {
        let bytes = WavBuilder::new(WavFormat::I16, 48_000, vec![Vec::new(), Vec::new()]).build();
        assert!(matches!(
            open_bytes(&bytes).err().unwrap(),
            MediaError::Empty
        ));
    }

    #[test]
    fn a_sample_rate_of_0_in_the_header_is_an_invalid_sample_rate() {
        let bytes = WavBuilder::new(WavFormat::I16, 48_000, vec![sine(48_000, 440.0, 0.5, 100)])
            .header_rate(0)
            .build();
        let error = open_bytes(&bytes).err().unwrap();
        assert!(matches!(error, MediaError::InvalidSampleRate(0)), "{error}");
        // `probe` stops it in the same way.
        let dir = TempDir::new();
        let path = dir.write("zero-rate.wav", &bytes);
        assert!(matches!(
            probe(&path).err().unwrap(),
            MediaError::InvalidSampleRate(0)
        ));
    }

    #[test]
    fn a_data_size_larger_than_the_file_is_a_decode_error() {
        let bytes = WavBuilder::new(WavFormat::I16, 48_000, vec![sine(48_000, 440.0, 0.5, 100)])
            .data_len(1_000_000)
            .build();
        let error = open_bytes(&bytes).err().unwrap();
        assert!(matches!(error, MediaError::Decode(_)), "{error}");
    }

    #[test]
    fn a_cut_off_wav_is_read_up_to_where_it_ends() {
        let (bytes, _) = stereo_wav(WavFormat::I16, 48_000, 1_000);
        let cut = &bytes[..bytes.len() / 2];
        let mut reader = open_bytes(cut).unwrap();
        assert_eq!(reader.info().frames(), Some(1_000), "the header says 1000");
        let (left, _) = read_all(&mut reader, 256);
        assert!(
            !left.is_empty() && left.len() < 1_000,
            "{} frames",
            left.len()
        );
    }

    #[test]
    fn something_that_is_not_audio_is_an_unsupported_format() {
        for bytes in [
            b"hello, this is a text file".to_vec(),
            noise(1, 4_096),
            b"RIFF".to_vec(),
            vec![0xFF],
        ] {
            let error = open_bytes(&bytes).err().unwrap();
            assert!(
                matches!(error, MediaError::UnsupportedFormat | MediaError::Decode(_)),
                "{error}"
            );
        }
    }

    #[test]
    fn an_unsupported_codec_in_a_wav_file_is_an_unsupported_format() {
        let mut bytes =
            WavBuilder::new(WavFormat::I16, 48_000, vec![sine(48_000, 440.0, 0.5, 100)]).build();
        // A format tag that no enabled codec knows.
        bytes[20..22].copy_from_slice(&0x1234_u16.to_le_bytes());
        let error = open_bytes(&bytes).err().unwrap();
        assert!(matches!(error, MediaError::UnsupportedFormat), "{error}");
    }

    #[test]
    fn a_damaged_packet_is_skipped_and_counted_and_the_rest_is_read() {
        let mut bytes = std::fs::read(fixture("tone-44k-stereo-id3.mp3")).unwrap();
        // Four bytes in the middle of the first packets; found by trying.
        bytes[1_367..1_371].fill(0xFF);
        let dir = TempDir::new();
        let path = dir.write("damaged.mp3", &bytes);

        let mut reader = SoundReader::open(&path).unwrap();
        let (left, _) = read_all(&mut reader, 1_024);
        assert_eq!(reader.skipped_packets(), 1);
        assert!(
            left.len() > 20_000 && left.len() < 22_050,
            "the sound before and after the packet: {} frames",
            left.len()
        );

        // The count is the total since the file was opened.
        reader.rewind().unwrap();
        read_all(&mut reader, 1_024);
        assert_eq!(reader.skipped_packets(), 2);
    }

    #[test]
    fn broken_files_never_panic() {
        let dir = TempDir::new();
        let (good_wav, _) = stereo_wav(WavFormat::I16, 48_000, 300);
        let good_mp3 = std::fs::read(fixture("tone-48k-stereo-gapless.mp3")).unwrap();
        let mut files: Vec<Vec<u8>> = Vec::new();
        for seed in 0..40 {
            files.push(wav_like_noise(seed, 64 + seed as usize * 37));
            files.push(mp3_like_noise(seed, 64 + seed as usize * 211));
            files.push(noise(seed, seed as usize * 53));
        }
        // Good files cut at every kind of place, and with a damaged middle.
        for cut in (0..good_wav.len()).step_by(37) {
            files.push(good_wav[..cut].to_vec());
        }
        for cut in (0..good_mp3.len()).step_by(517) {
            files.push(good_mp3[..cut].to_vec());
        }
        for seed in 0..20_usize {
            let mut damaged = good_mp3.clone();
            let at = 500 + seed * 400;
            for (k, byte) in noise(seed as u64, 120).into_iter().enumerate() {
                damaged[at + k] = byte;
            }
            files.push(damaged);
            let mut damaged = good_wav.clone();
            let at = 44 + seed * 20;
            for (k, byte) in noise(seed as u64, 40).into_iter().enumerate() {
                damaged[at + k] = byte;
            }
            files.push(damaged);
        }

        for (n, bytes) in files.iter().enumerate() {
            let path = dir.write(&format!("broken-{n}.bin"), bytes);
            // Whatever happens, it returns: an error or some sound.
            if let Ok(mut reader) = SoundReader::open(&path) {
                let (mut left, mut right) = (Vec::new(), Vec::new());
                loop {
                    match reader.read(&mut left, &mut right, 512) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                let _ = reader.rewind();
                let _ = reader.skipped_packets();
            }
        }
    }

    // -- other --------------------------------------------------------------

    #[test]
    fn a_file_with_a_japanese_name_can_be_read() {
        let dir = TempDir::new();
        let path = dir.copy_in(&fixture("tone-44k-mono.mp3"), "効果音.mp3");
        let mut reader = SoundReader::open(&path).unwrap();
        assert_eq!(read_all(&mut reader, 1_024).0.len(), 22_050);
    }

    #[test]
    fn the_extension_does_not_decide_the_format() {
        let dir = TempDir::new();
        let as_wav = dir.copy_in(&fixture("tone-44k-mono.mp3"), "really-an-mp3.wav");
        assert_eq!(probe(&as_wav).unwrap().format(), SoundFormat::Mp3);

        let (bytes, _) = stereo_wav(WavFormat::I16, 48_000, 100);
        let as_mp3 = dir.write("really-a-wav.mp3", &bytes);
        assert_eq!(probe(&as_mp3).unwrap().format(), SoundFormat::Wav);
    }

    #[test]
    fn a_reader_can_move_to_a_worker_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<SoundReader>();
        assert_send::<SoundInfo>();
    }
}
