//! Playing a long sound file a little at a time (ADR-0009, Issue 63).
//!
//! [`SoundStream`] is what the BGM worker thread pulls from. It reads the
//! file with [`SoundReader`], changes the sample rate to the output rate with
//! `StreamResampler` when the two differ, and hands out the frames in chunks
//! of [`BGM_WRITE_CHUNK_FRAMES`]. It has no thread of its own: the worker
//! calls [`next_chunk`](SoundStream::next_chunk) when the ring buffer has
//! room, so a test can drive it without any thread.
//!
//! # The end of a pass and the loop
//!
//! A *pass* is the song from its start to its end. When the song ends and
//! the caller asks for a loop, the stream goes back to the start
//! ([`SoundReader::rewind`], then the resampler is reset) and goes on, so a
//! pass always has the same number of frames at the output rate. A chunk
//! never spans two passes: the first chunk of a new pass says so with
//! [`Chunk::looped`].
//!
//! # The seam of a loop
//!
//! Where the end of a song meets its start there can be a step in the wave,
//! which is heard as a click. The first 1.5 ms of every pass after the first
//! (72 frames at 48 kHz, 66 at 44.1 kHz) is therefore moved by an offset that
//! shrinks to 0 in a straight line. The offset, for each channel, is the
//! difference between what the end of the last pass predicts for the next
//! frame (`2 × last − the one before`) and the first frame of the new pass.
//! The start of the song, and the start after a [`rewind`](SoundStream::rewind),
//! are not smoothed.
//!
//! An MP3 file without the encoder's delay in its header starts with a short
//! silence, which no smoothing hides: use a WAV file for a loop.
//!
//! # Errors
//!
//! When `next_chunk` fails, the stream is *dead*: it fails again until
//! [`rewind`](SoundStream::rewind) succeeds. The caller decides what the
//! failure means for the song (the BGM worker logs it and ends the song).
//!
//! This is for the worker thread, never the audio thread.

use std::path::Path;

use ink_core::graph::BGM_WRITE_CHUNK_FRAMES;

use crate::resample::frames_at;
use crate::stream_resample::StreamResampler;
use crate::{MediaError, SoundInfo, SoundReader};

/// How many frames are read from the file at a time.
const READ_FRAMES: usize = 4096;

/// How long the smoothing of a loop seam lasts, in seconds.
const SEAM_SECONDS: f64 = 0.0015;

/// Returns how many frames the smoothing of a seam lasts at `sample_rate`.
fn seam_frames(sample_rate: u32) -> usize {
    ((f64::from(sample_rate) * SEAM_SECONDS).round() as usize).max(1)
}

/// What [`SoundStream::next_chunk`] gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// How many frames were appended: at most [`BGM_WRITE_CHUNK_FRAMES`],
    /// and exactly that many unless the pass ends in this chunk. It is 0
    /// only when `ended` is set.
    pub frames: usize,
    /// This chunk is the start of a new pass: the song ended and began
    /// again.
    pub looped: bool,
    /// The song has ended and does not loop: this is the last chunk (it can
    /// be empty). Nothing more comes until [`SoundStream::rewind`].
    pub ended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Ended,
    /// An error happened; see the module doc.
    Dead,
}

/// The smoothing of a loop seam.
struct Seam {
    /// How long it lasts, in frames of the output rate.
    frames: usize,
    /// The last two frames of the previous pass, waiting for the first frame
    /// of the new pass to make the offset from.
    armed: Option<[[f32; 2]; 2]>,
    /// The offset at the first frame of the pass, for each channel.
    offset: [f32; 2],
    /// How many frames of the smoothing are left.
    left: usize,
}

impl Seam {
    fn clear(&mut self) {
        self.armed = None;
        self.offset = [0.0; 2];
        self.left = 0;
    }
}

/// A song file that is read, resampled and handed out a chunk at a time.
pub struct SoundStream {
    reader: SoundReader,
    /// `None` when the file is already at the output rate.
    resampler: Option<StreamResampler>,
    output_rate: u32,
    output_frames: Option<u64>,
    state: State,

    /// Scratch for what is read from the file.
    in_left: Vec<f32>,
    in_right: Vec<f32>,
    /// Frames of the output rate that are ready; `ready_at` is how many of
    /// them were handed out.
    ready_left: Vec<f32>,
    ready_right: Vec<f32>,
    ready_at: usize,

    /// The file and the resampler have given everything of this pass.
    pass_done: bool,
    /// Frames of this pass that were ready (handed out or not).
    pass_given: u64,
    /// Frames of this pass that were handed out.
    pass_emitted: u64,
    measured: Option<u64>,

    seam: Seam,
    /// The last two frames handed out: `[the one before, the last]`.
    tail: [[f32; 2]; 2],
    /// How many frames of `tail` are real (0, 1 or 2).
    tail_count: usize,
    skipped_reported: u64,
}

impl SoundStream {
    /// Opens the file at `path` for playing at `output_rate` Hz.
    ///
    /// This allocates (the resampler takes 40 to 140 KiB for the usual
    /// rates, and up to about 13 MiB for rates with a small common divisor).
    ///
    /// # Errors
    ///
    /// - everything [`SoundReader::open`] returns
    /// - [`MediaError::InvalidSampleRate`] if `output_rate` is 0
    /// - [`MediaError::Empty`] if the sound is so short that it has no frame
    ///   at `output_rate`
    /// - [`MediaError::Resample`] if the resampler cannot be made
    pub fn open(path: &Path, output_rate: u32) -> Result<Self, MediaError> {
        if output_rate == 0 {
            return Err(MediaError::InvalidSampleRate(0));
        }
        let reader = SoundReader::open(path)?;
        let from = reader.info().sample_rate();
        let output_frames = reader.info().frames().map(|frames| {
            frames_at(
                usize::try_from(frames).unwrap_or(usize::MAX),
                from,
                output_rate,
            ) as u64
        });
        if output_frames == Some(0) {
            return Err(MediaError::Empty);
        }
        let resampler = if from == output_rate {
            None
        } else {
            Some(StreamResampler::new(from, output_rate)?)
        };
        Ok(Self {
            reader,
            resampler,
            output_rate,
            output_frames,
            state: State::Running,
            in_left: Vec::with_capacity(READ_FRAMES),
            in_right: Vec::with_capacity(READ_FRAMES),
            ready_left: Vec::new(),
            ready_right: Vec::new(),
            ready_at: 0,
            pass_done: false,
            pass_given: 0,
            pass_emitted: 0,
            measured: None,
            seam: Seam {
                frames: seam_frames(output_rate),
                armed: None,
                offset: [0.0; 2],
                left: 0,
            },
            tail: [[0.0; 2]; 2],
            tail_count: 0,
            skipped_reported: 0,
        })
    }

    /// Returns what the file says about its audio (at the rate of the file).
    pub fn info(&self) -> &SoundInfo {
        self.reader.info()
    }

    /// Returns the sample rate of the frames this gives.
    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// Returns how many frames a pass has at the output rate, as far as the
    /// file says: `round(frames × output rate / file rate)`. It is `None`
    /// when the file does not say how long it is (an MP3 file without a
    /// header); see [`measured_frames`](Self::measured_frames).
    pub fn output_frames(&self) -> Option<u64> {
        self.output_frames
    }

    /// Returns how many frames the first pass really had, **at the output
    /// rate**, once the file was read to its end (a chunk can still be
    /// waiting to be handed out). Before that it is `None`.
    pub fn measured_frames(&self) -> Option<u64> {
        self.measured
    }

    /// Returns how many packets of the file could not be decoded since the
    /// last call of this function (the first call counts from the opening).
    pub fn take_skipped_packets(&mut self) -> u64 {
        let total = self.reader.skipped_packets();
        let new = total - self.skipped_reported;
        self.skipped_reported = total;
        new
    }

    /// Goes back to the start of the song, as new. Everything of the pass in
    /// progress is thrown away: the frames that are ready, what the
    /// resampler holds, the smoothing of a seam, and the frames kept for it.
    /// The start is not smoothed. It also brings a dead stream back to life
    /// when the file can be rewound.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be rewound; the stream is then
    /// dead.
    pub fn rewind(&mut self) -> Result<(), MediaError> {
        if let Err(e) = self.reader.rewind() {
            self.state = State::Dead;
            return Err(e);
        }
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
        }
        self.in_left.clear();
        self.in_right.clear();
        self.ready_left.clear();
        self.ready_right.clear();
        self.ready_at = 0;
        self.pass_done = false;
        self.pass_given = 0;
        self.pass_emitted = 0;
        self.seam.clear();
        self.tail = [[0.0; 2]; 2];
        self.tail_count = 0;
        self.state = State::Running;
        Ok(())
    }

    /// Appends the next chunk of the song to `left` and `right`.
    ///
    /// `looping` is asked at every call, so the caller can turn a loop on or
    /// off while the song plays: when the song ends with `looping` set, it
    /// starts again; if not, the song ends.
    ///
    /// It keeps reading until it has a whole chunk (or the pass ends), so a
    /// resampler with a long delay does not make it give an empty chunk. It
    /// gives an empty chunk only with [`Chunk::ended`] set.
    ///
    /// # Errors
    ///
    /// - [`MediaError::Empty`] if a pass of a loop had no frame at all
    ///   (otherwise the loop would turn without a sound)
    /// - an error of reading, decoding or resampling that cannot be skipped
    /// - [`MediaError::Decode`] if the stream is dead already
    ///
    /// After an error the stream is dead (see the module doc).
    pub fn next_chunk(
        &mut self,
        looping: bool,
        left: &mut Vec<f32>,
        right: &mut Vec<f32>,
    ) -> Result<Chunk, MediaError> {
        match self.state {
            State::Ended => {
                return Ok(Chunk {
                    frames: 0,
                    looped: false,
                    ended: true,
                });
            }
            State::Dead => {
                return Err(MediaError::Decode(
                    "the sound stopped earlier because of an error".to_string(),
                ));
            }
            State::Running => {}
        }
        let result = self.make_chunk(looping, left, right);
        if result.is_err() {
            self.state = State::Dead;
        }
        result
    }

    fn make_chunk(
        &mut self,
        looping: bool,
        left: &mut Vec<f32>,
        right: &mut Vec<f32>,
    ) -> Result<Chunk, MediaError> {
        // Frames that were handed out are not needed any more.
        if self.ready_at > 0 {
            self.ready_left.drain(..self.ready_at);
            self.ready_right.drain(..self.ready_at);
            self.ready_at = 0;
        }
        let mut looped = false;
        loop {
            let available = self.ready_left.len() - self.ready_at;
            if available >= BGM_WRITE_CHUNK_FRAMES {
                break;
            }
            if self.pass_done {
                if available > 0 {
                    // The rest of this pass; a chunk does not span two.
                    break;
                }
                if !looping {
                    self.state = State::Ended;
                    return Ok(Chunk {
                        frames: 0,
                        looped: false,
                        ended: true,
                    });
                }
                if self.pass_emitted == 0 {
                    return Err(MediaError::Empty);
                }
                self.start_next_pass()?;
                looped = true;
                continue;
            }
            self.fill()?;
        }

        let available = self.ready_left.len() - self.ready_at;
        let frames = available.min(BGM_WRITE_CHUNK_FRAMES);
        let ended = self.pass_done && !looping && frames == available;
        self.emit(frames, left, right);
        if ended {
            self.state = State::Ended;
        }
        Ok(Chunk {
            frames,
            looped,
            ended,
        })
    }

    /// Reads some of the file into the ready frames. At the end of the file,
    /// pushes the end of the resampler out and finishes the pass.
    fn fill(&mut self) -> Result<(), MediaError> {
        self.in_left.clear();
        self.in_right.clear();
        let read = self
            .reader
            .read(&mut self.in_left, &mut self.in_right, READ_FRAMES)?;
        let before = self.ready_left.len();
        if read == 0 {
            if let Some(resampler) = &mut self.resampler {
                resampler.finish(&mut self.ready_left, &mut self.ready_right)?;
            }
            self.pass_done = true;
        } else if let Some(resampler) = &mut self.resampler {
            resampler.push(
                &self.in_left,
                &self.in_right,
                &mut self.ready_left,
                &mut self.ready_right,
            )?;
        } else {
            self.ready_left.extend_from_slice(&self.in_left);
            self.ready_right.extend_from_slice(&self.in_right);
        }
        self.pass_given += (self.ready_left.len() - before) as u64;
        if self.pass_done && self.measured.is_none() {
            self.measured = Some(self.pass_given);
        }
        Ok(())
    }

    /// Goes back to the start of the file for a loop and arms the smoothing
    /// of the seam.
    fn start_next_pass(&mut self) -> Result<(), MediaError> {
        self.reader.rewind()?;
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
        }
        self.seam.left = 0;
        self.seam.armed = (self.tail_count >= 2).then_some(self.tail);
        self.pass_done = false;
        self.pass_given = 0;
        self.pass_emitted = 0;
        Ok(())
    }

    /// Moves `frames` ready frames to `left` and `right`, smoothing the seam
    /// at the start of a pass and keeping the last two frames.
    fn emit(&mut self, frames: usize, left: &mut Vec<f32>, right: &mut Vec<f32>) {
        let start = left.len();
        let range = self.ready_at..self.ready_at + frames;
        left.extend_from_slice(&self.ready_left[range.clone()]);
        right.extend_from_slice(&self.ready_right[range]);
        self.ready_at += frames;
        self.pass_emitted += frames as u64;

        for i in start..left.len() {
            if let Some([before, last]) = self.seam.armed.take() {
                self.seam.offset = [
                    2.0 * last[0] - before[0] - left[i],
                    2.0 * last[1] - before[1] - right[i],
                ];
                self.seam.left = self.seam.frames;
            }
            if self.seam.left > 0 {
                let weight = self.seam.left as f32 / self.seam.frames as f32;
                left[i] += self.seam.offset[0] * weight;
                right[i] += self.seam.offset[1] * weight;
                self.seam.left -= 1;
            }
            self.tail = [self.tail[1], [left[i], right[i]]];
            self.tail_count = (self.tail_count + 1).min(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TempDir, WavBuilder, WavFormat, amplitude_at, fixture, sine};
    use std::path::PathBuf;

    const CHUNK: usize = BGM_WRITE_CHUNK_FRAMES;

    type Pass = (Vec<f32>, Vec<f32>);

    /// A stereo float WAV file (the samples come back exactly).
    fn wav(dir: &TempDir, rate: u32, left: Vec<f32>, right: Vec<f32>) -> PathBuf {
        dir.write(
            "song.wav",
            &WavBuilder::new(WavFormat::F32, rate, vec![left, right]).build(),
        )
    }

    fn ramp(frames: usize, from: f32, to: f32) -> Vec<f32> {
        (0..frames)
            .map(|n| from + (to - from) * n as f32 / frames as f32)
            .collect()
    }

    fn tone(rate: u32, frames: usize) -> (Vec<f32>, Vec<f32>) {
        (
            sine(rate, 440.0, 0.5, frames),
            sine(rate, 660.0, 0.25, frames),
        )
    }

    fn next(stream: &mut SoundStream, looping: bool) -> (Chunk, Pass) {
        let (mut left, mut right) = (Vec::new(), Vec::new());
        let chunk = stream.next_chunk(looping, &mut left, &mut right).unwrap();
        assert_eq!(left.len(), chunk.frames);
        assert_eq!(right.len(), chunk.frames);
        (chunk, (left, right))
    }

    /// Plays the song once, without a loop. Returns the chunks' sizes too.
    fn play_once(stream: &mut SoundStream) -> (Pass, Vec<usize>) {
        let (mut left, mut right) = (Vec::new(), Vec::new());
        let mut sizes = Vec::new();
        loop {
            let (chunk, (l, r)) = next(stream, false);
            left.extend(l);
            right.extend(r);
            if chunk.frames > 0 {
                sizes.push(chunk.frames);
            }
            if chunk.ended {
                return ((left, right), sizes);
            }
            assert!(chunk.frames > 0, "an empty chunk that is not the end");
            assert!(sizes.len() < 100_000, "never ends");
        }
    }

    /// Plays `passes` whole passes with a loop.
    fn play_loop(stream: &mut SoundStream, passes: usize) -> Vec<Pass> {
        let mut all: Vec<Pass> = Vec::new();
        loop {
            let (chunk, (l, r)) = next(stream, true);
            assert!(!chunk.ended);
            assert!(chunk.frames > 0, "an empty chunk in a loop");
            if chunk.looped || all.is_empty() {
                if all.len() == passes {
                    return all;
                }
                all.push((Vec::new(), Vec::new()));
            }
            let pass = all.last_mut().unwrap();
            pass.0.extend(l);
            pass.1.extend(r);
            assert!(all.len() <= passes + 1);
        }
    }

    // -- one pass, no loop --------------------------------------------------

    #[test]
    fn a_wav_file_at_the_output_rate_comes_out_as_it_is_in_whole_chunks() {
        let dir = TempDir::new();
        let (left, right) = tone(48_000, 5_000);
        let path = wav(&dir, 48_000, left.clone(), right.clone());
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        assert_eq!(stream.output_rate(), 48_000);
        assert_eq!(stream.output_frames(), Some(5_000));
        assert_eq!(stream.measured_frames(), None);

        let (first, next_one) = (next(&mut stream, false), next(&mut stream, false));
        assert_eq!(first.0.frames, CHUNK);
        assert!(!first.0.ended && !first.0.looped);
        assert_eq!(next_one.0.frames, CHUNK);
        let last = next(&mut stream, false);
        assert_eq!(last.0.frames, 5_000 - 2 * CHUNK);
        assert!(last.0.ended, "the last chunk says it is the last");
        assert_eq!(stream.measured_frames(), Some(5_000));

        let mut got = first.1;
        got.0.extend(next_one.1.0);
        got.1.extend(next_one.1.1);
        got.0.extend(last.1.0);
        got.1.extend(last.1.1);
        assert_eq!(got.0, left);
        assert_eq!(got.1, right);

        // After the end, nothing comes.
        let (after, (l, _)) = next(&mut stream, false);
        assert_eq!(after.frames, 0);
        assert!(after.ended && l.is_empty());
    }

    #[test]
    fn a_song_that_ends_exactly_on_a_chunk_ends_with_an_empty_chunk() {
        let dir = TempDir::new();
        let (left, right) = tone(48_000, 2 * CHUNK);
        let path = wav(&dir, 48_000, left, right);
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        let (_, sizes) = play_once(&mut stream);
        assert_eq!(sizes, vec![CHUNK, CHUNK]);
    }

    #[test]
    fn the_rate_is_changed_to_the_output_rate_and_the_length_is_exact() {
        let dir = TempDir::new();
        for (from, to) in [
            (44_100_u32, 48_000_u32),
            (22_050, 48_000),
            (48_000, 44_100),
            (44_101, 48_000),
        ] {
            let frames = 20_000;
            let (left, right) = tone(from, frames);
            let path = wav(&dir, from, left, right);
            let mut stream = SoundStream::open(&path, to).unwrap();
            let wanted = frames_at(frames, from, to);
            assert_eq!(stream.output_frames(), Some(wanted as u64));
            let ((out_left, out_right), sizes) = play_once(&mut stream);
            assert_eq!(out_left.len(), wanted, "{from} -> {to}");
            assert_eq!(out_right.len(), wanted, "{from} -> {to}");
            assert!(sizes[..sizes.len() - 1].iter().all(|&n| n == CHUNK));
            // The pitch did not change, and the sound starts at once.
            let middle = &out_left[2_000..wanted - 2_000];
            assert!((amplitude_at(middle, to, 440.0) - 0.5).abs() < 0.01);
            assert!(amplitude_at(&out_right[2_000..wanted - 2_000], to, 660.0) > 0.2);
            assert_eq!(stream.measured_frames(), Some(wanted as u64));
        }
    }

    #[test]
    fn a_resampled_pass_is_the_same_as_resampling_the_whole_sound() {
        let dir = TempDir::new();
        let (left, right) = tone(44_100, 30_000);
        let path = wav(&dir, 44_100, left, right);
        let decoded = crate::decode_file(&path, None).unwrap().sound;
        let whole = crate::resample(&decoded, 48_000).unwrap();
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        let ((out_left, out_right), _) = play_once(&mut stream);
        assert_eq!(out_left, whole.left());
        assert_eq!(out_right, whole.right());
    }

    #[test]
    fn a_rate_with_a_long_delay_still_gives_a_chunk_at_the_first_call() {
        // 44,101 -> 48,000 has a delay of about 24,000 frames.
        let dir = TempDir::new();
        let (left, right) = tone(44_101, 60_000);
        let path = wav(&dir, 44_101, left, right);
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        let (chunk, _) = next(&mut stream, false);
        assert_eq!(chunk.frames, CHUNK, "read on until a whole chunk was made");
        let wanted = frames_at(60_000, 44_101, 48_000);
        let mut total = CHUNK;
        loop {
            let (chunk, _) = next(&mut stream, false);
            total += chunk.frames;
            if chunk.ended {
                break;
            }
        }
        assert_eq!(total, wanted);
    }

    #[test]
    fn a_song_shorter_than_the_delay_comes_out_with_the_right_length() {
        let dir = TempDir::new();
        for frames in [1_usize, 5, 100, 3_000] {
            let (left, right) = tone(44_101, frames);
            let path = wav(&dir, 44_101, left, right);
            let mut stream = SoundStream::open(&path, 48_000).unwrap();
            let wanted = frames_at(frames, 44_101, 48_000);
            let ((out, _), _) = play_once(&mut stream);
            assert_eq!(out.len(), wanted, "{frames} frames");
            assert_eq!(stream.measured_frames(), Some(wanted as u64));
        }
    }

    #[test]
    fn a_song_with_no_frame_at_the_output_rate_is_empty() {
        let dir = TempDir::new();
        let path = wav(&dir, 48_000, vec![0.5], vec![0.5]);
        let error = SoundStream::open(&path, 8_000).err().unwrap();
        assert!(matches!(error, MediaError::Empty), "{error}");
    }

    #[test]
    fn an_output_rate_of_0_is_refused() {
        let dir = TempDir::new();
        let (left, right) = tone(48_000, 100);
        let path = wav(&dir, 48_000, left, right);
        let error = SoundStream::open(&path, 0).err().unwrap();
        assert!(matches!(error, MediaError::InvalidSampleRate(0)), "{error}");
    }

    #[test]
    fn a_mono_mp3_file_gives_the_same_sound_on_both_channels() {
        let mut stream = SoundStream::open(&fixture("tone-44k-mono.mp3"), 48_000).unwrap();
        let ((left, right), _) = play_once(&mut stream);
        assert_eq!(left.len(), frames_at(22_050, 44_100, 48_000));
        assert_eq!(left, right);
        assert!(amplitude_at(&left[1_000..left.len() - 1_000], 48_000, 440.0) > 0.4);
    }

    #[test]
    fn the_length_of_an_mp3_file_without_a_header_is_found_by_playing_it() {
        let mut stream =
            SoundStream::open(&fixture("tone-48k-stereo-no-header.mp3"), 48_000).unwrap();
        let known = stream.output_frames();
        let ((left, _), _) = play_once(&mut stream);
        assert_eq!(stream.measured_frames(), Some(left.len() as u64));
        if let Some(known) = known {
            assert_eq!(known, left.len() as u64);
        }
    }

    // -- the loop -----------------------------------------------------------

    #[test]
    fn every_pass_of_a_loop_has_exactly_the_same_number_of_frames() {
        let dir = TempDir::new();
        for (from, to) in [
            (48_000_u32, 48_000_u32),
            (44_100, 48_000),
            (44_101, 48_000),
            (22_050, 48_000),
            (48_000, 44_100),
        ] {
            let frames = 6_000;
            let (left, right) = tone(from, frames);
            let path = wav(&dir, from, left, right);
            let mut stream = SoundStream::open(&path, to).unwrap();
            let wanted = frames_at(frames, from, to);
            let passes = play_loop(&mut stream, 4);
            assert_eq!(passes.len(), 4);
            for (n, (l, r)) in passes.iter().enumerate() {
                assert_eq!(l.len(), wanted, "{from} -> {to}, pass {n}");
                assert_eq!(r.len(), wanted, "{from} -> {to}, pass {n}");
            }
            // The start of the second pass is the start of the song again
            // (apart from the seam), not a silence.
            let seam = seam_frames(to);
            assert_eq!(passes[1].0[seam..], passes[0].0[seam..], "{from} -> {to}");
            assert_eq!(passes[3].0[seam..], passes[0].0[seam..], "{from} -> {to}");
        }
    }

    #[test]
    fn the_passes_of_mp3_files_have_the_same_length_and_no_delay_at_the_start() {
        for (name, rate, frames, to) in [
            (
                "tone-48k-stereo-gapless.mp3",
                48_000_u32,
                24_000_usize,
                48_000_u32,
            ),
            ("tone-48k-stereo-gapless.mp3", 48_000, 24_000, 44_100),
            ("tone-44k-stereo-id3.mp3", 44_100, 22_050, 48_000),
        ] {
            let mut stream = SoundStream::open(&fixture(name), to).unwrap();
            let wanted = frames_at(frames, rate, to);
            let passes = play_loop(&mut stream, 3);
            for (n, (l, _)) in passes.iter().enumerate() {
                assert_eq!(l.len(), wanted, "{name} -> {to}, pass {n}");
            }
            let seam = seam_frames(to);
            assert_eq!(passes[2].0[seam..], passes[0].0[seam..], "{name} -> {to}");
            // The cosine starts at its peak, so the sound begins at once.
            assert!(
                passes[0].0[..8].iter().any(|x| x.abs() > 0.3),
                "{name}: starts with a silence"
            );
            assert!(
                passes[1].0[..8].iter().any(|x| x.abs() > 0.3),
                "{name}: silence at the loop"
            );
        }
    }

    #[test]
    fn a_chunk_never_spans_two_passes_and_the_new_pass_says_so() {
        let dir = TempDir::new();
        let (left, right) = tone(48_000, 3_000);
        let path = wav(&dir, 48_000, left, right);
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        let mut seen = Vec::new();
        for _ in 0..5 {
            let (chunk, _) = next(&mut stream, true);
            seen.push((chunk.frames, chunk.looped));
        }
        assert_eq!(
            seen,
            vec![
                (CHUNK, false),
                (3_000 - CHUNK, false),
                (CHUNK, true),
                (3_000 - CHUNK, false),
                (CHUNK, true),
            ]
        );
    }

    #[test]
    fn a_loop_can_be_turned_off_in_the_middle_of_the_song() {
        let dir = TempDir::new();
        let (left, right) = tone(48_000, 5_000);
        let path = wav(&dir, 48_000, left, right);
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        let (first, _) = next(&mut stream, true);
        assert_eq!(first.frames, CHUNK);
        // From here on, the song ends at the end of this pass.
        let mut total = first.frames;
        loop {
            let (chunk, _) = next(&mut stream, false);
            assert!(!chunk.looped);
            total += chunk.frames;
            if chunk.ended {
                break;
            }
        }
        assert_eq!(total, 5_000);
    }

    #[test]
    fn a_loop_turned_off_after_the_last_chunk_of_a_pass_ends_the_song() {
        let dir = TempDir::new();
        let (left, right) = tone(48_000, 3_000);
        let path = wav(&dir, 48_000, left, right);
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        next(&mut stream, true);
        let (second, _) = next(&mut stream, true);
        assert!(!second.ended, "it was told to loop");
        let (end, _) = next(&mut stream, false);
        assert!(end.ended);
        assert_eq!(end.frames, 0);
    }

    // -- the seam -----------------------------------------------------------

    #[test]
    fn the_smoothing_lasts_1_5_ms_of_the_output_rate() {
        assert_eq!(seam_frames(48_000), 72);
        assert_eq!(seam_frames(44_100), 66);
        assert_eq!(seam_frames(96_000), 144);
        assert_eq!(seam_frames(8_000), 12);
        assert_eq!(seam_frames(100), 1);
    }

    /// The first frames of the second pass differ from the first pass by an
    /// offset that goes straight down to 0 in `seam_frames(rate)` frames.
    fn check_the_seam(rate: u32) {
        let dir = TempDir::new();
        // Ramps: the end of the song is far from its start.
        let left = ramp(1_000, 0.0, 0.5);
        let right = ramp(1_000, 0.25, -0.25);
        let path = wav(&dir, rate, left.clone(), right.clone());
        let mut stream = SoundStream::open(&path, rate).unwrap();
        let passes = play_loop(&mut stream, 3);
        let seam = seam_frames(rate);
        for (channel, source) in [(0, &left), (1, &right)] {
            let pass = |n: usize| {
                if channel == 0 {
                    &passes[n].0
                } else {
                    &passes[n].1
                }
            };
            let offset = 2.0 * source[999] - source[998] - source[0];
            assert_eq!(pass(0)[..], source[..], "the start is not smoothed");
            for (i, &original) in source.iter().enumerate() {
                let weight = if i < seam {
                    (seam - i) as f32 / seam as f32
                } else {
                    0.0
                };
                let wanted = original + offset * weight;
                assert!(
                    (pass(1)[i] - wanted).abs() < 1e-5,
                    "rate {rate}, channel {channel}, frame {i}: {} instead of {wanted}",
                    pass(1)[i]
                );
            }
            // The first frame is what the end of the song leads to.
            let predicted = 2.0 * source[999] - source[998];
            assert!((pass(1)[0] - predicted).abs() < 1e-5);
            // The third pass is smoothed in the same way.
            assert_eq!(pass(2)[..], pass(1)[..]);
        }
    }

    #[test]
    fn the_seam_of_a_loop_is_smoothed_in_72_frames_at_48_khz() {
        check_the_seam(48_000);
    }

    #[test]
    fn the_seam_of_a_loop_is_smoothed_in_66_frames_at_44_1_khz() {
        check_the_seam(44_100);
    }

    #[test]
    fn a_stop_in_the_middle_of_the_smoothing_leaves_nothing_for_the_next_song_start() {
        // A song shorter than the smoothing: when its second pass has been
        // handed out, 42 frames of the smoothing are still to come.
        let dir = TempDir::new();
        let left = ramp(30, 0.0, 0.5);
        let right = ramp(30, 0.25, -0.25);
        let path = wav(&dir, 48_000, left.clone(), right.clone());
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        let (first, plain) = next(&mut stream, true);
        assert_eq!(first.frames, 30);
        assert_eq!(plain.0, left);
        let (second, smoothed) = next(&mut stream, true);
        assert!(second.looped);
        assert_ne!(smoothed.0, left, "the second pass is smoothed");

        stream.rewind().unwrap();
        let (again, restarted) = next(&mut stream, true);
        assert!(!again.looped);
        assert_eq!(restarted.0, left, "no smoothing left over");
        assert_eq!(restarted.1, right);
        // And the pass after it is smoothed in the same way as before.
        let (_, smoothed_again) = next(&mut stream, true);
        assert_eq!(smoothed_again, smoothed);
    }

    #[test]
    fn a_rewind_in_the_middle_of_a_chunk_leaves_nothing_of_the_old_pass() {
        // What a stop does: the worker stops in the middle of a pass (the
        // resampler holds a part of a chunk and 2,048 frames are cut), the
        // ring buffer is emptied, and the song starts again.
        let dir = TempDir::new();
        for (from, to) in [(44_100_u32, 48_000_u32), (22_050, 48_000), (48_000, 48_000)] {
            let (left, right) = tone(from, 10_000);
            let path = wav(&dir, from, left, right);

            let mut fresh = SoundStream::open(&path, to).unwrap();
            let (wanted, _) = play_once(&mut fresh);

            let mut stream = SoundStream::open(&path, to).unwrap();
            let (chunk, _) = next(&mut stream, false);
            assert_eq!(chunk.frames, CHUNK);
            stream.rewind().unwrap();
            let (got, _) = play_once(&mut stream);
            assert_eq!(got, wanted, "{from} -> {to}: after a rewind in a pass");

            // The same in the middle of the second pass of a loop.
            let mut stream = SoundStream::open(&path, to).unwrap();
            let passes = play_loop(&mut stream, 2);
            assert_eq!(passes[0].0.len(), wanted.0.len());
            stream.rewind().unwrap();
            let (got, _) = play_once(&mut stream);
            assert_eq!(got, wanted, "{from} -> {to}: after a rewind in a loop");
        }
    }

    #[test]
    fn a_rewind_after_the_end_plays_the_song_again() {
        let dir = TempDir::new();
        let (left, right) = tone(48_000, 3_000);
        let path = wav(&dir, 48_000, left, right);
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        let (first, _) = play_once(&mut stream);
        stream.rewind().unwrap();
        let (second, _) = play_once(&mut stream);
        assert_eq!(first, second);
    }

    #[test]
    fn another_song_is_a_new_stream_that_shares_nothing() {
        let dir = TempDir::new();
        let (left, right) = tone(48_000, 3_000);
        let path = wav(&dir, 48_000, left.clone(), right);
        let mut a = SoundStream::open(&path, 48_000).unwrap();
        next(&mut a, true);
        drop(a);
        let mut b = SoundStream::open(&fixture("tone-44k-stereo-id3.mp3"), 48_000).unwrap();
        let ((out, _), _) = play_once(&mut b);
        assert_eq!(out.len(), frames_at(22_050, 44_100, 48_000));
    }

    // -- errors -------------------------------------------------------------

    /// A WAV file whose header says 1,000 frames and whose data was cut off.
    fn wav_without_frames(dir: &TempDir, rate: u32) -> PathBuf {
        let (left, right) = tone(rate, 1_000);
        let bytes = WavBuilder::new(WavFormat::F32, rate, vec![left, right]).build();
        dir.write("empty.wav", &bytes[..bytes.len() - 8_000])
    }

    /// Asks for chunks of a loop until one fails, on another thread. A check
    /// that is missing makes `next_chunk` turn for ever without returning, so
    /// a limit on the number of calls is not enough: this panics when there
    /// is no answer in `TIMEOUT` (the thread is left behind) or when
    /// `MAX_CALLS` calls give neither an error nor an end.
    fn call_until_it_fails(mut stream: SoundStream) -> MediaError {
        const MAX_CALLS: usize = 100;
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut left, mut right) = (Vec::new(), Vec::new());
            for _ in 0..MAX_CALLS {
                match stream.next_chunk(true, &mut left, &mut right) {
                    Err(e) => return sender.send(Some(e)).unwrap_or(()),
                    Ok(chunk) => assert!(!chunk.ended, "a loop does not end"),
                }
            }
            sender.send(None).unwrap_or(());
        });
        match receiver.recv_timeout(TIMEOUT) {
            Ok(Some(error)) => error,
            Ok(None) => panic!("{MAX_CALLS} calls and neither an error nor an end"),
            Err(_) => panic!("no answer in {TIMEOUT:?}: the loop turns for ever"),
        }
    }

    #[test]
    fn a_pass_with_no_frame_ends_the_song_or_fails_but_does_not_turn_for_ever() {
        let dir = TempDir::new();
        // 48,000 -> 48,000 has no resampler; 44,100 -> 48,000 has one, and
        // its empty pass (the resampler gives nothing after a reset) is
        // guarded in the same way.
        for rate in [48_000_u32, 44_100] {
            let path = wav_without_frames(&dir, rate);

            // Without a loop, it just ends.
            let mut stream = SoundStream::open(&path, 48_000).unwrap();
            let (chunk, (l, _)) = next(&mut stream, false);
            assert!(chunk.ended && chunk.frames == 0 && l.is_empty());

            // With a loop, it is an error (and the stream is dead afterwards: see
            // `a_dead_stream_stays_dead_until_it_is_rewound`).
            let stream = SoundStream::open(&path, 48_000).unwrap();
            let error = call_until_it_fails(stream);
            assert!(matches!(error, MediaError::Empty), "{rate}: {error}");
        }
    }

    #[test]
    fn a_dead_stream_stays_dead_until_it_is_rewound() {
        let dir = TempDir::new();
        let path = wav_without_frames(&dir, 48_000);
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        let (mut left, mut right) = (Vec::new(), Vec::new());
        assert!(stream.next_chunk(true, &mut left, &mut right).is_err());
        let again = stream.next_chunk(false, &mut left, &mut right).unwrap_err();
        assert!(matches!(again, MediaError::Decode(_)), "{again}");

        stream.rewind().unwrap();
        let chunk = stream.next_chunk(false, &mut left, &mut right).unwrap();
        assert!(chunk.ended);
    }

    #[test]
    fn packets_that_cannot_be_decoded_are_skipped_and_counted_once() {
        let mut bytes = std::fs::read(fixture("tone-44k-stereo-id3.mp3")).unwrap();
        bytes[1_367..1_371].fill(0xFF);
        let dir = TempDir::new();
        let path = dir.write("damaged.mp3", &bytes);
        let mut stream = SoundStream::open(&path, 48_000).unwrap();
        assert_eq!(stream.take_skipped_packets(), 0);
        let passes = play_loop(&mut stream, 2);
        assert_eq!(passes.len(), 2);
        let counted = stream.take_skipped_packets();
        assert!(counted >= 1, "{counted}");
        assert_eq!(stream.take_skipped_packets(), 0, "only the new ones");
    }

    #[test]
    fn a_stream_can_move_to_the_worker_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<SoundStream>();
        assert_send::<Chunk>();
    }
}
