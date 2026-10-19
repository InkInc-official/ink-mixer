//! The graph of the mixer: MIC (and the background music and the sound
//! effects) -> Gain -> Mixer -> MASTER -> output (ADR-0007, ADR-0009).

use std::sync::Arc;

use super::{
    AudioBuffer, AudioNode, AudioSource, BgmPlayer, ChannelControl, GainNode, MicrophoneSource,
    MixerControl, MixerNode, SePlayer, block_peak,
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
/// [`MicGraph::with_bgm`] and [`MicGraph::with_se`] add the background music
/// (BGM) channel and the sound effects (SE) channel next to the MIC. They
/// have the same path, and the mixer sums the MIC, the BGM, and the SE, in
/// this order (ARCHITECTURE.md §7):
///
/// ```text
/// BgmPlayer -> GainNode(bgm) -> BGM meter -> GainNode(mute) -> MixerNode
/// SePlayer  -> GainNode(se)  -> SE meter  -> GainNode(mute) -> MixerNode
/// ```
///
/// The sum goes on to the MASTER gain and the MASTER meter. Without these
/// channels the graph does exactly what it did in Phase 1: the same
/// operations in the same order, so the output does not change by a bit.
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
    bgm: Option<BgmPath>,
    se: Option<SePath>,
}

/// The volume, the meter, and the mute of a channel that a player fills
/// (the BGM and the SE work in the same way).
struct Strip {
    gain: GainNode,
    mute: GainNode,
    /// The stereo block of the player, which the gain nodes work on.
    buffer: AudioBuffer,
}

impl Strip {
    /// The gain and the mute start at the current values of `control`,
    /// without ramping.
    fn new(control: &ChannelControl, sample_rate: u32, max_frames: usize) -> Self {
        let mut gain = GainNode::new(control.gain());
        let mut mute = GainNode::new(control.mute_gain());
        for node in [&mut gain, &mut mute] {
            node.prepare(sample_rate);
        }
        Self {
            gain,
            mute,
            buffer: AudioBuffer::new(2, max_frames),
        }
    }

    /// Takes the block the player made, applies the volume, records the
    /// meter (after the volume and before the mute, like the MIC), and
    /// applies the mute. The result is in `buffer`.
    fn process(&mut self, rendered: &AudioBuffer, control: &ChannelControl) {
        self.buffer.set_frames(rendered.frames());
        for channel in 0..2 {
            self.buffer
                .channel_mut(channel)
                .copy_from_slice(rendered.channel(channel));
        }

        self.gain.set_target(control.gain());
        self.gain.process(&mut self.buffer);
        let peak = block_peak(self.buffer.channel(0)).max(block_peak(self.buffer.channel(1)));
        control.meter().record(peak);

        self.mute.set_target(control.mute_gain());
        self.mute.process(&mut self.buffer);
    }
}

/// The BGM channel of the graph.
struct BgmPath {
    player: BgmPlayer,
    strip: Strip,
}

/// The SE channel of the graph.
struct SePath {
    player: SePlayer,
    strip: Strip,
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
            bgm: None,
            se: None,
        }
    }

    /// Adds the background music channel, played by `player`.
    ///
    /// `player` must have been made with the same `max_frames` as this graph
    /// (see [`bgm_channel`](super::bgm_channel)), because the graph renders it
    /// in parts of at most `max_frames` frames. This allocates the buffer:
    /// call it outside the realtime path. The BGM volume and mute start at
    /// the current values of the control, without ramping.
    ///
    /// # Panics
    ///
    /// Panics if `player` has a different `max_frames`: better here than in
    /// the audio callback.
    pub fn with_bgm(mut self, player: BgmPlayer) -> Self {
        assert_eq!(
            player.max_frames(),
            self.mic.max_frames(),
            "the BGM player and the graph must have the same max_frames"
        );
        self.bgm = Some(BgmPath {
            player,
            strip: Strip::new(&self.control.bgm, self.sample_rate, self.mic.max_frames()),
        });
        self
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
        self.se = Some(SePath {
            player,
            strip: Strip::new(&self.control.se, self.sample_rate, self.mic.max_frames()),
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

        if let Some(bgm) = &mut self.bgm {
            let rendered = bgm.player.render(frames);
            bgm.strip.process(rendered, &control.bgm);
        }
        if let Some(se) = &mut self.se {
            let rendered = se.player.render(frames);
            se.strip.process(rendered, &control.se);
        }

        // The MIC, the BGM, and the SE, in the order of ARCHITECTURE.md §7.
        let mut inputs: [&AudioBuffer; 3] = [&self.mic; 3];
        let mut count = 1;
        if let Some(bgm) = &self.bgm {
            inputs[count] = &bgm.strip.buffer;
            count += 1;
        }
        if let Some(se) = &self.se {
            inputs[count] = &se.strip.buffer;
            count += 1;
        }
        self.mixer.process(&inputs[..count], &mut self.mix);

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

    // --- The background music (BGM) channel ---

    use crate::graph::{BgmController, BgmFeeder, FlushPoll, bgm_channel};

    /// A graph with the MIC, the BGM, and the SE, and the parts to drive
    /// them. The BGM ring buffer holds 1000 frames and needs 100 to start.
    struct Full {
        graph: MicGraph,
        writer: LinkWriter,
        control: Arc<MixerControl>,
        bgm: BgmController,
        feeder: BgmFeeder,
        se: SeController,
        /// The frames of the BGM written so far.
        written: usize,
    }

    fn full() -> Full {
        let (writer, reader, _) = audio_link(RATE, MAX_FRAMES);
        let control = Arc::new(MixerControl::new());
        let (bgm, feeder, bgm_player) = bgm_channel(RATE, MAX_FRAMES);
        let (se, se_player) = se_channel(RATE, MAX_FRAMES);
        let graph = MicGraph::new(
            MicrophoneSource::new(reader),
            Arc::clone(&control),
            RATE,
            MAX_FRAMES,
        )
        .with_bgm(bgm_player)
        .with_se(se_player);
        Full {
            graph,
            writer,
            control,
            bgm,
            feeder,
            se,
            written: 0,
        }
    }

    impl Full {
        /// Writes `count` frames of a constant to the BGM.
        fn feed_constant(&mut self, left: f32, right: f32, count: usize) {
            let (left, right) = (vec![left; count], vec![right; count]);
            assert_eq!(self.feeder.write(&left, &right), count);
            self.written += count;
        }

        /// Writes `count` frames of a counter to the BGM: frame `i` is
        /// `i + 1` on the left and `-(i + 1)` on the right.
        fn feed_counter(&mut self, count: usize) {
            let left: Vec<f32> = (0..count).map(|i| (self.written + i + 1) as f32).collect();
            let right: Vec<f32> = left.iter().map(|l| -l).collect();
            assert_eq!(self.feeder.write(&left, &right), count);
            self.written += count;
        }

        /// Feeds the MIC with `frames` frames of `value` and renders as many
        /// stereo frames, like one pair of callbacks.
        fn step(&mut self, mic: f32, frames: usize) -> Vec<f32> {
            push(&mut self.writer, mic, frames);
            let mut out = vec![-1.0; frames * 2];
            self.graph.render(&mut out, 2);
            out
        }
    }

    /// The left channel of an interleaved stereo block.
    fn lefts(out: &[f32]) -> Vec<f32> {
        out.chunks(2).map(|frame| frame[0]).collect()
    }

    #[test]
    fn bgm_reaches_left_and_right_unchanged_at_0_db() {
        let mut full = full();
        full.bgm.play();
        full.feed_constant(0.25, -0.5, 900);
        // The first block waits for nothing (900 frames are there); the fade
        // in takes 10 frames.
        full.step(0.0, 8);
        full.step(0.0, 8);
        let out = full.step(0.0, 8);
        for frame in out.chunks(2) {
            assert_eq!(frame, [0.25, -0.5]);
        }
    }

    #[test]
    fn mic_bgm_and_se_are_summed() {
        let mut full = full();
        prime(&mut full.writer, 0.5);
        full.bgm.play();
        full.feed_constant(0.25, 0.125, 900);
        let long = sound(1000, 0.0625, -0.0625);
        full.se.set_sound(0, Arc::clone(&long)).unwrap();
        full.se.play(0, 1.0, SePolicy::Overlap).unwrap();

        // The fade in of the BGM takes 10 frames.
        full.step(0.5, 8);
        full.step(0.5, 8);
        for _ in 0..5 {
            let out = full.step(0.5, 8);
            for frame in out.chunks(2) {
                // 0.5 + 0.25 + 0.0625 and 0.5 + 0.125 - 0.0625.
                assert_eq!(frame, [0.8125, 0.5625]);
            }
        }
    }

    #[test]
    fn the_bgm_alone_and_the_se_alone_are_what_each_is() {
        let mut full = full();
        full.bgm.play();
        full.feed_constant(0.5, 0.5, 900);
        for _ in 0..3 {
            full.step(0.0, 8);
        }
        assert!(full.step(0.0, 8).iter().all(|&s| s == 0.5));

        // The BGM is paused (a fade of 10 frames); then only the SE.
        full.bgm.pause();
        full.step(0.0, 8);
        full.step(0.0, 8);
        assert!(full.step(0.0, 8).iter().all(|&s| s == 0.0));
        full.se.set_sound(0, sound(1000, 0.125, 0.125)).unwrap();
        full.se.play(0, 1.0, SePolicy::Overlap).unwrap();
        assert!(full.step(0.0, 8).iter().all(|&s| s == 0.125));
    }

    #[test]
    fn bgm_volume_follows_the_control_with_a_ramp() {
        let mut full = full();
        full.control.bgm.set_gain_db(-6.0);
        full.bgm.play();
        full.feed_constant(0.5, 0.25, 900);
        // 3 blocks: the fade in and the ramp of the volume are over.
        for _ in 0..3 {
            full.step(0.0, 8);
        }
        let out = full.step(0.0, 8);
        let gain = db_to_gain(-6.0);
        for frame in out.chunks(2) {
            assert_close(frame[0], 0.5 * gain);
            assert_close(frame[1], 0.25 * gain);
        }
    }

    #[test]
    fn bgm_mute_silences_output_but_not_the_bgm_meter_and_leaves_the_others_alone() {
        let mut full = full();
        full.bgm.play();
        full.feed_constant(0.5, 0.25, 900);
        prime(&mut full.writer, 0.125);
        full.se.set_sound(0, sound(1000, 0.0625, 0.0625)).unwrap();
        full.se.play(0, 1.0, SePolicy::Overlap).unwrap();
        for _ in 0..3 {
            full.step(0.125, 8);
        }
        full.control.bgm.set_muted(true);
        for _ in 0..3 {
            full.step(0.125, 8);
        }
        full.control.bgm.meter().take();
        full.control.se.meter().take();
        full.control.mic.meter().take();

        let out = full.step(0.125, 8);
        // Only the MIC and the SE are heard.
        for frame in out.chunks(2) {
            assert_eq!(frame, [0.1875, 0.1875]);
        }
        // The meter of the BGM shows the louder channel after the volume and
        // before the mute; the other meters show their own channel.
        assert_close(full.control.bgm.meter().take(), 0.5);
        assert_close(full.control.se.meter().take(), 0.0625);
        assert_close(full.control.mic.meter().take(), 0.125);
        assert!(!full.control.se.is_muted());
        assert_eq!(full.control.se.gain_db(), 0.0);
    }

    #[test]
    fn the_bgm_meter_reads_after_the_volume() {
        let mut full = full();
        full.control.bgm.set_gain_db(-6.0);
        full.bgm.play();
        full.feed_constant(0.2, -0.8, 900);
        for _ in 0..3 {
            full.step(0.0, 8);
        }
        full.control.bgm.meter().take();
        full.step(0.0, 8);
        assert_close(full.control.bgm.meter().take(), 0.8 * db_to_gain(-6.0));
    }

    #[test]
    fn master_applies_to_the_sum_of_all_three() {
        let mut full = full();
        full.control.master.set_gain_db(-6.0);
        prime(&mut full.writer, 0.5);
        full.bgm.play();
        full.feed_constant(0.25, 0.25, 900);
        full.se.set_sound(0, sound(1000, 0.25, 0.25)).unwrap();
        full.se.play(0, 1.0, SePolicy::Overlap).unwrap();
        for _ in 0..4 {
            full.step(0.5, 8);
        }
        full.control.master.meter().take();
        let out = full.step(0.5, 8);
        let expected = 1.0 * db_to_gain(-6.0);
        for &sample in &out {
            assert_close(sample, expected);
        }
        assert_close(full.control.master.meter().take(), expected);
    }

    #[test]
    fn blocks_shorter_than_max_frames_give_the_same_sound() {
        // The same song and sound effect and MIC, in blocks of 8 frames and
        // in blocks of every length from 1 to 8.
        let mut full_blocks = full();
        let mut mixed_blocks = full();
        for rig in [&mut full_blocks, &mut mixed_blocks] {
            prime(&mut rig.writer, 0.5);
            rig.bgm.play();
            rig.feed_counter(900);
            rig.se.set_sound(0, sound(1000, 0.0625, 0.0625)).unwrap();
            rig.se.play(0, 1.0, SePolicy::Overlap).unwrap();
        }
        let mut a = Vec::new();
        let mut b = Vec::new();
        for _ in 0..16 {
            a.extend(full_blocks.step(0.5, 8));
        }
        // 1 + 2 + ... + 8 = 36 frames, 3 times and then 20 more: 128 frames.
        let mut done = 0;
        let mut size = 0;
        while done < 128 {
            size = size % 8 + 1;
            let size = size.min(128 - done);
            b.extend(mixed_blocks.step(0.5, size));
            done += size;
        }
        assert_eq!(a.len(), b.len());
        // The fade in (10 frames) is a ramp that is worked out from the
        // start of each block, so its values can differ in the last bit; from
        // then on the two are the same bit for bit.
        for (frame, (x, y)) in a.chunks(2).zip(b.chunks(2)).enumerate() {
            if frame < 12 {
                assert_close(x[0], y[0]);
                assert_close(x[1], y[1]);
            } else {
                assert_eq!(x, y, "frame {frame}");
            }
        }
        assert_eq!(
            full_blocks.bgm.stats().played_frames,
            mixed_blocks.bgm.stats().played_frames
        );
        // And the song itself is whole: after the fade in, the counter goes
        // up by one for each frame.
        let left = lefts(&a);
        for pair in left[10..].windows(2) {
            assert_eq!(pair[1] - pair[0], 1.0);
        }
    }

    #[test]
    fn a_long_block_renders_the_bgm_and_the_se_in_parts_of_max_frames() {
        let mut full = full();
        prime(&mut full.writer, 0.5);
        full.bgm.play();
        full.feed_counter(900);
        full.se.set_sound(0, sound(1000, 0.0625, 0.0625)).unwrap();
        full.se.play(0, 1.0, SePolicy::Overlap).unwrap();
        // 20 frames with MAX_FRAMES = 8: parts of 8, 8, and 4.
        let out = full.step(0.5, 20);
        let left = lefts(&out);
        assert_eq!(left.len(), 20);
        for pair in left[10..].windows(2) {
            assert_eq!(pair[1] - pair[0], 1.0);
        }
        // The SE is in every part.
        assert_eq!(left[19], 0.5 + 20.0 + 0.0625);
        assert_eq!(full.bgm.stats().played_frames, 20);
    }

    #[test]
    fn pause_and_stop_of_the_bgm_work_through_the_graph() {
        let mut full = full();
        full.bgm.play();
        full.feed_counter(500);
        for _ in 0..4 {
            full.step(0.0, 8);
        }
        assert_eq!(full.bgm.stats().played_frames, 32);

        // Pause: a fade of 10 frames, then silence.
        full.bgm.pause();
        let fading = lefts(&full.step(0.0, 8));
        assert!(fading.iter().all(|&s| s != 0.0));
        full.step(0.0, 8);
        assert!(full.step(0.0, 8).iter().all(|&s| s == 0.0));
        assert_eq!(full.bgm.stats().played_frames, 42);

        // Stop: the worker answers, and the old sound is thrown away.
        full.bgm.request_flush();
        let mut done = false;
        for _ in 0..10 {
            done |= full.feeder.poll_flush() == FlushPoll::Done;
            full.step(0.0, 8);
        }
        assert!(full.bgm.is_flushed());
        assert!(done, "the worker was told that the sound is thrown away");
        assert_eq!(full.bgm.stats().played_frames, 0);
        assert_eq!(full.feeder.free_frames(), 1000);
    }

    #[test]
    fn the_order_of_with_se_and_with_bgm_does_not_matter() {
        fn build(
            bgm_first: bool,
        ) -> (MicGraph, LinkWriter, BgmController, BgmFeeder, SeController) {
            let (writer, reader, _) = audio_link(RATE, MAX_FRAMES);
            let control = Arc::new(MixerControl::new());
            let (bgm, feeder, bgm_player) = bgm_channel(RATE, MAX_FRAMES);
            let (se, se_player) = se_channel(RATE, MAX_FRAMES);
            let graph = MicGraph::new(MicrophoneSource::new(reader), control, RATE, MAX_FRAMES);
            let graph = if bgm_first {
                graph.with_bgm(bgm_player).with_se(se_player)
            } else {
                graph.with_se(se_player).with_bgm(bgm_player)
            };
            (graph, writer, bgm, feeder, se)
        }
        let mut outputs = Vec::new();
        for bgm_first in [true, false] {
            let (mut graph, mut writer, bgm, mut feeder, mut se) = build(bgm_first);
            prime(&mut writer, 0.5);
            bgm.play();
            let counter: Vec<f32> = (1..=900).map(|i| i as f32).collect();
            let negative: Vec<f32> = counter.iter().map(|c| -c).collect();
            feeder.write(&counter, &negative);
            se.set_sound(0, sound(1000, 0.0625, 0.0625)).unwrap();
            se.play(0, 1.0, SePolicy::Overlap).unwrap();
            let mut out = Vec::new();
            for _ in 0..6 {
                push(&mut writer, 0.5, 8);
                let mut block = vec![0.0; 16];
                graph.render(&mut block, 2);
                out.extend(block);
            }
            outputs.push(out);
        }
        assert_eq!(outputs[0], outputs[1]);
    }

    #[test]
    #[should_panic(expected = "same max_frames")]
    fn with_bgm_refuses_a_player_with_another_max_frames() {
        let (_, reader, _) = audio_link(RATE, MAX_FRAMES);
        let (_bgm, _feeder, player) = bgm_channel(RATE, MAX_FRAMES * 2);
        let _ = MicGraph::new(
            MicrophoneSource::new(reader),
            Arc::new(MixerControl::new()),
            RATE,
            MAX_FRAMES,
        )
        .with_bgm(player);
    }

    #[test]
    fn idle_bgm_and_se_leave_the_mic_as_it_is() {
        let (mut plain, mut plain_writer, plain_control) = graph();
        let mut full = full();
        for control in [&plain_control, &full.control] {
            control.mic.set_gain_db(-4.0);
            control.master.set_gain_db(-9.0);
        }
        prime(&mut plain_writer, 0.5);
        prime(&mut full.writer, 0.5);
        for block in 0..6 {
            if block == 3 {
                plain_control.mic.set_muted(true);
                full.control.mic.set_muted(true);
            }
            let expected = step(&mut plain, &mut plain_writer, 0.5, 8, 2);
            let actual = full.step(0.5, 8);
            assert_eq!(actual, expected, "block {block}");
        }
        assert_eq!(full.control.bgm.meter().take(), 0.0);
        assert_eq!(full.control.se.meter().take(), 0.0);
    }

    /// The Phase 1 chain written out with the nodes, as a reference.
    struct Chain {
        source: MicrophoneSource,
        control: Arc<MixerControl>,
        gain: GainNode,
        mute: GainNode,
        mixer: MixerNode,
        master: GainNode,
        mic: AudioBuffer,
        mix: AudioBuffer,
    }

    impl Chain {
        fn new(source: MicrophoneSource, control: Arc<MixerControl>) -> Self {
            let mut source = source;
            source.prepare(RATE);
            let mut gain = GainNode::new(control.mic.gain());
            let mut mute = GainNode::new(control.mic.mute_gain());
            let mut master = GainNode::new(control.master.gain());
            for node in [&mut gain, &mut mute, &mut master] {
                node.prepare(RATE);
            }
            Self {
                source,
                control,
                gain,
                mute,
                mixer: MixerNode::new(),
                master,
                mic: AudioBuffer::new(1, MAX_FRAMES),
                mix: AudioBuffer::new(2, MAX_FRAMES),
            }
        }

        fn render(&mut self, out: &mut [f32], channels: usize) {
            for chunk in out.chunks_mut(MAX_FRAMES * channels) {
                let frames = chunk.len() / channels;
                self.mic.set_frames(frames);
                self.mix.set_frames(frames);
                self.source.render(&mut self.mic);
                self.gain.set_target(self.control.mic.gain());
                self.gain.process(&mut self.mic);
                self.control
                    .mic
                    .meter()
                    .record(block_peak(self.mic.channel(0)));
                self.mute.set_target(self.control.mic.mute_gain());
                self.mute.process(&mut self.mic);
                self.mixer.process(&[&self.mic], &mut self.mix);
                self.master.set_target(self.control.master.gain());
                self.master.process(&mut self.mix);
                let peak = block_peak(self.mix.channel(0)).max(block_peak(self.mix.channel(1)));
                self.control.master.meter().record(peak);
                write_interleaved(&self.mix, chunk, channels);
            }
        }
    }

    #[test]
    fn the_phase_1_graph_is_the_chain_of_its_nodes_bit_for_bit() {
        for channels in [1, 2, 4] {
            for frames in [8, 20, 3] {
                let (graph_writer, graph_reader, _) = audio_link(RATE, MAX_FRAMES);
                let (chain_writer, chain_reader, _) = audio_link(RATE, MAX_FRAMES);
                let graph_control = Arc::new(MixerControl::new());
                let chain_control = Arc::new(MixerControl::new());
                let mut graph = MicGraph::new(
                    MicrophoneSource::new(graph_reader),
                    Arc::clone(&graph_control),
                    RATE,
                    MAX_FRAMES,
                );
                let mut chain = Chain::new(
                    MicrophoneSource::new(chain_reader),
                    Arc::clone(&chain_control),
                );
                let (mut graph_writer, mut chain_writer) = (graph_writer, chain_writer);
                prime(&mut graph_writer, 0.3);
                prime(&mut chain_writer, 0.3);

                for block in 0..40 {
                    // A changing MIC, and changes of the controls.
                    let value = ((block * 7) % 11) as f32 / 10.0 - 0.5;
                    if block == 5 {
                        for control in [&graph_control, &chain_control] {
                            control.mic.set_gain_db(-7.3);
                            control.master.set_gain_db(-2.1);
                        }
                    }
                    if block == 15 {
                        for control in [&graph_control, &chain_control] {
                            control.mic.set_muted(true);
                        }
                    }
                    if block == 25 {
                        for control in [&graph_control, &chain_control] {
                            control.mic.set_muted(false);
                            control.master.set_gain_db(-60.0);
                        }
                    }
                    push(&mut graph_writer, value, frames);
                    push(&mut chain_writer, value, frames);
                    let mut from_graph = vec![-1.0; frames * channels];
                    let mut from_chain = vec![-1.0; frames * channels];
                    graph.render(&mut from_graph, channels);
                    chain.render(&mut from_chain, channels);
                    let bits = |out: &[f32]| out.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
                    assert_eq!(
                        bits(&from_graph),
                        bits(&from_chain),
                        "{channels} channels, {frames} frames, block {block}"
                    );
                }
                assert_eq!(
                    graph_control.master.meter().take().to_bits(),
                    chain_control.master.meter().take().to_bits()
                );
            }
        }
    }
}
