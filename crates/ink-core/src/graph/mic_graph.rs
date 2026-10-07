//! The Phase 1 graph: MIC -> Gain -> Mixer -> MASTER -> output (ADR-0007).

use std::sync::Arc;

use super::{
    AudioBuffer, AudioNode, AudioSource, GainNode, MicrophoneSource, MixerControl, MixerNode,
    block_peak,
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
pub struct MicGraph {
    source: MicrophoneSource,
    control: Arc<MixerControl>,
    gain: GainNode,
    mute: GainNode,
    mixer: MixerNode,
    master: GainNode,
    mic: AudioBuffer,
    mix: AudioBuffer,
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
        }
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

        self.mixer.process(&[&self.mic], &mut self.mix);

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
}
