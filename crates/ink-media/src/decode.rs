//! Reading a whole audio file into memory.

use std::path::Path;
use std::time::Duration;

use ink_core::graph::SoundData;

use crate::{MediaError, SoundReader};

/// How many frames are read at a time.
const BLOCK_FRAMES: usize = 16_384;

/// The most frames memory is reserved for before reading, whatever the file
/// says about its length (a broken header can say anything). A longer sound
/// is still read: its buffers grow as it is read.
const MAX_RESERVED_FRAMES: u64 = 1 << 22;

/// A sound read from a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    /// The sound, at the sample rate of the file.
    pub sound: SoundData,
    /// How many packets could not be decoded and were skipped (see
    /// [`SoundReader::skipped_packets`]). The caller, a worker thread,
    /// should warn once when this is 1 or more.
    pub skipped_packets: u64,
}

/// Reads the whole audio file at `path` as 2-channel f32 sound, at the
/// sample rate of the file ([`resample`](crate::resample()) changes it).
///
/// A mono file becomes the same sound on both channels, and a file with
/// more than 2 channels its first 2 (see [`SoundReader`]).
///
/// With `max_duration`, a longer sound is refused with
/// [`MediaError::TooLong`]. A file that says how long it is, is refused
/// before it is decoded.
///
/// This reads files and allocates memory: call it from a control thread or a
/// worker thread, never the audio thread.
///
/// # Errors
///
/// - Every error of [`SoundReader::open`]
/// - [`MediaError::TooLong`] if the sound is longer than `max_duration`
/// - [`MediaError::Empty`] if not a single frame could be decoded
/// - [`MediaError::Decode`] or [`MediaError::Io`] if reading fails in a way
///   that cannot be skipped
pub fn decode_file(path: &Path, max_duration: Option<Duration>) -> Result<Decoded, MediaError> {
    let mut reader = SoundReader::open(path)?;
    let sample_rate = reader.info().sample_rate();
    let limit = max_duration.map(|duration| Limit {
        duration,
        frames: max_frames(duration, sample_rate),
    });

    if let (Some(limit), Some(frames)) = (limit, reader.info().frames())
        && frames > limit.frames
    {
        return Err(MediaError::TooLong {
            limit: limit.duration,
        });
    }

    let reserve = reader
        .info()
        .frames()
        .unwrap_or(0)
        .min(limit.map_or(MAX_RESERVED_FRAMES, |limit| limit.frames))
        .min(MAX_RESERVED_FRAMES);
    let (left, right) = read_limited(&mut reader, limit, reserve as usize)?;
    if left.is_empty() {
        return Err(MediaError::Empty);
    }
    Ok(Decoded {
        sound: SoundData::new(sample_rate, left, right),
        skipped_packets: reader.skipped_packets(),
    })
}

/// A limit on the length of a sound.
#[derive(Debug, Clone, Copy)]
struct Limit {
    duration: Duration,
    /// The same, in frames at the sample rate of the sound.
    frames: u64,
}

/// Returns how many frames of `sample_rate` fit in `duration`, rounded down.
///
/// This uses whole numbers: with floating point, a length that is exactly a
/// number of frames can come out one frame short (for example 9 ms at
/// 48000 Hz, which is 432 frames), and a sound of exactly the limit would be
/// refused.
fn max_frames(duration: Duration, sample_rate: u32) -> u64 {
    let frames = duration.as_nanos() * u128::from(sample_rate) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// Reads all that is left of `reader`, and stops with `TooLong` as soon as it
/// has more than `limit`. This also covers a file that does not say how long
/// it is.
fn read_limited(
    reader: &mut SoundReader,
    limit: Option<Limit>,
    reserve_frames: usize,
) -> Result<(Vec<f32>, Vec<f32>), MediaError> {
    let mut left = Vec::with_capacity(reserve_frames);
    let mut right = Vec::with_capacity(reserve_frames);
    loop {
        // One frame past the limit is enough to know it is too long.
        let block = match limit {
            Some(limit) => {
                let room = limit.frames.saturating_add(1) - left.len() as u64;
                BLOCK_FRAMES.min(usize::try_from(room).unwrap_or(BLOCK_FRAMES))
            }
            None => BLOCK_FRAMES,
        };
        if reader.read(&mut left, &mut right, block)? == 0 {
            return Ok((left, right));
        }
        if let Some(limit) = limit
            && left.len() as u64 > limit.frames
        {
            return Err(MediaError::TooLong {
                limit: limit.duration,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{
        TempDir, WavBuilder, WavFormat, amplitude_at, fixture, mp3_like_noise, noise, sine,
    };

    fn wav(format: WavFormat, rate: u32, frames: usize) -> (Vec<u8>, Vec<Vec<f32>>) {
        let channels = vec![
            sine(rate, 440.0, 0.5, frames),
            sine(rate, 660.0, 0.25, frames),
        ];
        (
            WavBuilder::new(format, rate, channels.clone()).build(),
            channels,
        )
    }

    fn all_of(reader: &mut SoundReader) -> (Vec<f32>, Vec<f32>) {
        read_limited(reader, None, 0).unwrap()
    }

    // -- reading ------------------------------------------------------------

    #[test]
    fn every_wav_format_is_read_within_one_step() {
        let dir = TempDir::new();
        for format in WavFormat::ALL {
            let (bytes, channels) = wav(format, 48_000, 2_000);
            let decoded = decode_file(&dir.write("sound.wav", &bytes), None).unwrap();
            assert_eq!(decoded.skipped_packets, 0);
            let sound = decoded.sound;
            assert_eq!(sound.sample_rate(), 48_000, "{format:?}");
            assert_eq!(sound.frames(), 2_000, "{format:?}");
            for (n, written) in channels[0].iter().enumerate() {
                assert!(
                    (sound.left()[n] - written).abs() <= format.step(),
                    "{format:?} left {n}"
                );
            }
            for (n, written) in channels[1].iter().enumerate() {
                assert!(
                    (sound.right()[n] - written).abs() <= format.step(),
                    "{format:?} right {n}"
                );
            }
        }
    }

    #[test]
    fn the_sound_keeps_its_rate_and_its_frequencies() {
        let dir = TempDir::new();
        for rate in [22_050, 44_100, 48_000, 96_000] {
            let (bytes, _) = wav(WavFormat::I16, rate, rate as usize);
            let sound = decode_file(&dir.write("sound.wav", &bytes), None)
                .unwrap()
                .sound;
            assert_eq!(sound.sample_rate(), rate);
            assert_eq!(sound.duration(), Duration::from_secs(1));
            assert!((amplitude_at(sound.left(), rate, 440.0) - 0.5).abs() < 0.01);
            assert!((amplitude_at(sound.right(), rate, 660.0) - 0.25).abs() < 0.01);
            assert!(amplitude_at(sound.left(), rate, 660.0) < 0.01);
        }
    }

    #[test]
    fn it_is_the_same_as_reading_with_a_reader() {
        let dir = TempDir::new();
        let (bytes, _) = wav(WavFormat::F32, 44_100, 10_000);
        for path in [
            dir.write("sound.wav", &bytes),
            fixture("tone-48k-stereo-gapless.mp3"),
            fixture("tone-44k-stereo-id3.mp3"),
        ] {
            let sound = decode_file(&path, None).unwrap().sound;
            let (left, right) = all_of(&mut SoundReader::open(&path).unwrap());
            assert_eq!(sound.left(), left, "{}", path.display());
            assert_eq!(sound.right(), right, "{}", path.display());
        }
    }

    #[test]
    fn mono_becomes_the_same_sound_on_both_channels() {
        let dir = TempDir::new();
        let bytes =
            WavBuilder::new(WavFormat::I16, 44_100, vec![sine(44_100, 440.0, 0.5, 500)]).build();
        let sound = decode_file(&dir.write("sound.wav", &bytes), None)
            .unwrap()
            .sound;
        assert_eq!(sound.left(), sound.right());

        let mp3 = decode_file(&fixture("tone-44k-mono.mp3"), None)
            .unwrap()
            .sound;
        assert_eq!(mp3.left(), mp3.right());
        assert_eq!(mp3.frames(), 22_050);
    }

    // -- MP3 ----------------------------------------------------------------

    #[test]
    fn the_mp3_files_have_the_right_length_rate_and_sound() {
        let cases = [
            ("tone-48k-stereo-gapless.mp3", 48_000, 24_000),
            ("tone-44k-stereo-id3.mp3", 44_100, 22_050),
        ];
        for (name, rate, frames) in cases {
            let sound = decode_file(&fixture(name), None).unwrap().sound;
            assert_eq!(sound.sample_rate(), rate, "{name}");
            assert_eq!(sound.frames(), frames, "{name}");
            // A cosine starts at its peak (0.5): a file that is gapless has no
            // silence in front. The encoder smooths the very first samples.
            assert!(sound.left()[0] > 0.3, "{name}: {}", sound.left()[0]);
            assert!((amplitude_at(sound.left(), rate, 440.0) - 0.5).abs() < 0.03);
            assert!((amplitude_at(sound.right(), rate, 660.0) - 0.5).abs() < 0.03);
        }
    }

    #[test]
    fn an_mp3_without_the_header_keeps_its_short_silence_at_the_start() {
        // The known limit of ADR-0009: it is not removed.
        let sound = decode_file(&fixture("tone-48k-stereo-no-header.mp3"), None)
            .unwrap()
            .sound;
        let silent = sound
            .left()
            .iter()
            .zip(sound.right())
            .take_while(|(l, r)| l.abs() < 0.01 && r.abs() < 0.01)
            .count();
        assert!(silent >= 100, "{silent} silent frames");
    }

    // -- the limit on the length -------------------------------------------

    #[test]
    fn a_sound_as_long_as_the_limit_is_read_and_a_frame_more_is_refused() {
        let dir = TempDir::new();
        let one_second = Duration::from_secs(1);

        let (exact, _) = wav(WavFormat::I16, 48_000, 48_000);
        let path = dir.write("exact.wav", &exact);
        assert_eq!(
            decode_file(&path, Some(one_second)).unwrap().sound.frames(),
            48_000
        );

        let (longer, _) = wav(WavFormat::I16, 48_000, 48_001);
        let path = dir.write("longer.wav", &longer);
        let error = decode_file(&path, Some(one_second)).unwrap_err();
        assert!(
            matches!(error, MediaError::TooLong { limit } if limit == one_second),
            "{error}"
        );

        // Without a limit the same file is read.
        assert_eq!(decode_file(&path, None).unwrap().sound.frames(), 48_001);
    }

    #[test]
    fn the_limit_is_in_frames_of_the_rate_of_the_file() {
        let dir = TempDir::new();
        let (bytes, _) = wav(WavFormat::I16, 8_000, 4_000);
        let path = dir.write("sound.wav", &bytes);
        // Half a second at 8 kHz is 4000 frames.
        assert!(decode_file(&path, Some(Duration::from_millis(500))).is_ok());
        assert!(decode_file(&path, Some(Duration::from_millis(499))).is_err());
    }

    #[test]
    fn a_length_that_floating_point_gets_wrong_is_still_read_exactly() {
        // With `seconds as f64 * rate`, each of these comes out one frame
        // short (9 ms at 48 kHz is 431.99999999999994), so a sound of exactly
        // the limit would be refused as too long.
        let dir = TempDir::new();
        let cases = [
            (Duration::from_millis(300), 44_100, 13_230),
            (Duration::from_millis(9), 48_000, 432),
            (Duration::from_millis(71), 48_000, 3_408),
            (Duration::from_millis(141), 96_000, 13_536),
        ];
        for (limit, rate, frames) in cases {
            assert_eq!(max_frames(limit, rate), frames, "{limit:?} at {rate} Hz");

            let (bytes, _) = wav(WavFormat::I16, rate, frames as usize);
            let path = dir.write("exact.wav", &bytes);
            let decoded = decode_file(&path, Some(limit)).unwrap();
            assert_eq!(
                decoded.sound.frames(),
                frames as usize,
                "{limit:?} at {rate} Hz"
            );

            let (bytes, _) = wav(WavFormat::I16, rate, frames as usize + 1);
            let path = dir.write("longer.wav", &bytes);
            let error = decode_file(&path, Some(limit)).unwrap_err();
            assert!(matches!(error, MediaError::TooLong { .. }), "{error}");
        }
    }

    #[test]
    fn the_number_of_frames_of_a_limit_is_rounded_down_and_never_overflows() {
        assert_eq!(max_frames(Duration::ZERO, 48_000), 0);
        assert_eq!(max_frames(Duration::from_nanos(20_833), 48_000), 0);
        assert_eq!(max_frames(Duration::from_nanos(20_834), 48_000), 1);
        assert_eq!(max_frames(Duration::from_secs(60), 48_000), 2_880_000);
        assert_eq!(max_frames(Duration::MAX, 192_000), u64::MAX);
    }

    #[test]
    fn a_sound_that_does_not_say_its_length_is_stopped_while_it_is_read() {
        // `decode_file` refuses a file that says it is too long before it
        // reads it. Here the limit is checked while reading, which is what
        // protects against a file that does not say how long it is.
        let dir = TempDir::new();
        let (bytes, _) = wav(WavFormat::I16, 48_000, 50_000);
        let mut reader = SoundReader::open(&dir.write("sound.wav", &bytes)).unwrap();
        let limit = Limit {
            duration: Duration::from_secs(1),
            frames: 48_000,
        };
        let error = read_limited(&mut reader, Some(limit), 0).unwrap_err();
        assert!(matches!(error, MediaError::TooLong { .. }), "{error}");

        // At exactly the limit it passes, and the memory asked for first is
        // only the limit.
        let (bytes, _) = wav(WavFormat::I16, 48_000, 48_000);
        let mut reader = SoundReader::open(&dir.write("sound.wav", &bytes)).unwrap();
        let (left, right) = read_limited(&mut reader, Some(limit), 48_000).unwrap();
        assert_eq!((left.len(), right.len()), (48_000, 48_000));
    }

    #[test]
    fn a_header_that_claims_a_huge_length_does_not_make_it_reserve_that_much() {
        let dir = TempDir::new();
        let (mut bytes, _) = wav(WavFormat::I16, 48_000, 100);
        // The RIFF size and the data size both say about 2 GB.
        bytes[4..8].copy_from_slice(&0x7FFF_FFF0_u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&0x7FFF_FFD0_u32.to_le_bytes());
        let path = dir.write("lying.wav", &bytes);
        // The file has 100 frames. Whatever the outcome, it must come back.
        match decode_file(&path, None) {
            Ok(decoded) => assert_eq!(decoded.sound.frames(), 100),
            Err(error) => assert!(
                matches!(error, MediaError::Decode(_) | MediaError::Empty),
                "{error}"
            ),
        }
    }

    // -- when there is no sound ---------------------------------------------

    #[test]
    fn a_file_with_no_decodable_frame_is_empty() {
        // The header says 100 frames, but the file ends right after it.
        let dir = TempDir::new();
        let (bytes, _) = wav(WavFormat::I16, 48_000, 100);
        let path = dir.write("cut.wav", &bytes[..44]);
        let error = decode_file(&path, None).unwrap_err();
        assert!(matches!(error, MediaError::Empty), "{error}");
    }

    #[test]
    fn a_wav_without_frames_is_empty() {
        let dir = TempDir::new();
        let bytes = WavBuilder::new(WavFormat::I16, 48_000, vec![Vec::new(), Vec::new()]).build();
        let error = decode_file(&dir.write("none.wav", &bytes), None).unwrap_err();
        assert!(matches!(error, MediaError::Empty), "{error}");
    }

    // -- errors --------------------------------------------------------------

    #[test]
    fn a_sample_rate_of_0_never_reaches_the_sound() {
        let dir = TempDir::new();
        let bytes = WavBuilder::new(WavFormat::I16, 48_000, vec![sine(48_000, 440.0, 0.5, 100)])
            .header_rate(0)
            .build();
        let error = decode_file(&dir.write("zero.wav", &bytes), None).unwrap_err();
        assert!(matches!(error, MediaError::InvalidSampleRate(0)), "{error}");
    }

    #[test]
    fn the_errors_of_opening_come_through() {
        let dir = TempDir::new();
        let missing = decode_file(&dir.path().join("none.wav"), None).unwrap_err();
        assert!(matches!(missing, MediaError::Io(_)), "{missing}");

        let empty = decode_file(&dir.write("empty.wav", b""), None).unwrap_err();
        assert!(matches!(empty, MediaError::Empty), "{empty}");

        let text = decode_file(&dir.write("text.mp3", b"not audio at all"), None).unwrap_err();
        assert!(
            matches!(text, MediaError::UnsupportedFormat | MediaError::Decode(_)),
            "{text}"
        );
    }

    #[test]
    fn broken_files_never_panic() {
        let dir = TempDir::new();
        let (good_wav, _) = wav(WavFormat::I16, 48_000, 300);
        let good_mp3 = std::fs::read(fixture("tone-44k-stereo-id3.mp3")).unwrap();
        let mut files: Vec<Vec<u8>> = Vec::new();
        for seed in 0..30 {
            files.push(mp3_like_noise(seed, 64 + seed as usize * 173));
            files.push(noise(seed, seed as usize * 61));
        }
        for cut in (0..good_wav.len()).step_by(41) {
            files.push(good_wav[..cut].to_vec());
        }
        for cut in (0..good_mp3.len()).step_by(433) {
            files.push(good_mp3[..cut].to_vec());
        }
        for seed in 0..25_usize {
            let mut damaged = good_mp3.clone();
            let at = 400 + seed * 300;
            for (k, byte) in noise(seed as u64, 150).into_iter().enumerate() {
                damaged[at + k] = byte;
            }
            files.push(damaged);
        }
        for (n, bytes) in files.iter().enumerate() {
            let path = dir.write(&format!("broken-{n}.bin"), bytes);
            // An error or some sound; with or without a limit.
            let _ = decode_file(&path, None);
            let _ = decode_file(&path, Some(Duration::from_millis(100)));
        }
    }

    // -- skipped packets -----------------------------------------------------

    #[test]
    fn the_number_of_skipped_packets_is_returned() {
        let mut bytes = std::fs::read(fixture("tone-44k-stereo-id3.mp3")).unwrap();
        // The four bytes that make one packet undecodable (see the test of the
        // reader). If a new version of symphonia changes this, look here first.
        bytes[1_367..1_371].fill(0xFF);
        let dir = TempDir::new();
        let decoded = decode_file(&dir.write("damaged.mp3", &bytes), None).unwrap();
        assert_eq!(decoded.skipped_packets, 1);
        assert!(decoded.sound.frames() > 20_000 && decoded.sound.frames() < 22_050);
    }

    // -- other ----------------------------------------------------------------

    #[test]
    fn a_file_with_a_japanese_name_can_be_read() {
        let dir = TempDir::new();
        let path = dir.copy_in(&fixture("tone-48k-stereo-gapless.mp3"), "効果音.mp3");
        let decoded = decode_file(&path, None).unwrap();
        assert_eq!(decoded.sound.frames(), 24_000);
    }

    #[test]
    fn the_result_can_move_between_threads() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Decoded>();
    }
}
