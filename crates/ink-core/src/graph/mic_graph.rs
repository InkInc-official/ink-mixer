//! The graph of the mixer: MIC (and the sound effects) -> Gain -> Mixer ->
//! MASTER -> output (ADR-0007, ADR-0009).

use std::sync::Arc;

use super::{
    AudioBuffer, AudioNode, AudioSource, GainNode, MicrophoneSource, MixerControl, MixerNode,
    SePlayer, block_peak,
};

/// The fixed Phase 1 path, run by the output callback:
///
/// ```text
/// MicrophoneSource -> GainNode(gain) -> MIC meter -> GainNode(mute)
///   -> MixerNode (mono to L/R) -> GainNode(master) -> MASTER meter -> output
/// ```
///
/// The MIC meter reads after the gain and before the mute, so it still shows
/// the MIC level while muted. Gains and the mute come from a shared
/// [`MixerControl`] every block.
///
/// [`MicGraph::with_se`] adds the sound effects (SE) channel next to the MIC.
/// Its path is the same, and the mixer sums the two:
///
/// ```text
/// SePlayer -> GainNode(se) -> SE meter -> GainNode(mute) -> MixerNode
/// ```
///
/// Without it the graph does exactly what it did in Phase 1.
pub struct MicGraph {
    source: MicrophoneSource,
    control: Arc<MixerControl>,
    gain: GainNode,
    mute: GainNode,
    mixer: MixerNode,
    master: GainNode,
    mic: AudioBuffer,
    mix: AudioBuffer,
    sample_rate: u32,
    se: Option<SePath>,
}

/// The SE channel of the graph.
struct SePath {
    player: SePlayer,
    gain: GainNode,
    mute: GainNode,
    /// The stereo mix of the player, which the gain nodes work on.
    buffer: AudioBuffer,
}

impl MicGraph {
    /// Builds the graph for `sample_rate`, processing at most `max_frames`
    /// frames at a time.
    ///
    /// This allocates the buffers: call it outside the realtime path. The
    /// gains start at the current values of `control`, without ramping.
    pub fn new(
        mut source: MicrophoneSource,
        control: Arc<MixerControl>,
        sample_rate: u32,
        max_frames: usize,
    ) -> Self {
        source.prepare(sample_rate);
        let mut gain = GainNode::new(control.mic.gain());
        let mut mute = GainNode::new(control.mic.mute_gain());
        let mut master = GainNode::new(control.master.gain());
        for node in [&mut gain, &mut mute, &mut master] {
            node.prepare(sample_rate);
        }
        Self {
            source,
            control,
            gain,
            mute,
            mixer: MixerNode::new(),
            master,
            mic: AudioBuffer::new(1, max_frames),
            mix: AudioBuffer::new(2, max_frames),
            sample_rate,
            se: None,
        }
    }

    /// Adds the sound effects channel, played by `player`.
    ///
    /// `player` must have been made with the same `max_frames` as this graph
    /// (see [`se_channel`](super::se_channel)), because the graph renders it
    /// in parts of at most `max_frames` frames. This allocates the buffer:
    /// call it outside the realtime path. The SE gain and mute start at the
    /// current values of the control, without ramping.
    ///
    /// # Panics
    ///
    /// Panics if `player` has a different `max_frames`: better here than in
    /// the audio callback.
    pub fn with_se(mut self, player: SePlayer) -> Self {
        assert_eq!(
            player.max_frames(),
            self.mic.max_frames(),
            "the SE player and the graph must have the same max_frames"
        );
        let mut gain = GainNode::new(self.control.se.gain());
        let mut mute = GainNode::new(self.control.se.mute_gain());
        for node in [&mut gain, &mut mute] {
            node.prepare(self.sample_rate);
        }
        self.se = Some(SePath {
            player,
            gain,
            mute,
            buffer: AudioBuffer::new(2, self.mic.max_frames()),
        });
        self
    }

    /// Fills the interleaved device buffer `out` with `channels` channels.
    /// A buffer longer than `max_frames` is processed in parts.
    ///
    /// Safe to call on the realtime path: it does not allocate or block.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        let max_frames = self.mic.max_frames();
        if channels == 0 || max_frames == 0 {
            out.fill(0.0);
            return;
        }
        for chunk in out.chunks_mut(max_frames * channels) {
            self.render_chunk(chunk, channels);
        }
    }

    fn render_chunk(&mut self, out: &mut [f32], channels: usize) {
        let frames = out.len() / channels;
        let control = &self.control;
        self.mic.set_frames(frames);
        self.mix.set_frames(frames);

        self.source.render(&mut self.mic);

        self.gain.set_target(control.mic.gain());
        self.gain.process(&mut self.mic);
        control.mic.meter().record(block_peak(self.mic.channel(0)));

        self.mute.set_target(control.mic.mute_gain());
        self.mute.process(&mut self.mic);

        match &mut self.se {
            None => self.mixer.process(&[&self.mic], &mut self.mix),
            Some(se) => {
                let rendered = se.player.render(frames);
                se.buffer.set_frames(frames);
                for channel in 0..2 {
                    se.buffer
                        .channel_mut(channel)
                        .copy_from_slice(rendered.channel(channel));
                }

                se.gain.set_target(control.se.gain());
                se.gain.process(&mut se.buffer);
                let peak = block_peak(se.buffer.channel(0)).max(block_peak(se.buffer.channel(1)));
                control.se.meter().record(peak);

                se.mute.set_target(control.se.mute_gain());
                se.mute.process(&mut se.buffer);

                self.mixer.process(&[&self.mic, &se.buffer], &mut self.mix);
            }
        }

        self.master.set_target(control.master.gain());
        self.master.process(&mut self.mix);
        let peak = block_peak(self.mix.channel(0)).max(block_peak(self.mix.channel(1)));
        control.master.meter().record(peak);

        write_interleaved(&self.mix, out, channels);
    }
}

/// Writes the stereo `mix` to the interleaved device buffer `out` with
/// `channels` channels: L and R to the first two channels and silence to the
/// rest, or the average of L and R to a single channel.
///
/// Writes `mix.frames()` frames, or fewer if `out` is shorter.
pub fn write_interleaved(mix: &AudioBuffer, out: &mut [f32], channels: usize) {
    if channels == 0 || mix.channels() < 2 {
        out.fill(0.0);
        return;
    }
    let (left, right) = (mix.channel(0), mix.channel(1));
    for (i, frame) in out.chunks_mut(channels).take(mix.frames()).enumerate() {
        if channels == 1 {
            frame[0] = (left[i] + right[i]) * 0.5;
        } else {
            frame[0] = left[i];
            frame[1] = right[i];
            frame[2..].fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{LinkWriter, audio_link, db_to_gain};

    /// 1000 Hz: the link primes at 20 frames and gains ramp over 10 frames.
    const RATE: u32 = 1000;
    const MAX_FRAMES: usize = 8;

    fn graph() -> (MicGraph, LinkWriter, Arc<MixerControl>) {
        let (writer, reader, _) = audio_link(RATE, MAX_FRAMES);
        let control = Arc::new(MixerControl::new());
        let graph = MicGraph::new(
            MicrophoneSource::new(reader),
            Arc::clone(&control),
            RATE,
            MAX_FRAMES,
        );
        (graph, writer, control)
    }

    fn push(writer: &mut LinkWriter, value: f32, frames: usize) {
        for _ in 0..frames {
            writer.push_channel(&[value], 1, 0);
        }
    }

    /// Buffers the link's target amount (20 frames) of `value`.
    fn prime(writer: &mut LinkWriter, value: f32) {
        push(writer, value, 20);
    }

    /// Feeds `frames` of `value` and renders as many, like one pair of input
    /// and output callbacks.
    fn step(
        graph: &mut MicGraph,
        writer: &mut LinkWriter,
        value: f32,
        frames: usize,
        channels: usize,
    ) -> Vec<f32> {
        push(writer, value, frames);
        let mut out = vec![-1.0; frames * channels];
        graph.render(&mut out, channels);
        out
    }

    /// Runs `blocks` steps of 8 stereo frames and returns the last output.
    fn run(graph: &mut MicGraph, writer: &mut LinkWriter, value: f32, blocks: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..blocks {
            out = step(graph, writer, value, 8, 2);
        }
        out
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "{actual} is not {expected}"
        );
    }

    #[test]
    fn mic_reaches_both_channels_unchanged_at_0_db() {
        let (mut graph, mut writer, _) = graph();
        prime(&mut writer, 0.5);
        let out = step(&mut graph, &mut writer, 0.5, 8, 2);
        assert!(out.iter().all(|&s| s == 0.5), "{out:?}");
    }

    #[test]
    fn gain_and_master_follow_the_control() {
        let (mut graph, mut writer, control) = graph();
        control.mic.set_gain_db(-6.0);
        control.master.set_gain_db(-6.0);
        prime(&mut writer, 0.5);

        let first = step(&mut graph, &mut writer, 0.5, 8, 2);
        let expected = 0.5 * db_to_gain(-6.0) * db_to_gain(-6.0);
        assert!(first[0] > expected, "ramping at first");

        // The 10-frame ramp is over after the first block.
        let out = run(&mut graph, &mut writer, 0.5, 3);
        for &sample in &out {
            assert_close(sample, expected);
        }
    }

    #[test]
    fn mute_silences_output_but_not_the_mic_meter() {
        let (mut graph, mut writer, control) = graph();
        prime(&mut writer, 0.5);
        run(&mut graph, &mut writer, 0.5, 2);
        control.mic.set_muted(true);

        run(&mut graph, &mut writer, 0.5, 3); // the mute ramp ends
        control.mic.meter().take();
        control.master.meter().take();

        let out = run(&mut graph, &mut writer, 0.5, 1);
        assert_eq!(out, vec![0.0; 16]);
        assert_close(control.mic.meter().take(), 0.5);
        assert_eq!(control.master.meter().take(), 0.0);
    }

    #[test]
    fn master_meter_reads_the_final_output() {
        let (mut graph, mut writer, control) = graph();
        control.master.set_gain_db(-6.0);
        prime(&mut writer, 0.5);
        run(&mut graph, &mut writer, 0.5, 2); // the ramp ends
        control.master.meter().take();
        control.mic.meter().take();

        run(&mut graph, &mut writer, 0.5, 1);
        assert_close(control.master.meter().take(), 0.5 * db_to_gain(-6.0));
        assert_close(control.mic.meter().take(), 0.5);
    }

    #[test]
    fn output_channel_layouts() {
        let (mut graph, mut writer, _) = graph();
        prime(&mut writer, 0.5);
        assert_eq!(step(&mut graph, &mut writer, 0.5, 2, 1), vec![0.5, 0.5]);
        assert_eq!(
            step(&mut graph, &mut writer, 0.5, 2, 4),
            vec![0.5, 0.5, 0.0, 0.0, 0.5, 0.5, 0.0, 0.0]
        );
    }

    #[test]
    fn write_interleaved_maps_left_and_right() {
        let mut mix = AudioBuffer::new(2, 2);
        mix.channel_mut(0).copy_from_slice(&[0.2, 0.4]);
        mix.channel_mut(1).copy_from_slice(&[0.6, 0.8]);

        let mut mono = [9.0; 2];
        write_interleaved(&mix, &mut mono, 1);
        assert_close(mono[0], 0.4);
        assert_close(mono[1], 0.6);

        let mut quad = [9.0; 8];
        write_interleaved(&mix, &mut quad, 4);
        assert_eq!(quad, [0.2, 0.6, 0.0, 0.0, 0.4, 0.8, 0.0, 0.0]);
    }

    #[test]
    fn no_input_gives_silence() {
        let (mut graph, _, _) = graph();
        let mut out = vec![-1.0; 16];
        graph.render(&mut out, 2);
        assert_eq!(out, vec![0.0; 16]);
    }

    #[test]
    fn long_blocks_are_rendered_in_parts() {
        let (mut graph, mut writer, _) = graph();
        prime(&mut writer, 0.5);
        // 20 frames with MAX_FRAMES = 8: parts of 8, 8, and 4.
        let out = step(&mut graph, &mut writer, 0.5, 20, 2);
        assert!(out.iter().all(|&s| s == 0.5), "{out:?}");
    }

    // --- The sound effects (SE) channel ---

    use crate::graph::{SE_PLAYING_VOICES, SeController, SePolicy, SoundData, se_channel};

    /// A sound of `frames` frames with `left` and `right` all the way.
    fn sound(frames: usize, left: f32, right: f32) -> Arc<SoundData> {
        Arc::new(SoundData::new(
            RATE,
            vec![left; frames],
            vec![right; frames],
        ))
    }

    fn graph_with_se() -> (MicGraph, LinkWriter, Arc<MixerControl>, SeController) {
        let (writer, reader, _) = audio_link(RATE, MAX_FRAMES);
        let control = Arc::new(MixerControl::new());
        let (se, player) = se_channel(RATE, MAX_FRAMES);
        let graph = MicGraph::new(
            MicrophoneSource::new(reader),
            Arc::clone(&control),
            RATE,
            MAX_FRAMES,
        )
        .with_se(player);
        (graph, writer, control, se)
    }

    /// Renders `frames` stereo frames without feeding the MIC.
    fn render(graph: &mut MicGraph, frames: usize) -> Vec<f32> {
        let mut out = vec![-1.0; frames * 2];
        graph.render(&mut out, 2);
        out
    }

    fn play(se: &mut SeController, slot: usize, sound: &Arc<SoundData>) {
        se.set_sound(slot, Arc::clone(sound)).unwrap();
        se.play(slot, 1.0, SePolicy::Overlap).unwrap();
    }

    #[test]
    fn se_reaches_left_and_right_unchanged_at_0_db() {
        let (mut graph, _, _, mut se) = graph_with_se();
        play(&mut se, 0, &sound(100, 0.25, -0.5));
        let out = render(&mut graph, 8);
        for frame in out.chunks(2) {
            assert_eq!(frame, [0.25, -0.5]);
        }
    }

    #[test]
    fn mic_and_se_are_summed() {
        let (mut graph, mut writer, _, mut se) = graph_with_se();
        prime(&mut writer, 0.5);
        play(&mut se, 0, &sound(100, 0.25, 0.125));
        let out = step(&mut graph, &mut writer, 0.5, 8, 2);
        for frame in out.chunks(2) {
            assert_eq!(frame, [0.75, 0.625]);
        }
    }

    #[test]
    fn without_a_sound_the_output_is_the_same_as_without_se() {
        let (mut plain, mut plain_writer, plain_control) = graph();
        let (mut with_se, mut se_writer, se_control, _se) = graph_with_se();
        for control in [&plain_control, &se_control] {
            control.mic.set_gain_db(-4.0);
            control.master.set_gain_db(-9.0);
        }
        prime(&mut plain_writer, 0.5);
        prime(&mut se_writer, 0.5);

        for block in 0..6 {
            if block == 3 {
                plain_control.mic.set_muted(true);
                se_control.mic.set_muted(true);
            }
            let expected = step(&mut plain, &mut plain_writer, 0.5, 8, 2);
            let actual = step(&mut with_se, &mut se_writer, 0.5, 8, 2);
            assert_eq!(actual, expected, "block {block}");
        }
        assert_eq!(se_control.se.meter().take(), 0.0);
        assert_eq!(
            se_control.master.meter().take(),
            plain_control.master.meter().take()
        );
    }

    #[test]
    fn se_volume_follows_the_control_with_a_ramp() {
        let (mut graph, _, control, mut se) = graph_with_se();
        control.se.set_gain_db(-6.0);
        play(&mut se, 0, &sound(1000, 0.5, 0.5));

        let first = render(&mut graph, 8);
        let expected = 0.5 * db_to_gain(-6.0);
        assert!(first[0] > expected, "ramping at first");

        // The 10-frame ramp is over after the second block.
        render(&mut graph, 8);
        for &sample in &render(&mut graph, 8) {
            assert_close(sample, expected);
        }
    }

    #[test]
    fn master_applies_to_se_too() {
        let (mut graph, _, control, mut se) = graph_with_se();
        control.master.set_gain_db(-6.0);
        play(&mut se, 0, &sound(1000, 0.5, 0.5));
        render(&mut graph, 8);
        render(&mut graph, 8);
        control.master.meter().take();

        let out = render(&mut graph, 8);
        let expected = 0.5 * db_to_gain(-6.0);
        for &sample in &out {
            assert_close(sample, expected);
        }
        assert_close(control.master.meter().take(), expected);
    }

    #[test]
    fn se_mute_silences_output_but_not_the_se_meter() {
        let (mut graph, _, control, mut se) = graph_with_se();
        play(&mut se, 0, &sound(1000, 0.5, 0.25));
        render(&mut graph, 8);
        control.se.set_muted(true);
        render(&mut graph, 8);
        render(&mut graph, 8); // the mute ramp ends
        control.se.meter().take();
        control.master.meter().take();

        let out = render(&mut graph, 8);
        assert_eq!(out, vec![0.0; 16]);
        assert_close(control.se.meter().take(), 0.5);
        assert_eq!(control.master.meter().take(), 0.0);
    }

    #[test]
    fn the_se_meter_reads_the_louder_channel_after_the_gain() {
        let (mut graph, _, control, mut se) = graph_with_se();
        control.se.set_gain_db(-6.0);
        play(&mut se, 0, &sound(1000, 0.2, -0.8));
        render(&mut graph, 8);
        render(&mut graph, 8);
        control.se.meter().take();

        render(&mut graph, 8);
        assert_close(control.se.meter().take(), 0.8 * db_to_gain(-6.0));
        assert_eq!(control.mic.meter().take(), 0.0);
    }

    #[test]
    fn mic_and_se_mute_are_independent() {
        let (mut graph, mut writer, control, mut se) = graph_with_se();
        prime(&mut writer, 0.5);
        play(&mut se, 0, &sound(1000, 0.25, 0.25));
        run_mixed(&mut graph, &mut writer, 2);

        control.mic.set_muted(true);
        let out = run_mixed(&mut graph, &mut writer, 3);
        assert!(out.iter().all(|&s| (s - 0.25).abs() < 1e-6), "{out:?}");

        control.mic.set_muted(false);
        control.se.set_muted(true);
        let out = run_mixed(&mut graph, &mut writer, 3);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6), "{out:?}");
    }

    /// Runs `blocks` steps of 8 stereo frames with the MIC at 0.5.
    fn run_mixed(graph: &mut MicGraph, writer: &mut LinkWriter, blocks: usize) -> Vec<f32> {
        run(graph, writer, 0.5, blocks)
    }

    /// The device asks for more frames than `max_frames`: the graph renders
    /// them in parts, and the SE player (which panics above its own
    /// `max_frames`) is called in parts too.
    #[test]
    fn long_blocks_render_the_se_in_parts_of_max_frames() {
        let (mut graph, _, _, mut se) = graph_with_se();
        // 20 frames with MAX_FRAMES = 8: parts of 8, 8, and 4.
        play(&mut se, 0, &sound(100, 0.25, -0.25));
        let out = render(&mut graph, 20);
        assert_eq!(out.len(), 40);
        for frame in out.chunks(2) {
            assert_eq!(frame, [0.25, -0.25]);
        }
    }

    #[test]
    fn a_sound_that_ends_inside_a_part_is_cut_at_the_right_frame() {
        let (mut graph, _, _, mut se) = graph_with_se();
        // 13 frames end in the middle of the second part.
        play(&mut se, 0, &sound(13, 0.5, 0.5));
        let out = render(&mut graph, 20);
        for (i, frame) in out.chunks(2).enumerate() {
            let expected = if i < 13 { 0.5 } else { 0.0 };
            assert_eq!(frame, [expected, expected], "frame {i}");
        }
        se.collect();
        assert_eq!(se.outstanding(), 0);
    }

    #[test]
    fn a_sound_goes_on_across_blocks_and_parts() {
        let (mut graph, _, _, mut se) = graph_with_se();
        let source = SoundData::new(
            RATE,
            (0..30).map(|i| i as f32 / 100.0).collect(),
            vec![0.0; 30],
        );
        se.set_sound(0, Arc::new(source)).unwrap();
        se.play(0, 1.0, SePolicy::Overlap).unwrap();

        let mut left = Vec::new();
        for frames in [5, 20, 5] {
            left.extend(render(&mut graph, frames).chunks(2).map(|frame| frame[0]));
        }
        for (i, &sample) in left.iter().enumerate() {
            assert_close(sample, i as f32 / 100.0);
        }
    }

    #[test]
    fn many_sounds_at_once_are_summed() {
        let (mut graph, _, _, mut se) = graph_with_se();
        for slot in 0..SE_PLAYING_VOICES {
            play(&mut se, slot, &sound(100, 0.01, 0.01));
        }
        let out = render(&mut graph, 8);
        for &sample in &out {
            assert_close(sample, 0.16);
        }
    }

    #[test]
    #[should_panic(expected = "same max_frames")]
    fn with_se_refuses_a_player_with_another_max_frames() {
        let (_, reader, _) = audio_link(RATE, MAX_FRAMES);
        let (_se, player) = se_channel(RATE, MAX_FRAMES * 2);
        let _ = MicGraph::new(
            MicrophoneSource::new(reader),
            Arc::new(MixerControl::new()),
            RATE,
            MAX_FRAMES,
        )
        .with_se(player);
    }

    #[test]
    fn se_output_follows_the_channel_layouts() {
        let (mut graph, _, _, mut se) = graph_with_se();
        play(&mut se, 0, &sound(100, 0.25, 0.75));
        let mut mono = vec![-1.0; 2];
        graph.render(&mut mono, 1);
        assert_eq!(mono, vec![0.5, 0.5]);

        let mut quad = vec![-1.0; 8];
        graph.render(&mut quad, 4);
        assert_eq!(quad, vec![0.25, 0.75, 0.0, 0.0, 0.25, 0.75, 0.0, 0.0]);
    }
}
