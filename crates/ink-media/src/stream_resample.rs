//! Changing the sample rate of a sound a little at a time.
//!
//! [`resample`](crate::resample()) changes the rate of a whole sound that is
//! in memory. A song is too long for that (and its length has no limit), so
//! [`StreamResampler`] takes the frames as the decoder makes them and gives
//! back the frames of the new rate as they are ready. It does the same
//! calculation as the whole-sound version: for the same sound, the result is
//! the same, bit for bit.
//!
//! # The delay
//!
//! The resampler (rubato, FFT) gives out a little silence first, as long as
//! its delay ([`output_delay`](rubato::Resampler::output_delay), 160 frames
//! from 44.1 to 48 kHz, up to half a second for rates with a small common
//! divisor). That silence is cut, so the sound starts at the same moment as
//! the input. After [`StreamResampler::reset`] (the start of the next loop),
//! the delay comes again, and is cut again.
//!
//! # The length
//!
//! A pass (from the start or a reset to [`finish`](StreamResampler::finish))
//! gives exactly `frames_at(frames in, from, to)` frames: the length times
//! the ratio, rounded to the nearest frame, the same rule as the whole-sound
//! version. The frames before the end never pass that length, so the end only
//! has to push out what is left and cut.
//!
//! This is for the worker thread, never the audio thread.

use rubato::audioadapter::{Adapter, AdapterMut};
use rubato::audioadapter_buffers::owned::SequentialOwned;
use rubato::{Fft, FixedSync, Indexing, Resampler};

use crate::MediaError;
use crate::resample::{CHUNK_FRAMES, frames_at};

/// The most calls `finish` makes to push the end out. Rates with a small
/// common divisor need about a hundred (a delay of half a second); more than
/// this means the resampler is not giving the end of the sound.
const MAX_FINISH_CALLS: usize = 100_000;

/// Changes the sample rate of a stereo sound a little at a time.
pub(crate) struct StreamResampler {
    resampler: Fft<f32>,
    from: u32,
    to: u32,
    /// The chunk that goes into the resampler (2 channels).
    input: SequentialOwned<f32>,
    /// What the resampler gives out for one chunk (2 channels).
    output: SequentialOwned<f32>,
    /// Frames that came in and do not make a whole chunk yet.
    pending_left: Vec<f32>,
    pending_right: Vec<f32>,
    /// The delay of the resampler, in frames of the new rate.
    delay: usize,
    /// How much of the delay is still to be cut in this pass.
    delay_left: usize,
    frames_in: u64,
    frames_out: u64,
    finished: bool,
}

impl StreamResampler {
    /// Makes the resampler for `from` -> `to` Hz. It works on chunks of 1,024
    /// frames and 2 channels. This allocates (about 40 to 140 KiB for the
    /// usual rates, up to about 13 MiB for rates with a small common
    /// divisor), and nothing later does, except the vectors given to
    /// [`push`](Self::push) and [`finish`](Self::finish).
    ///
    /// # Errors
    ///
    /// - [`MediaError::InvalidSampleRate`] if a rate is 0
    /// - [`MediaError::Resample`] if the resampler cannot be made for the two
    ///   rates
    pub(crate) fn new(from: u32, to: u32) -> Result<Self, MediaError> {
        if from == 0 || to == 0 {
            return Err(MediaError::InvalidSampleRate(0));
        }
        let resampler = Fft::<f32>::new(
            from as usize,
            to as usize,
            CHUNK_FRAMES,
            2,
            FixedSync::Input,
        )
        .map_err(|e| MediaError::Resample(e.to_string()))?;
        let delay = resampler.output_delay();
        let input = SequentialOwned::<f32>::new(0.0, 2, resampler.input_frames_max());
        let output = SequentialOwned::<f32>::new(0.0, 2, resampler.output_frames_max());
        let chunk = resampler.input_frames_max();
        Ok(Self {
            resampler,
            from,
            to,
            input,
            output,
            pending_left: Vec::with_capacity(chunk),
            pending_right: Vec::with_capacity(chunk),
            delay,
            delay_left: delay,
            frames_in: 0,
            frames_out: 0,
            finished: false,
        })
    }

    /// Goes back to the start of a pass, as new: the state of the
    /// resampler, the frames waiting, the delay that is to be cut, and the
    /// counts. Call it at the start of every loop.
    pub(crate) fn reset(&mut self) {
        self.resampler.reset();
        self.pending_left.clear();
        self.pending_right.clear();
        self.delay_left = self.delay;
        self.frames_in = 0;
        self.frames_out = 0;
        self.finished = false;
    }

    /// Takes frames, any number of them, and appends the frames of the new
    /// rate that are ready to `out_left` and `out_right`. Nothing may be
    /// ready yet (the first chunk, or the delay).
    ///
    /// `left` and `right` have the same length (if not, the shorter one
    /// counts).
    ///
    /// # Errors
    ///
    /// - [`MediaError::Resample`] if the pass was finished and
    ///   [`reset`](Self::reset) was not called, or the resampler fails
    pub(crate) fn push(
        &mut self,
        left: &[f32],
        right: &[f32],
        out_left: &mut Vec<f32>,
        out_right: &mut Vec<f32>,
    ) -> Result<(), MediaError> {
        if self.finished {
            return Err(MediaError::Resample(
                "frames were given after the end of the sound; reset first".to_string(),
            ));
        }
        let frames = left.len().min(right.len());
        self.frames_in += frames as u64;
        let chunk = self.resampler.input_frames_next();
        let mut offset = 0;
        while offset < frames {
            let take = (chunk - self.pending_left.len()).min(frames - offset);
            self.pending_left
                .extend_from_slice(&left[offset..offset + take]);
            self.pending_right
                .extend_from_slice(&right[offset..offset + take]);
            offset += take;
            if self.pending_left.len() == chunk {
                self.process(chunk, None, out_left, out_right)?;
                self.pending_left.clear();
                self.pending_right.clear();
            }
        }
        Ok(())
    }

    /// Tells that the sound has ended, and appends what is left, so that the
    /// pass has exactly `frames_at(frames in, from, to)` frames. After it,
    /// only [`reset`](Self::reset) starts a new pass. Calling it again does
    /// nothing.
    ///
    /// # Errors
    ///
    /// - [`MediaError::Resample`] if the resampler fails or does not give the
    ///   end of the sound
    pub(crate) fn finish(
        &mut self,
        out_left: &mut Vec<f32>,
        out_right: &mut Vec<f32>,
    ) -> Result<(), MediaError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let wanted = u64::try_from(frames_at(
            usize::try_from(self.frames_in).unwrap_or(usize::MAX),
            self.from,
            self.to,
        ))
        .unwrap_or(u64::MAX);
        if wanted == 0 {
            self.pending_left.clear();
            self.pending_right.clear();
            return Ok(());
        }
        // The frames waiting: a chunk that is not whole.
        if !self.pending_left.is_empty() {
            let count = self.pending_left.len();
            self.process(count, Some(wanted), out_left, out_right)?;
            self.pending_left.clear();
            self.pending_right.clear();
        }
        // Then silence, to push the rest out of the resampler.
        let mut calls = 0;
        while self.frames_out < wanted {
            calls += 1;
            if calls > MAX_FINISH_CALLS {
                return Err(MediaError::Resample(
                    "the resampler did not give the end of the sound".to_string(),
                ));
            }
            self.process(0, Some(wanted), out_left, out_right)?;
        }
        Ok(())
    }

    /// Returns the rate of the frames that come in.
    #[cfg(test)]
    pub(crate) fn input_rate(&self) -> u32 {
        self.from
    }

    /// Returns the rate of the frames that go out.
    #[cfg(test)]
    pub(crate) fn output_rate(&self) -> u32 {
        self.to
    }

    /// Returns the frames that came in during this pass.
    #[cfg(test)]
    pub(crate) fn frames_in(&self) -> u64 {
        self.frames_in
    }

    /// Returns the frames that went out during this pass.
    #[cfg(test)]
    pub(crate) fn frames_out(&self) -> u64 {
        self.frames_out
    }

    /// Runs the resampler on the first `count` frames waiting (`None`: a
    /// whole chunk) and appends what comes out, without the delay, and not
    /// more than `limit` frames in the pass.
    fn process(
        &mut self,
        count: usize,
        limit: Option<u64>,
        out_left: &mut Vec<f32>,
        out_right: &mut Vec<f32>,
    ) -> Result<(), MediaError> {
        for (frame, (&l, &r)) in self
            .pending_left
            .iter()
            .zip(&self.pending_right)
            .take(count)
            .enumerate()
        {
            self.input.write_sample(0, frame, &l);
            self.input.write_sample(1, frame, &r);
        }
        let partial = Indexing::new().partial_len(count);
        let whole = count == self.resampler.input_frames_next();
        let indexing = if whole { None } else { Some(&partial) };
        let (_used, made) = self
            .resampler
            .process_into_buffer(&self.input, &mut self.output, indexing)
            .map_err(|e| MediaError::Resample(e.to_string()))?;
        for frame in 0..made {
            if self.delay_left > 0 {
                self.delay_left -= 1;
                continue;
            }
            if limit.is_some_and(|limit| self.frames_out >= limit) {
                break;
            }
            // The frame is inside the buffer, so a sample is always there.
            out_left.push(self.output.read_sample(0, frame).unwrap_or(0.0));
            out_right.push(self.output.read_sample(1, frame).unwrap_or(0.0));
            self.frames_out += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ink_core::graph::SoundData;

    use super::*;
    use crate::resample;
    use crate::test_util::{amplitude_at, sine};

    /// A stereo test sound: different waves in the two channels.
    fn waves(rate: u32, frames: usize) -> (Vec<f32>, Vec<f32>) {
        (
            sine(rate, 1_000.0, 0.5, frames),
            sine(rate, 2_345.0, 0.25, frames),
        )
    }

    /// Runs a whole pass: pushes the sound in pieces of the given sizes (one
    /// after the other, again and again), then finishes.
    fn pass(
        resampler: &mut StreamResampler,
        left: &[f32],
        right: &[f32],
        sizes: &[usize],
    ) -> (Vec<f32>, Vec<f32>) {
        let (mut out_left, mut out_right) = (Vec::new(), Vec::new());
        let mut at = 0;
        let mut which = 0;
        while at < left.len() {
            let size = sizes[which % sizes.len()].min(left.len() - at);
            resampler
                .push(
                    &left[at..at + size],
                    &right[at..at + size],
                    &mut out_left,
                    &mut out_right,
                )
                .unwrap();
            at += size;
            which += 1;
        }
        resampler.finish(&mut out_left, &mut out_right).unwrap();
        (out_left, out_right)
    }

    fn bits(samples: &[f32]) -> Vec<u32> {
        samples.iter().map(|s| s.to_bits()).collect()
    }

    /// Rates the tests use: the usual ones.
    const PAIRS: [(u32, u32); 8] = [
        (44_100, 48_000),
        (48_000, 44_100),
        (44_100, 96_000),
        (22_050, 48_000),
        (8_000, 48_000),
        (96_000, 48_000),
        (32_000, 48_000),
        (48_000, 22_050),
    ];

    #[test]
    fn it_gives_what_the_whole_sound_version_gives_bit_for_bit() {
        for (from, to) in PAIRS {
            for total in [1, 100, 1_023, 1_024, 1_025, 5_000, 20_000] {
                let (left, right) = waves(from, total);
                let whole =
                    resample(&SoundData::new(from, left.clone(), right.clone()), to).unwrap();
                let mut resampler = StreamResampler::new(from, to).unwrap();
                let (out_left, out_right) = pass(&mut resampler, &left, &right, &[total]);
                assert_eq!(out_left.len(), whole.frames(), "{from}->{to} {total}");
                assert_eq!(bits(&out_left), bits(whole.left()), "{from}->{to} {total}");
                assert_eq!(
                    bits(&out_right),
                    bits(whole.right()),
                    "{from}->{to} {total}"
                );
            }
        }
    }

    #[test]
    fn it_gives_what_the_whole_sound_version_gives_for_rates_with_a_small_common_divisor() {
        // 44,101 -> 48,000 needs a delay of half a second and 5 MiB.
        for total in [1, 3_000, 30_000] {
            let (left, right) = waves(44_101, total);
            let whole =
                resample(&SoundData::new(44_101, left.clone(), right.clone()), 48_000).unwrap();
            let mut resampler = StreamResampler::new(44_101, 48_000).unwrap();
            let (out_left, out_right) = pass(&mut resampler, &left, &right, &[1_000]);
            assert_eq!(out_left.len(), frames_at(total, 44_101, 48_000));
            assert_eq!(bits(&out_left), bits(whole.left()), "{total}");
            assert_eq!(bits(&out_right), bits(whole.right()), "{total}");
        }
    }

    #[test]
    fn the_pieces_the_sound_comes_in_do_not_matter() {
        let (left, right) = waves(44_100, 12_345);
        let mut resampler = StreamResampler::new(44_100, 48_000).unwrap();
        let (whole_left, whole_right) = pass(&mut resampler, &left, &right, &[12_345]);
        for sizes in [
            &[1][..],
            &[7],
            &[1_000],
            &[1_024],
            &[1_500],
            &[1, 2, 3, 1_023, 1_024, 1_025, 4_000],
        ] {
            resampler.reset();
            let (out_left, out_right) = pass(&mut resampler, &left, &right, sizes);
            assert_eq!(bits(&out_left), bits(&whole_left), "{sizes:?}");
            assert_eq!(bits(&out_right), bits(&whole_right), "{sizes:?}");
        }
    }

    #[test]
    fn a_pass_has_exactly_the_length_times_the_ratio_rounded_to_the_nearest() {
        for (from, to) in PAIRS {
            for total in [0, 1, 2, 100, 1_023, 1_024, 1_025, 2_048, 5_000, 44_100] {
                let (left, right) = waves(from, total);
                let mut resampler = StreamResampler::new(from, to).unwrap();
                let (out_left, out_right) = pass(&mut resampler, &left, &right, &[333]);
                let wanted =
                    (total as u128 * u128::from(to) + u128::from(from) / 2) / u128::from(from);
                assert_eq!(out_left.len() as u128, wanted, "{from}->{to} {total}");
                assert_eq!(out_right.len(), out_left.len());
                assert_eq!(resampler.frames_in(), total as u64);
                assert_eq!(resampler.frames_out() as u128, wanted);
            }
        }
        // 44,100 frames from 44.1 to 48 kHz are 48,000, though the resampler
        // itself would make 48,001.
        let mut resampler = StreamResampler::new(44_100, 48_000).unwrap();
        let (left, right) = waves(44_100, 44_100);
        assert_eq!(
            pass(&mut resampler, &left, &right, &[4_410]).0.len(),
            48_000
        );
    }

    #[test]
    fn a_loop_gives_the_same_pass_every_time_with_no_gap_between() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (44_100, 96_000),
            (44_101, 48_000),
        ] {
            let total = 20_000;
            let (left, right) = waves(from, total);
            let mut resampler = StreamResampler::new(from, to).unwrap();
            let wanted = frames_at(total, from, to);
            let (first_left, first_right) = pass(&mut resampler, &left, &right, &[4_096]);
            // The passes together, as the worker puts them in the ring
            // buffer.
            let mut all_left = first_left.clone();
            for round in 1..=3 {
                resampler.reset();
                // Each pass is cut into other pieces.
                let (out_left, out_right) = pass(&mut resampler, &left, &right, &[round * 999]);
                assert_eq!(out_left.len(), wanted, "{from}->{to} pass {round}");
                assert_eq!(
                    bits(&out_left),
                    bits(&first_left),
                    "{from}->{to} pass {round}"
                );
                assert_eq!(
                    bits(&out_right),
                    bits(&first_right),
                    "{from}->{to} pass {round}"
                );
                all_left.extend(out_left);
            }
            // The start of every pass is the start of the sound: nothing of
            // the delay is left as silence between the passes.
            for round in 1..=3 {
                let start = round * wanted;
                assert_eq!(
                    bits(&all_left[start..start + 2_000]),
                    bits(&first_left[..2_000]),
                    "{from}->{to}"
                );
            }
        }
    }

    #[test]
    fn the_delay_is_cut_and_the_sound_starts_at_the_same_moment() {
        // A sine of 1 kHz that starts at 0: after the delay is cut, the
        // output is the same sine at the new rate.
        let (left, right) = waves(44_100, 20_000);
        let mut resampler = StreamResampler::new(44_100, 48_000).unwrap();
        let (out_left, _) = pass(&mut resampler, &left, &right, &[1_024]);
        let worst = out_left
            .iter()
            .take(8_000)
            .enumerate()
            .map(|(n, &x)| {
                let t = n as f64 / 48_000.0;
                (f64::from(x) - 0.5 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()).abs()
            })
            .fold(0.0, f64::max);
        assert!(worst < 0.01, "{worst}");
        let level = amplitude_at(&out_left[2_000..10_000], 48_000, 1_000.0);
        assert!((level - 0.5).abs() < 0.01, "{level}");
    }

    #[test]
    fn what_comes_out_before_the_end_never_passes_the_length_of_the_pass() {
        // 10 rate pairs, 1 to 200 chunks each: 2,000 cases.
        let pairs = [
            (44_100, 48_000),
            (48_000, 44_100),
            (44_100, 96_000),
            (22_050, 48_000),
            (8_000, 48_000),
            (96_000, 48_000),
            (32_000, 48_000),
            (48_000, 22_050),
            (11_025, 48_000),
            (88_200, 48_000),
        ];
        let silence = vec![0.0_f32; CHUNK_FRAMES];
        let mut cases = 0;
        for (from, to) in pairs {
            let mut resampler = StreamResampler::new(from, to).unwrap();
            let (mut out_left, mut out_right) = (Vec::new(), Vec::new());
            for chunks in 1..=200_usize {
                resampler
                    .push(&silence, &silence, &mut out_left, &mut out_right)
                    .unwrap();
                let wanted = frames_at(chunks * CHUNK_FRAMES, from, to);
                assert!(
                    out_left.len() <= wanted,
                    "{from}->{to} after {chunks} chunks: {} > {wanted}",
                    out_left.len()
                );
                cases += 1;
            }
        }
        assert_eq!(cases, 2_000);
    }

    #[test]
    fn frames_given_after_the_end_are_refused_until_reset() {
        let (left, right) = waves(44_100, 3_000);
        let mut resampler = StreamResampler::new(44_100, 48_000).unwrap();
        let (mut out_left, mut out_right) = (Vec::new(), Vec::new());
        resampler
            .push(&left, &right, &mut out_left, &mut out_right)
            .unwrap();
        resampler.finish(&mut out_left, &mut out_right).unwrap();
        let error = resampler
            .push(&left, &right, &mut out_left, &mut out_right)
            .unwrap_err();
        assert!(matches!(error, MediaError::Resample(_)), "{error:?}");
        // Finishing again changes nothing.
        let length = out_left.len();
        resampler.finish(&mut out_left, &mut out_right).unwrap();
        assert_eq!(out_left.len(), length);

        resampler.reset();
        resampler
            .push(&left, &right, &mut out_left, &mut out_right)
            .unwrap();
    }

    #[test]
    fn a_pass_with_no_frames_gives_nothing_and_the_next_pass_is_normal() {
        let mut resampler = StreamResampler::new(44_100, 48_000).unwrap();
        let (mut out_left, mut out_right) = (Vec::new(), Vec::new());
        resampler.finish(&mut out_left, &mut out_right).unwrap();
        assert!(out_left.is_empty() && out_right.is_empty());
        assert_eq!((resampler.frames_in(), resampler.frames_out()), (0, 0));

        resampler.reset();
        let (left, right) = waves(44_100, 5_000);
        let (out_left, _) = pass(&mut resampler, &left, &right, &[5_000]);
        assert_eq!(out_left.len(), frames_at(5_000, 44_100, 48_000));
    }

    #[test]
    fn a_rate_of_0_is_refused() {
        for (from, to) in [(0, 48_000), (44_100, 0), (0, 0)] {
            let error = StreamResampler::new(from, to).err().unwrap();
            assert!(
                matches!(error, MediaError::InvalidSampleRate(0)),
                "{error:?}"
            );
        }
    }

    #[test]
    fn the_same_rate_works_and_keeps_the_length() {
        let (left, right) = waves(48_000, 5_000);
        let mut resampler = StreamResampler::new(48_000, 48_000).unwrap();
        let (out_left, _) = pass(&mut resampler, &left, &right, &[700]);
        assert_eq!(out_left.len(), 5_000);
        assert_eq!(resampler.input_rate(), 48_000);
        assert_eq!(resampler.output_rate(), 48_000);
    }

    #[test]
    fn a_sound_with_a_different_length_in_the_two_channels_uses_the_shorter() {
        let (left, right) = waves(44_100, 3_000);
        let mut resampler = StreamResampler::new(44_100, 48_000).unwrap();
        let (mut out_left, mut out_right) = (Vec::new(), Vec::new());
        resampler
            .push(&left, &right[..2_000], &mut out_left, &mut out_right)
            .unwrap();
        assert_eq!(resampler.frames_in(), 2_000);
        resampler.finish(&mut out_left, &mut out_right).unwrap();
        assert_eq!(out_left.len(), frames_at(2_000, 44_100, 48_000));
        assert_eq!(out_right.len(), out_left.len());
    }

    #[test]
    fn the_resampler_can_be_used_on_a_worker_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<StreamResampler>();
    }
}
