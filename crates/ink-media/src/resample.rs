//! Changing the sample rate of a sound.

use std::path::Path;
use std::time::Duration;

use ink_core::graph::SoundData;
use rubato::audioadapter_buffers::direct::SequentialSliceOfSlices;
use rubato::audioadapter_buffers::owned::SequentialOwned;
use rubato::{Fft, FixedSync, Resampler};

use crate::{Decoded, MediaError, decode_file};

/// The number of input frames the resampler takes at a time. Measured with
/// the throwaway prototype of ADR-0009: a 5 minute sound takes about 0.4
/// seconds.
const CHUNK_FRAMES: usize = 1_024;

/// The most frames a resampled sound may have (about 256 MiB for each
/// channel). A sound effect is at most a minute long, so this is far above
/// what is needed; it stops a very high sample rate from asking for more
/// memory than the machine has.
const MAX_OUTPUT_FRAMES: usize = 1 << 26;

/// Changes the sample rate of `sound` to `sample_rate` (rubato, FFT).
///
/// The result has `round(frames × sample_rate / old rate)` frames and starts
/// at the same moment as the original: the delay of the resampler is
/// removed. A sound at the wanted rate is returned as it is (a copy). A sound
/// so short that it has no frame at the new rate (one frame at 96000 Hz,
/// resampled to 8000 Hz) gives an empty sound.
///
/// This allocates memory and takes time in proportion to the length of the
/// sound: call it from a control thread or a worker thread, never the audio
/// thread.
///
/// # Errors
///
/// - [`MediaError::InvalidSampleRate`] if `sample_rate` is 0
/// - [`MediaError::Resample`] if the result would have more than 2^26
///   frames, or the resampler cannot be made for the two rates, or fails
pub fn resample(sound: &SoundData, sample_rate: u32) -> Result<SoundData, MediaError> {
    if sample_rate == 0 {
        return Err(MediaError::InvalidSampleRate(0));
    }
    let from = sound.sample_rate();
    if from == sample_rate {
        return Ok(sound.clone());
    }
    let frames = sound.frames();
    let wanted = frames_at(frames, from, sample_rate);
    if wanted > MAX_OUTPUT_FRAMES {
        return Err(MediaError::Resample(format!(
            "the sound would have {wanted} frames at {sample_rate} Hz, more than the limit of {MAX_OUTPUT_FRAMES}"
        )));
    }
    if frames == 0 || wanted == 0 {
        return Ok(SoundData::new(sample_rate, Vec::new(), Vec::new()));
    }

    let mut resampler = Fft::<f32>::new(
        from as usize,
        sample_rate as usize,
        CHUNK_FRAMES,
        2,
        FixedSync::Input,
    )
    .map_err(|e| MediaError::Resample(e.to_string()))?;

    let channels: [&[f32]; 2] = [sound.left(), sound.right()];
    let input = SequentialSliceOfSlices::new(&channels, 2, frames)
        .map_err(|e| MediaError::Resample(e.to_string()))?;
    let capacity = resampler.process_all_needed_output_len(frames);
    let mut output = SequentialOwned::<f32>::new(0.0, 2, capacity);
    let (_, produced) = resampler
        .process_all_into_buffer(&input, &mut output, frames, None)
        .map_err(|e| MediaError::Resample(e.to_string()))?;

    // The buffer holds one channel after the other. Keep exactly `wanted`
    // frames (the resampler can make one more or less).
    let data = output.take_data();
    let channel = |index: usize| {
        let all = &data[index * capacity..(index + 1) * capacity];
        let mut frames = all[..produced.min(wanted)].to_vec();
        frames.resize(wanted, 0.0);
        frames
    };
    Ok(SoundData::new(sample_rate, channel(0), channel(1)))
}

/// Returns how many frames `frames` frames at `from` Hz are at `to` Hz,
/// rounded to the nearest whole number.
fn frames_at(frames: usize, from: u32, to: u32) -> usize {
    let from = u128::from(from);
    let wanted = (frames as u128 * u128::from(to) + from / 2) / from;
    usize::try_from(wanted).unwrap_or(usize::MAX)
}

/// Reads the audio file at `path` and changes its sample rate to
/// `sample_rate`: [`decode_file`] and [`resample`] in one. This is how a
/// sound effect is loaded for the output rate.
///
/// `max_duration` limits the length of the sound in the file (see
/// [`decode_file`]). The number of skipped packets is that of the decoding.
///
/// This reads files, allocates memory, and takes time: call it from a
/// control thread or a worker thread, never the audio thread.
///
/// # Errors
///
/// - [`MediaError::InvalidSampleRate`] if `sample_rate` is 0 (nothing is
///   read)
/// - Every error of [`decode_file`] and [`resample`]
/// - [`MediaError::Empty`] if the sound has no frame at the new rate
pub fn load_sound(
    path: &Path,
    sample_rate: u32,
    max_duration: Option<Duration>,
) -> Result<Decoded, MediaError> {
    if sample_rate == 0 {
        return Err(MediaError::InvalidSampleRate(0));
    }
    let decoded = decode_file(path, max_duration)?;
    let sound = resample(&decoded.sound, sample_rate)?;
    if sound.is_empty() {
        return Err(MediaError::Empty);
    }
    Ok(Decoded {
        sound,
        skipped_packets: decoded.skipped_packets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TempDir, WavBuilder, WavFormat, amplitude_at, fixture, sine};

    fn stereo(rate: u32, frames: usize) -> SoundData {
        SoundData::new(
            rate,
            sine(rate, 440.0, 0.5, frames),
            sine(rate, 660.0, 0.25, frames),
        )
    }

    /// The root mean square of the difference of two sounds, without the
    /// first and last tenth (where the edges of the sound are).
    fn difference(a: &[f32], b: &[f32]) -> f64 {
        assert_eq!(a.len(), b.len());
        let (start, end) = (a.len() / 10, a.len() - a.len() / 10);
        let sum: f64 = (start..end)
            .map(|n| (f64::from(a[n]) - f64::from(b[n])).powi(2))
            .sum();
        (sum / (end - start) as f64).sqrt()
    }

    // -- length -------------------------------------------------------------

    #[test]
    fn the_length_follows_the_ratio_of_the_rates() {
        let cases = [
            (44_100, 48_000, 44_100, 48_000),
            (48_000, 44_100, 48_000, 44_100),
            (44_100, 96_000, 44_100, 96_000),
            (96_000, 48_000, 96_000, 48_000),
            (22_050, 48_000, 22_050, 48_000),
            (48_000, 8_000, 48_000, 8_000),
            (44_100, 48_000, 1_000, 1_088),
            (48_000, 44_100, 1_000, 919),
            (48_000, 96_000, 777, 1_554),
        ];
        for (from, to, frames, expected) in cases {
            let out = resample(&stereo(from, frames), to).unwrap();
            assert_eq!(out.sample_rate(), to);
            assert_eq!(out.frames(), expected, "{from} -> {to}, {frames} frames");
            assert_eq!(out.left().len(), out.right().len());
        }
    }

    #[test]
    fn a_sound_shorter_than_one_chunk_is_resampled() {
        for frames in [1, 2, 10, 100, 1_023, 1_024, 1_025] {
            let out = resample(&stereo(44_100, frames), 48_000).unwrap();
            assert_eq!(out.frames(), frames_at(frames, 44_100, 48_000));
            assert!(out.left().iter().all(|x| x.is_finite()));
        }
    }

    #[test]
    fn frames_are_rounded_to_the_nearest() {
        assert_eq!(frames_at(1, 44_100, 48_000), 1);
        assert_eq!(frames_at(1_000, 44_100, 48_000), 1_088);
        assert_eq!(frames_at(1_000, 48_000, 44_100), 919);
        assert_eq!(frames_at(1, 96_000, 8_000), 0);
        assert_eq!(frames_at(6, 96_000, 8_000), 1);
        assert_eq!(frames_at(0, 44_100, 48_000), 0);
    }

    // -- the sound ----------------------------------------------------------

    #[test]
    fn the_frequency_and_level_of_each_channel_are_kept() {
        let cases = [
            (44_100, 48_000),
            (48_000, 44_100),
            (44_100, 96_000),
            (96_000, 48_000),
            (22_050, 48_000),
            (48_000, 96_000),
        ];
        for (from, to) in cases {
            let out = resample(&stereo(from, from as usize), to).unwrap();
            let rate = to;
            assert!(
                (amplitude_at(out.left(), rate, 440.0) - 0.5).abs() < 0.01,
                "{from} -> {to}: left 440 Hz"
            );
            assert!(
                (amplitude_at(out.right(), rate, 660.0) - 0.25).abs() < 0.01,
                "{from} -> {to}: right 660 Hz"
            );
            // Not the other channel's sound, and no new frequencies.
            assert!(amplitude_at(out.left(), rate, 660.0) < 0.01);
            assert!(amplitude_at(out.right(), rate, 440.0) < 0.01);
            assert!(amplitude_at(out.left(), rate, 880.0) < 0.01);
        }
    }

    #[test]
    fn the_result_is_the_same_wave_at_the_same_moments() {
        // The delay of the resampler is removed: the resampled wave is the
        // wave made directly at the new rate.
        for (from, to) in [(44_100, 48_000), (48_000, 44_100), (48_000, 96_000)] {
            let out = resample(&stereo(from, from as usize), to).unwrap();
            let ideal = stereo(to, out.frames());
            assert!(
                difference(out.left(), ideal.left()) < 0.01,
                "{from} -> {to}: {}",
                difference(out.left(), ideal.left())
            );
            assert!(difference(out.right(), ideal.right()) < 0.01);
        }
    }

    #[test]
    fn a_frequency_above_the_new_nyquist_frequency_does_not_come_back_as_an_alias() {
        // 6 kHz at 48 kHz, down to 8 kHz (Nyquist 4 kHz): without a filter it
        // would come back as a tone of 2 kHz.
        let tone = SoundData::new(
            48_000,
            sine(48_000, 6_000.0, 0.5, 48_000),
            sine(48_000, 6_000.0, 0.5, 48_000),
        );
        let out = resample(&tone, 8_000).unwrap();
        assert!(amplitude_at(out.left(), 8_000, 2_000.0) < 0.02);
    }

    #[test]
    fn a_silent_sound_stays_silent() {
        let silent = SoundData::new(44_100, vec![0.0; 5_000], vec![0.0; 5_000]);
        let out = resample(&silent, 48_000).unwrap();
        assert!(out.left().iter().all(|x| x.abs() < 1e-6));
    }

    #[test]
    fn the_same_rate_gives_the_same_sound() {
        let sound = stereo(48_000, 3_000);
        assert_eq!(resample(&sound, 48_000).unwrap(), sound);
    }

    #[test]
    fn going_there_and_back_gives_the_wave_again() {
        let sound = stereo(48_000, 48_000);
        let there = resample(&sound, 44_100).unwrap();
        let back = resample(&there, 48_000).unwrap();
        assert_eq!(back.frames(), sound.frames());
        assert!(difference(back.left(), sound.left()) < 0.01);
    }

    // -- edges ----------------------------------------------------------------

    #[test]
    fn an_empty_sound_is_empty_at_the_new_rate() {
        let empty = SoundData::new(44_100, Vec::new(), Vec::new());
        let out = resample(&empty, 48_000).unwrap();
        assert!(out.is_empty());
        assert_eq!(out.sample_rate(), 48_000);
    }

    #[test]
    fn a_sound_with_no_frame_at_the_new_rate_is_empty() {
        let one = SoundData::new(96_000, vec![0.5], vec![0.5]);
        let out = resample(&one, 8_000).unwrap();
        assert!(out.is_empty());
        assert_eq!(out.sample_rate(), 8_000);
    }

    #[test]
    fn a_new_rate_of_0_is_refused() {
        let error = resample(&stereo(48_000, 100), 0).unwrap_err();
        assert!(matches!(error, MediaError::InvalidSampleRate(0)), "{error}");
        // Also for a sound that is empty.
        let empty = SoundData::new(44_100, Vec::new(), Vec::new());
        assert!(matches!(
            resample(&empty, 0).unwrap_err(),
            MediaError::InvalidSampleRate(0)
        ));
    }

    #[test]
    fn unusual_rates_give_a_sound_or_an_error_and_never_a_panic() {
        let sound = stereo(48_000, 10);
        for rate in [1, 2, 3, 7, 11_025, 192_000, 384_000] {
            if let Ok(out) = resample(&sound, rate) {
                assert_eq!(out.sample_rate(), rate);
                assert_eq!(out.frames(), frames_at(10, 48_000, rate));
            }
        }
    }

    #[test]
    fn a_rate_that_would_ask_for_too_much_memory_is_refused_before_anything_is_made() {
        // One second at 48 kHz, at the highest possible rate: 4 billion frames.
        let sound = stereo(48_000, 48_000);
        let error = resample(&sound, u32::MAX).unwrap_err();
        assert!(matches!(error, MediaError::Resample(_)), "{error}");
        assert!(error.to_string().contains("more than the limit"), "{error}");

        // The limit itself is allowed: 2^26 frames at 48 kHz is 23 minutes of
        // sound, too long to make here, so the edge is checked on the number.
        assert_eq!(frames_at(48_000, 48_000, 48_000), 48_000);
        assert!(frames_at(48_000, 48_000, u32::MAX) > MAX_OUTPUT_FRAMES);
        assert!(frames_at(60 * 48_000, 48_000, 192_000) < MAX_OUTPUT_FRAMES);
    }

    // -- load_sound -----------------------------------------------------------

    #[test]
    fn a_file_is_read_and_resampled() {
        let decoded = load_sound(&fixture("tone-44k-stereo-id3.mp3"), 48_000, None).unwrap();
        assert_eq!(decoded.skipped_packets, 0);
        let sound = decoded.sound;
        assert_eq!(sound.sample_rate(), 48_000);
        assert_eq!(sound.frames(), frames_at(22_050, 44_100, 48_000));
        assert_eq!(sound.frames(), 24_000);
        assert!((amplitude_at(sound.left(), 48_000, 440.0) - 0.5).abs() < 0.03);
        assert!((amplitude_at(sound.right(), 48_000, 660.0) - 0.5).abs() < 0.03);
    }

    #[test]
    fn a_file_at_the_wanted_rate_is_read_as_it_is() {
        let path = fixture("tone-48k-stereo-gapless.mp3");
        let loaded = load_sound(&path, 48_000, None).unwrap();
        assert_eq!(loaded, decode_file(&path, None).unwrap());
    }

    #[test]
    fn the_limit_is_on_the_sound_in_the_file_and_the_skipped_packets_come_through() {
        let path = fixture("tone-44k-stereo-id3.mp3");
        // 0.5 seconds long.
        assert!(load_sound(&path, 48_000, Some(Duration::from_millis(500))).is_ok());
        let error = load_sound(&path, 48_000, Some(Duration::from_millis(400))).unwrap_err();
        assert!(matches!(error, MediaError::TooLong { .. }), "{error}");

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[1_367..1_371].fill(0xFF);
        let dir = TempDir::new();
        let damaged = dir.write("damaged.mp3", &bytes);
        assert_eq!(
            load_sound(&damaged, 48_000, None).unwrap().skipped_packets,
            1
        );
    }

    #[test]
    fn nothing_is_read_for_a_new_rate_of_0() {
        // The file does not exist: the rate is refused first.
        let dir = TempDir::new();
        let error = load_sound(&dir.path().join("none.wav"), 0, None).unwrap_err();
        assert!(matches!(error, MediaError::InvalidSampleRate(0)), "{error}");
    }

    #[test]
    fn the_errors_of_reading_come_through() {
        let dir = TempDir::new();
        let missing = load_sound(&dir.path().join("none.wav"), 48_000, None).unwrap_err();
        assert!(matches!(missing, MediaError::Io(_)), "{missing}");

        let text = dir.write("text.wav", b"not audio at all");
        assert!(load_sound(&text, 48_000, None).is_err());
    }

    #[test]
    fn a_wav_file_with_a_japanese_name_is_loaded_for_the_output_rate() {
        let dir = TempDir::new();
        let bytes = WavBuilder::new(
            WavFormat::I16,
            44_100,
            vec![
                sine(44_100, 440.0, 0.5, 44_100),
                sine(44_100, 660.0, 0.25, 44_100),
            ],
        )
        .build();
        let path = dir.write("効果音.wav", &bytes);
        let sound = load_sound(&path, 48_000, None).unwrap().sound;
        assert_eq!(sound.sample_rate(), 48_000);
        assert_eq!(sound.frames(), 48_000);
        assert!((amplitude_at(sound.left(), 48_000, 440.0) - 0.5).abs() < 0.01);
    }
}
