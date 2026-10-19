//! Shows that the audio path does not allocate or free memory (AGENTS.md §5,
//! ADR-0009).
//!
//! This test binary replaces the global allocator with one that counts the
//! allocations, reallocations, and frees made by the thread that is inside
//! [`counted`]. Other threads (the test harness, other tests) are not
//! counted, because the counters and the switch are thread-local.
//!
//! The `unsafe` here is only the `GlobalAlloc` impl, which has to be
//! `unsafe`; it forwards every call to the system allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use ink_core::graph::{
    AudioBuffer, LinkWriter, MicGraph, MicrophoneSource, MixerControl, SeController, SePolicy,
    SoundData, audio_link, se_channel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Counts {
    allocs: usize,
    deallocs: usize,
    reallocs: usize,
}

thread_local! {
    // `const` initializers: no lazy setup, so the allocator can use them
    // without allocating.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNTS: Cell<Counts> = const { Cell::new(Counts { allocs: 0, deallocs: 0, reallocs: 0 }) };
}

struct CountingAllocator;

fn note(update: impl FnOnce(&mut Counts)) {
    // `try_with`: the thread-locals may be gone while a thread exits.
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = COUNTS.try_with(|counts| {
                let mut value = counts.get();
                update(&mut value);
                counts.set(value);
            });
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(|counts| counts.allocs += 1);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(|counts| counts.allocs += 1);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(|counts| counts.deallocs += 1);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(|counts| counts.reallocs += 1);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `f` and counts what this thread allocates and frees meanwhile. The
/// counts add up over calls until [`take`].
fn counted<R>(f: impl FnOnce() -> R) -> R {
    ARMED.set(true);
    let result = f();
    ARMED.set(false);
    result
}

/// Returns the counts so far and starts again from 0.
fn take() -> Counts {
    COUNTS.replace(Counts::default())
}

const RATE: u32 = 48_000;
/// The graph renders in parts of this many frames.
const MAX_FRAMES: usize = 256;
/// The largest block the tests ask for: more than `MAX_FRAMES`, so that the
/// graph splits it.
const BIGGEST_BLOCK: usize = 700;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn sound(rng: &mut Rng) -> Arc<SoundData> {
    let frames = [1, 300, 480, 481, 3000, 20_000, 20_000, 20_000][rng.below(8) as usize];
    Arc::new(SoundData::new(
        RATE,
        vec![0.01; frames],
        vec![-0.01; frames],
    ))
}

fn graph(with_se: bool) -> (MicGraph, LinkWriter, Arc<MixerControl>, SeController) {
    let (writer, reader, _) = audio_link(RATE, MAX_FRAMES);
    let control = Arc::new(MixerControl::new());
    let (se, player) = se_channel(RATE, MAX_FRAMES);
    let graph = MicGraph::new(
        MicrophoneSource::new(reader),
        Arc::clone(&control),
        RATE,
        MAX_FRAMES,
    );
    let graph = if with_se {
        graph.with_se(player)
    } else {
        graph
    };
    (graph, writer, control, se)
}

#[test]
fn the_counting_allocator_counts_this_thread() {
    take();
    counted(|| {
        let buffer = std::hint::black_box(vec![1_u8; 64]);
        drop(buffer);
    });
    let counts = take();
    assert_eq!((counts.allocs, counts.deallocs), (1, 1));

    // Outside `counted` nothing is counted.
    drop(std::hint::black_box(vec![1_u8; 64]));
    assert_eq!(take(), Counts::default());
}

#[test]
fn resizing_a_buffer_does_not_allocate() {
    let mut buffer = AudioBuffer::new(2, 512);
    take();
    counted(|| {
        for frames in [1, 7, 256, 512, 100, 512, 3] {
            buffer.set_frames(frames);
            for channel in 0..2 {
                buffer.channel_mut(channel).fill(0.5);
            }
            buffer.clear();
        }
    });
    assert_eq!(take(), Counts::default());
}

#[test]
fn the_graph_without_se_does_not_allocate() {
    let (mut graph, mut writer, control, _se) = graph(false);
    let mic = vec![0.1_f32; BIGGEST_BLOCK];
    let mut out = vec![0.0_f32; BIGGEST_BLOCK * 2];
    take();
    for frames in [64, 256, 700, 1, 300] {
        writer.push_channel(&mic[..frames], 1, 0);
        control.mic.set_gain_db(-3.0);
        counted(|| graph.render(&mut out[..frames * 2], 2));
    }
    assert_eq!(take(), Counts::default());
}

/// Random control actions and blocks. The sounds are replaced and cleared
/// by the control side, so the audio side holds the last reference of many
/// of them. Rendering allocates nothing and frees nothing.
#[test]
fn rendering_with_se_does_not_allocate_or_free() {
    for seed in [5_u64, 77, 31_337] {
        let (mut graph, mut writer, control, mut se) = graph(true);
        let mic = vec![0.1_f32; BIGGEST_BLOCK];
        let mut out = vec![0.0_f32; BIGGEST_BLOCK * 2];
        let mut rng = Rng(seed);
        take();

        for _ in 0..3000 {
            for _ in 0..rng.below(6) {
                let slot = rng.below(10) as usize;
                match rng.below(27) {
                    0..=11 => {
                        let policy = [SePolicy::Overlap, SePolicy::Restart, SePolicy::Ignore]
                            [rng.below(3) as usize];
                        let _ = se.play(slot, 1.0, policy);
                    }
                    12..=13 => {
                        let _ = se.stop(slot);
                    }
                    14 => {
                        let _ = se.stop_all();
                    }
                    15..=18 => {
                        // The control side lets go of the old sound; a
                        // voice may hold the last reference.
                        se.set_sound(slot, sound(&mut rng)).unwrap();
                    }
                    19 => {
                        se.clear(slot).unwrap();
                    }
                    20 => {
                        control.se.set_gain_db(rng.below(40) as f32 - 30.0);
                    }
                    21 => {
                        control.se.set_muted(rng.below(2) == 0);
                    }
                    22 => {
                        control.master.set_gain_db(rng.below(30) as f32 - 24.0);
                    }
                    23 => {
                        se.collect();
                    }
                    _ => {
                        // A burst: more plays than there are voices, and
                        // often a stop all after it, so the fading voices
                        // fill up too.
                        for _ in 0..20 {
                            let _ = se.play(rng.below(10) as usize, 1.0, SePolicy::Overlap);
                        }
                        if rng.below(2) == 0 {
                            let _ = se.stop_all();
                        }
                    }
                }
            }
            let frames = 1 + rng.below(BIGGEST_BLOCK as u64) as usize;
            writer.push_channel(&mic[..frames], 1, 0);
            counted(|| graph.render(&mut out[..frames * 2], 2));
        }

        let counts = take();
        assert_eq!(counts, Counts::default(), "seed {seed}");
        let stats = se.stats();
        assert!(stats.plays_started > 100, "seed {seed}: {stats:?}");
        assert!(stats.plays_dropped > 0, "seed {seed}: the voices filled up");
        assert!(stats.plays_ignored > 0, "seed {seed}");
        assert_eq!(stats.leaked, 0, "seed {seed}");
    }
}

/// The audio side holds the last reference of a sound. When the sound ends,
/// nothing is freed on the audio side; it is freed by `collect()` on the
/// control side.
#[test]
fn the_last_reference_is_dropped_on_the_control_side() {
    let (mut graph, _writer, _control, mut se) = graph(true);
    let mut out = vec![0.0_f32; 200];

    // Only the command (and later the voice) holds this sound.
    se.set_sound(
        0,
        Arc::new(SoundData::new(RATE, vec![0.25; 2000], vec![0.25; 2000])),
    )
    .unwrap();
    se.play(0, 1.0, SePolicy::Overlap).unwrap();
    se.clear(0).unwrap();
    assert!(se.sound(0).is_none());

    take();
    for _ in 0..30 {
        counted(|| graph.render(&mut out, 2));
    }
    assert_eq!(take(), Counts::default(), "rendering until the sound ends");
    assert_eq!(se.outstanding(), 1, "the sound waits in the return queue");

    let collected = counted(|| se.collect());
    let counts = take();
    assert_eq!(collected, 1);
    assert_eq!(counts.allocs, 0);
    // The `Arc` itself and the two channels of the sound.
    assert_eq!(counts.deallocs, 3);
    assert_eq!(se.outstanding(), 0);
}
