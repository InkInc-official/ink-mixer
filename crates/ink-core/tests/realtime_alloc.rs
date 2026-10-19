//! Shows that the audio path does not allocate or free memory (AGENTS.md §5,
//! ADR-0009).
//!
//! This test binary replaces the global allocator with one that counts the
//! allocations, reallocations, and frees made by the thread that is inside
//! [`counted`]. Other threads (the test harness, other tests) are not
//! counted, because the counters and the switch are thread-local.
//!
//! A thread can also name one size with [`watch`]: the allocations and frees
//! of exactly that many bytes are counted on their own. The tests of the
//! order in which the BGM parts are dropped use it with the size of the ring
//! buffer, to tell which thread frees it.
//!
//! The `unsafe` here is only the `GlobalAlloc` impl, which has to be
//! `unsafe`; it forwards every call to the system allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ink_core::graph::{
    AudioBuffer, BGM_WRITE_CHUNK_FRAMES, BgmController, BgmFeeder, BgmPlayer, FlushPoll,
    LinkWriter, MicGraph, MicrophoneSource, MixerControl, SeController, SePolicy, SoundData,
    audio_link, bgm_channel, bgm_ring_frames, se_channel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Counts {
    allocs: usize,
    deallocs: usize,
    reallocs: usize,
    /// Allocations of exactly the size given to [`watch`] (also in `allocs`).
    watched_allocs: usize,
    /// Frees of exactly the size given to [`watch`] (also in `deallocs`).
    watched_deallocs: usize,
}

thread_local! {
    // `const` initializers: no lazy setup, so the allocator can use them
    // without allocating.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNTS: Cell<Counts> = const { Cell::new(Counts {
        allocs: 0,
        deallocs: 0,
        reallocs: 0,
        watched_allocs: 0,
        watched_deallocs: 0,
    }) };
    /// The size in bytes that is counted on its own; 0 for none.
    static WATCH: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

/// `update` is given the counts and the watched size.
fn note(update: impl FnOnce(&mut Counts, usize)) {
    // `try_with`: the thread-locals may be gone while a thread exits.
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let watched = WATCH.try_with(Cell::get).unwrap_or(0);
            let _ = COUNTS.try_with(|counts| {
                let mut value = counts.get();
                update(&mut value, watched);
                counts.set(value);
            });
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(|counts, watched| {
            counts.allocs += 1;
            if layout.size() == watched {
                counts.watched_allocs += 1;
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(|counts, watched| {
            counts.allocs += 1;
            if layout.size() == watched {
                counts.watched_allocs += 1;
            }
        });
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(|counts, watched| {
            counts.deallocs += 1;
            if layout.size() == watched {
                counts.watched_deallocs += 1;
            }
        });
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(|counts, _| counts.reallocs += 1);
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

/// Makes this thread count the allocations and frees of exactly `bytes`
/// bytes on their own (`watched_allocs`, `watched_deallocs`).
fn watch(bytes: usize) {
    WATCH.set(bytes);
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

// --- The background music (BGM) ---

/// A stand-in for the worker thread of the app: it writes a sine into the
/// ring buffer and follows the flush protocol, like the real one. It is not
/// counted (only the thread that renders is).
struct Worker {
    /// While true, the worker writes nothing (so the ring buffer runs dry).
    stall: Arc<AtomicBool>,
    /// The length of the song in frames; 0 for a song without an end. After
    /// that many frames, the worker marks the end.
    song_frames: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<BgmFeeder>>,
}

impl Worker {
    fn start(mut feeder: BgmFeeder) -> Self {
        let stall = Arc::new(AtomicBool::new(false));
        let song_frames = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (stall, song_frames, stop) = (
                Arc::clone(&stall),
                Arc::clone(&song_frames),
                Arc::clone(&stop),
            );
            thread::spawn(move || {
                let mut left = vec![0.0_f32; BGM_WRITE_CHUNK_FRAMES];
                let mut right = vec![0.0_f32; BGM_WRITE_CHUNK_FRAMES];
                let mut written = 0_usize;
                let mut end_marked = false;
                feeder.begin_stream(None);
                while !stop.load(Ordering::Relaxed) {
                    match feeder.poll_flush() {
                        FlushPoll::Waiting => {
                            thread::sleep(Duration::from_micros(50));
                            continue;
                        }
                        FlushPoll::Done => {
                            written = 0;
                            end_marked = false;
                            feeder.begin_stream(None);
                        }
                        FlushPoll::Idle => {}
                    }
                    if stall.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_micros(200));
                        continue;
                    }
                    let limit = song_frames.load(Ordering::Relaxed);
                    if limit > 0 && written >= limit {
                        if !end_marked {
                            feeder.mark_eof();
                            end_marked = true;
                        }
                        thread::sleep(Duration::from_micros(200));
                        continue;
                    }
                    let mut count = feeder.free_frames().min(BGM_WRITE_CHUNK_FRAMES);
                    if limit > 0 {
                        count = count.min(limit - written);
                    }
                    if count == 0 {
                        thread::sleep(Duration::from_micros(100));
                        continue;
                    }
                    for i in 0..count {
                        let phase = (written + i) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0;
                        left[i] = 0.1 * phase.sin();
                        right[i] = -left[i];
                    }
                    written += feeder.write(&left[..count], &right[..count]);
                }
                feeder
            })
        };
        Self {
            stall,
            song_frames,
            stop,
            handle: Some(handle),
        }
    }

    /// Stops the worker and gives the writing end back.
    fn finish(mut self) -> BgmFeeder {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap()
    }
}

/// The graph with the MIC, the BGM, and the SE, and what drives them.
struct Full {
    graph: MicGraph,
    control: Arc<MixerControl>,
    bgm: BgmController,
    se: SeController,
    worker: Worker,
    out: Vec<f32>,
}

fn full_graph() -> Full {
    let (_writer, reader, _) = audio_link(RATE, MAX_FRAMES);
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
        control,
        bgm,
        se,
        worker: Worker::start(feeder),
        out: vec![0.0; BIGGEST_BLOCK * 2],
    }
}

impl Full {
    /// Renders one block. This is the only thing that is counted.
    fn render(&mut self, frames: usize) {
        let out = &mut self.out[..frames * 2];
        let graph = &mut self.graph;
        counted(|| graph.render(out, 2));
    }

    /// Renders blocks of 256 frames until `done`. The worker has to be given
    /// time, so there is a short wait after every block.
    fn render_until(&mut self, what: &str, mut done: impl FnMut(&Full) -> bool) {
        let started = Instant::now();
        while !done(self) {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "timed out: {what}"
            );
            self.render(MAX_FRAMES);
            thread::sleep(Duration::from_micros(300));
        }
    }
}

/// Every path of the BGM and the SE, one after the other, then a random
/// mix. Rendering allocates nothing and frees nothing.
#[test]
fn rendering_the_bgm_and_the_se_does_not_allocate_or_free_on_any_path() {
    let mut full = full_graph();
    take();

    // The first start: priming, then a fade in, then playing.
    full.bgm.play();
    full.render_until("the first start", |f| f.bgm.stats().played_frames >= 20_000);
    assert_eq!(take(), Counts::default(), "a normal play");

    // Pause: the fade out, then nothing is read.
    full.bgm.pause();
    for _ in 0..4 {
        full.render(MAX_FRAMES);
    }
    let paused_at = full.bgm.stats().played_frames;
    for _ in 0..10 {
        full.render(MAX_FRAMES);
    }
    assert_eq!(full.bgm.stats().played_frames, paused_at, "paused");
    full.bgm.play();
    full.render_until("play after pause", |f| {
        f.bgm.stats().played_frames >= paused_at + 5_000
    });
    assert_eq!(take(), Counts::default(), "pause and play");

    // The ring buffer runs dry: the fade out for an underrun, the silence,
    // priming, and the fade in.
    full.worker.stall.store(true, Ordering::Relaxed);
    full.render_until("the ring buffer runs dry", |f| f.bgm.stats().underruns >= 1);
    for _ in 0..5 {
        full.render(MAX_FRAMES);
    }
    let dry = full.bgm.stats();
    assert!(dry.underrun_frames > 0, "{dry:?}");
    full.worker.stall.store(false, Ordering::Relaxed);
    let resumed_from = dry.played_frames;
    full.render_until("play after the underrun", |f| {
        f.bgm.stats().played_frames >= resumed_from + 5_000
    });
    assert_eq!(take(), Counts::default(), "an underrun and priming");

    // Stop: the fade out, the wait for the worker, and the ring buffer is
    // thrown away.
    full.bgm.request_flush();
    full.render_until("the flush", |f| f.bgm.is_flushed());
    assert_eq!(full.bgm.stats().played_frames, 0);
    full.bgm.play();
    full.render_until("play after stop", |f| f.bgm.stats().played_frames >= 5_000);
    assert_eq!(take(), Counts::default(), "stop and play again");

    // The end of a song that does not loop.
    full.worker.song_frames.store(20_000, Ordering::Relaxed);
    full.bgm.request_flush();
    full.render_until("the flush before the short song", |f| f.bgm.is_flushed());
    full.bgm.play();
    full.render_until("the end of the song", |f| f.bgm.is_finished());
    assert_eq!(full.bgm.stats().played_frames, 20_000);
    for _ in 0..3 {
        full.render(MAX_FRAMES);
    }
    assert_eq!(take(), Counts::default(), "the end of a song");
    // A stop after the end brings the song back to the start.
    full.bgm.request_flush();
    full.render_until("the flush after the end", |f| f.bgm.is_flushed());
    assert!(!full.bgm.is_finished());
    full.worker.song_frames.store(0, Ordering::Relaxed);
    assert_eq!(take(), Counts::default(), "stop after the end");

    // A random mix of everything, with the SE.
    let mut rng = Rng(2026);
    full.bgm.play();
    for round in 0..1500 {
        for _ in 0..rng.below(4) {
            match rng.below(14) {
                0 => full.bgm.play(),
                1 => full.bgm.pause(),
                2 => {
                    full.bgm.request_flush();
                    full.bgm.play();
                }
                3 => full
                    .worker
                    .stall
                    .store(rng.below(2) == 0, Ordering::Relaxed),
                4 => full
                    .worker
                    .song_frames
                    .store(rng.below(3) as usize * 15_000, Ordering::Relaxed),
                5 => full.control.bgm.set_gain_db(rng.below(40) as f32 - 30.0),
                6 => full.control.bgm.set_muted(rng.below(2) == 0),
                7 => {
                    let slot = rng.below(10) as usize;
                    let _ = full.se.play(slot, 1.0, SePolicy::Overlap);
                }
                8 => {
                    let slot = rng.below(10) as usize;
                    full.se.set_sound(slot, sound(&mut rng)).unwrap();
                }
                9 => {
                    let _ = full.se.stop_all();
                }
                10 => {
                    full.se.collect();
                }
                11 => full.control.se.set_muted(rng.below(2) == 0),
                _ => full.control.master.set_gain_db(rng.below(30) as f32 - 24.0),
            }
        }
        let frames = 1 + rng.below(BIGGEST_BLOCK as u64) as usize;
        full.render(frames);
        if round % 4 == 0 {
            thread::sleep(Duration::from_micros(200));
        }
    }
    assert_eq!(take(), Counts::default(), "the random mix");
    full.worker.stall.store(false, Ordering::Relaxed);
    full.worker.song_frames.store(0, Ordering::Relaxed);
    assert_eq!(full.se.stats().leaked, 0);
    let Full { worker, .. } = full;
    drop(worker.finish());
}

/// The fade of a Pause that is interrupted by Play while only a little is
/// left in the ring buffer is cut, and the next fade starts from 0 again.
/// The worker is replaced by the test, which writes exact amounts.
#[test]
fn a_cut_fade_does_not_allocate_or_free() {
    let (_writer, reader, _) = audio_link(RATE, MAX_FRAMES);
    let control = Arc::new(MixerControl::new());
    let (bgm, mut feeder, player) = bgm_channel(RATE, MAX_FRAMES);
    let mut graph = MicGraph::new(
        MicrophoneSource::new(reader),
        Arc::clone(&control),
        RATE,
        MAX_FRAMES,
    )
    .with_bgm(player);
    let mut out = vec![0.0_f32; MAX_FRAMES * 2];
    let mut render = |graph: &mut MicGraph| {
        counted(|| graph.render(&mut out, 2));
    };

    // A block is 256 frames and a fade 480: playing needs 736 at the start of
    // a block. 5600 frames are 4800 (priming) and then 19 blocks and 736 over.
    let total = 5_600;
    let sine: Vec<f32> = (0..total).map(|i| 0.1 * (i as f32 * 0.05).sin()).collect();
    let negative: Vec<f32> = sine.iter().map(|s| -s).collect();
    assert_eq!(feeder.write(&sine, &negative), total);
    bgm.play();
    take();

    for _ in 0..20 {
        render(&mut graph);
    }
    // The 20th block started with exactly one block and one fade.
    assert_eq!(bgm.stats().fill_frames, 736);
    assert_eq!(bgm.stats().underruns, 0);

    // Pause: the fade reads one block of its 480 frames.
    bgm.pause();
    render(&mut graph);
    // Play: it plays again, finds 224 frames, and fades out for an underrun
    // that needs 480: the fade is cut.
    assert_eq!(bgm.stats().fill_frames, 480);
    bgm.play();
    render(&mut graph);
    let stats = bgm.stats();
    assert_eq!(stats.underruns, 1);
    assert_eq!(stats.played_frames, total as u64, "every frame was read");

    // The next start: priming, then a fade in from 0.
    assert_eq!(feeder.write(&sine, &negative), total);
    for _ in 0..4 {
        render(&mut graph);
    }
    assert_eq!(bgm.stats().played_frames, total as u64 + 1024);
    assert_eq!(take(), Counts::default(), "a cut fade");
}

/// The ring buffer is freed by the last of the parts that hold it. These
/// tests count the frees of its size, in bytes, on the thread that drops the
/// player (the audio backend does it on its own thread) and on the others.
const ODD_RATE: u32 = 47_999;
const RING_BYTES: usize = 47_999 * 2 * size_of::<f32>();

fn ring_parts() -> (BgmController, BgmFeeder, BgmPlayer) {
    // A rate that nothing else uses, so that only the ring buffer has this
    // size.
    assert_eq!(
        bgm_ring_frames(ODD_RATE, MAX_FRAMES) * 2 * size_of::<f32>(),
        RING_BYTES
    );
    watch(RING_BYTES);
    take();
    let parts = counted(|| bgm_channel(ODD_RATE, MAX_FRAMES));
    let made = take();
    assert_eq!(
        made.watched_allocs, 1,
        "the ring buffer is one block of memory"
    );
    parts
}

/// Drops `value` on a new thread and returns what that thread counted.
fn drop_on_a_new_thread<T: Send + 'static>(value: T) -> Counts {
    thread::spawn(move || {
        watch(RING_BYTES);
        take();
        counted(|| drop(value));
        take()
    })
    .join()
    .unwrap()
}

/// Drops `value` on this thread and returns what was counted.
fn drop_here<T>(value: T) -> Counts {
    take();
    counted(|| drop(value));
    take()
}

#[test]
fn when_the_controller_lives_longer_the_control_side_frees_the_ring_buffer() {
    let (controller, feeder, player) = ring_parts();

    // The stream is closed: the player is dropped on the thread of the
    // audio backend. The ring buffer is not freed there.
    let on_the_audio_thread = drop_on_a_new_thread(player);
    assert_eq!(on_the_audio_thread.watched_deallocs, 0);

    // The worker ends. The ring buffer is not freed yet either: the keeper
    // of the controller holds the reading end.
    let worker_end = drop_here(feeder);
    assert_eq!(worker_end.watched_deallocs, 0);

    // The controller is dropped on the control side: it frees the ring
    // buffer, once.
    let control_side = drop_here(controller);
    assert_eq!(control_side.watched_deallocs, 1);
}

#[test]
fn when_the_worker_ends_last_the_worker_frees_the_ring_buffer() {
    let (controller, feeder, player) = ring_parts();
    assert_eq!(drop_on_a_new_thread(player).watched_deallocs, 0);
    assert_eq!(drop_here(controller).watched_deallocs, 0);
    // The writing end is the last one that holds the ring buffer.
    let worker_thread = drop_on_a_new_thread(feeder);
    assert_eq!(worker_thread.watched_deallocs, 1);
}

/// A LIMIT, not a wish: if the controller is dropped before the player, the
/// last reference to the ring buffer is the player's, and the thread that
/// drops the player frees it (once, when the stream is closed; never while
/// it plays). The session drops the parts in the order of the first test.
#[test]
fn limit_when_the_controller_is_dropped_first_the_audio_thread_frees_the_ring_buffer() {
    let (controller, feeder, player) = ring_parts();
    assert_eq!(drop_here(controller).watched_deallocs, 0);
    assert_eq!(drop_here(feeder).watched_deallocs, 0);
    let on_the_audio_thread = drop_on_a_new_thread(player);
    assert_eq!(on_the_audio_thread.watched_deallocs, 1, "the known limit");
}
