//! `asset_info` on real WAV and MP3 files, and through the sound library of
//! `ink-core` (ADR-0010): import, scan, and the index that is saved.
//!
//! The WAV files are made here (silence with a correct header); the MP3 files
//! are the ones in `tests/fixtures`.

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ink_core::{
    AssetFormat, AssetId, AssetKind, AssetState, ImportError, ImportOutcome, Library, ProbeFailure,
    SE_MAX_DURATION,
};
use ink_media::{asset_info, probe};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A fresh folder under the system temp dir, removed when it is dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ink-media-asset-info-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn id(text: &str) -> AssetId {
    text.parse().unwrap()
}

/// The header of a 16-bit PCM WAV file with `data_len` bytes of sound.
fn wav_header(sample_rate: u32, channels: u16, data_len: u32) -> Vec<u8> {
    let block_align = channels * 2;
    let mut bytes = Vec::with_capacity(44);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes
}

/// A WAV file of `frames` frames of silence.
fn wav(sample_rate: u32, channels: u16, frames: u32) -> Vec<u8> {
    let data_len = frames * u32::from(channels) * 2;
    let mut bytes = wav_header(sample_rate, channels, data_len);
    bytes.resize(bytes.len() + data_len as usize, 0);
    bytes
}

fn write(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

// -- WAV ------------------------------------------------------------------------

#[test]
fn a_wav_file_says_its_format_rate_channels_and_frames() {
    let dir = TempDir::new("wav");
    let path = write(&dir, "tone.wav", &wav(48_000, 2, 48_000));
    let info = asset_info(&path).unwrap();
    assert_eq!(info.format, AssetFormat::Wav);
    assert_eq!(info.sample_rate, 48_000);
    assert_eq!(info.channels, 2);
    assert_eq!(info.frames, Some(48_000));
    assert_eq!(info.duration(), Some(std::time::Duration::from_secs(1)));
}

#[test]
fn every_sample_rate_and_channel_count_comes_through() {
    let dir = TempDir::new("rates");
    for (rate, channels, frames) in [
        (8_000, 1, 8_000),
        (22_050, 1, 11_025),
        (44_100, 2, 44_100),
        (48_000, 1, 24_000),
        (96_000, 2, 9_600),
        (192_000, 2, 19_200),
    ] {
        let path = write(
            &dir,
            &format!("{rate}-{channels}.wav"),
            &wav(rate, channels, frames),
        );
        let info = asset_info(&path).unwrap();
        assert_eq!(info.sample_rate, rate);
        assert_eq!(info.channels, channels);
        assert_eq!(info.frames, Some(u64::from(frames)));
        assert_eq!(info.format, AssetFormat::Wav);
    }
}

// -- MP3 ------------------------------------------------------------------------

#[test]
fn an_mp3_file_with_a_lame_header_says_the_frames_that_are_played() {
    let path = fixture("tone-48k-stereo-gapless.mp3");
    let info = asset_info(&path).unwrap();
    assert_eq!(info.format, AssetFormat::Mp3);
    assert_eq!(info.sample_rate, 48_000);
    assert_eq!(info.channels, 2);
    // 0.5 seconds: the delay and the padding of the encoder are not counted.
    assert_eq!(info.frames, Some(24_000));
    assert_eq!(info.frames, probe(&path).unwrap().frames());
}

#[test]
fn the_mp3_fixtures_say_what_ink_media_says_of_them() {
    for (name, rate, channels) in [
        ("tone-48k-stereo-gapless.mp3", 48_000, 2),
        ("tone-44k-stereo-id3.mp3", 44_100, 2),
        ("tone-44k-mono.mp3", 44_100, 1),
        ("tone-48k-stereo-no-header.mp3", 48_000, 2),
    ] {
        let path = fixture(name);
        let info = asset_info(&path).unwrap();
        let probed = probe(&path).unwrap();
        assert_eq!(info.format, AssetFormat::Mp3, "{name}");
        assert_eq!(info.sample_rate, rate, "{name}");
        assert_eq!(info.channels, channels, "{name}");
        assert_eq!(info.sample_rate, probed.sample_rate(), "{name}");
        assert_eq!(info.channels, probed.channels(), "{name}");
        assert_eq!(info.frames, probed.frames(), "{name}");
    }
}

// -- files that are not sounds ----------------------------------------------------

#[test]
fn files_that_are_not_sounds_are_not_audio() {
    let dir = TempDir::new("not-audio");
    let good = wav(48_000, 2, 4_800);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty.wav", Vec::new()),
        ("text.wav", b"this is not a sound file at all".to_vec()),
        (
            "noise.mp3",
            (0..4096_u32).map(|n| (n * 7 + 13) as u8).collect(),
        ),
        ("header-only.wav", wav_header(48_000, 2, 0)),
        ("cut-header.wav", good[..20].to_vec()),
        ("zero-rate.wav", wav(0, 2, 100)),
    ];
    for (name, bytes) in cases {
        let path = write(&dir, name, &bytes);
        let failure = asset_info(&path).unwrap_err();
        assert!(
            matches!(failure, ProbeFailure::NotAudio(_)),
            "{name}: {failure}"
        );
    }
}

#[test]
fn a_file_that_is_cut_off_says_what_its_header_says() {
    // `asset_info` does not decode: a file that ends sooner than its header
    // says still tells the header's length. It is played up to where it ends
    // (ink-media's reader), and a scan reads it again whenever it changes.
    let dir = TempDir::new("cut");
    let good = wav(48_000, 2, 48_000);
    let cut = write(&dir, "cut.wav", &good[..good.len() / 2]);
    let info = asset_info(&cut).unwrap();
    assert_eq!(info.format, AssetFormat::Wav);
    assert_eq!(info.frames, Some(48_000));

    let mp3 = fs::read(fixture("tone-48k-stereo-gapless.mp3")).unwrap();
    let cut_mp3 = write(&dir, "cut.mp3", &mp3[..mp3.len() / 2]);
    let info = asset_info(&cut_mp3).unwrap();
    assert_eq!(info.format, AssetFormat::Mp3);
    assert_eq!(info.frames, Some(24_000));
}

// -- files that cannot be read ------------------------------------------------------

#[test]
fn a_file_that_is_not_there_is_unreadable() {
    let dir = TempDir::new("missing");
    let failure = asset_info(&dir.join("missing.wav")).unwrap_err();
    assert!(matches!(failure, ProbeFailure::Unreadable(_)), "{failure}");
}

#[cfg(unix)]
#[test]
fn a_file_with_no_permission_is_unreadable() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new("denied");
    let path = write(&dir, "locked.wav", &wav(48_000, 2, 100));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let readable_anyway = fs::File::open(&path).is_ok();
    let result = asset_info(&path);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    // A user who can read everything (root) is not stopped by the mode.
    if !readable_anyway {
        let failure = result.unwrap_err();
        assert!(matches!(failure, ProbeFailure::Unreadable(_)), "{failure}");
    }
}

// -- big files --------------------------------------------------------------------

/// A WAV file of 2 GiB (without writing it: the file system keeps the empty
/// part as a hole).
fn big_wav(dir: &TempDir) -> (PathBuf, u32) {
    let data_len = 0x7FFF_FFD0_u32;
    let path = dir.join("big.wav");
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(&wav_header(48_000, 2, data_len)).unwrap();
    file.set_len(44 + u64::from(data_len)).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    (path, data_len)
}

#[test]
fn a_file_of_2_gigabytes_is_read_by_its_header_only() {
    let dir = TempDir::new("big");
    let (path, data_len) = big_wav(&dir);
    let info = asset_info(&path).unwrap();
    assert_eq!(info.frames, Some(u64::from(data_len) / 4));
    assert_eq!(info.sample_rate, 48_000);
    assert!(info.duration().unwrap() > std::time::Duration::from_secs(3 * 3600));
}

#[test]
fn a_big_file_as_a_sound_effect_is_refused_before_anything_is_copied() {
    let dir = TempDir::new("big-se");
    let (path, _) = big_wav(&dir);
    let root = TempDir::new("big-se-root");
    let mut library = Library::open(root.path()).unwrap().library;
    let error = library
        .import(&path, AssetKind::Se, &asset_info)
        .unwrap_err();
    assert!(matches!(error, ImportError::TooLongForSe { .. }), "{error}");
    assert_eq!(
        fs::read_dir(library.folder(AssetKind::Se)).unwrap().count(),
        0
    );
}

#[test]
fn a_file_of_tens_of_megabytes_is_imported_as_a_bgm() {
    let dir = TempDir::new("tens");
    let path = dir.join("long.wav");
    let data_len = 64 * 1024 * 1024_u32;
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(&wav_header(48_000, 2, data_len)).unwrap();
    file.set_len(44 + u64::from(data_len)).unwrap();
    drop(file);

    let root = TempDir::new("tens-root");
    let mut library = Library::open(root.path()).unwrap().library;
    let outcome = library.import(&path, AssetKind::Bgm, &asset_info).unwrap();
    assert_eq!(outcome, ImportOutcome::Imported(id("bgm-0001")));
    let copy = library.path_of(&id("bgm-0001")).unwrap();
    assert_eq!(fs::metadata(copy).unwrap().len(), 44 + u64::from(data_len));
    assert_eq!(
        library.get(&id("bgm-0001")).unwrap().info().unwrap().frames,
        Some(u64::from(data_len) / 4)
    );
}

// -- through the sound library ------------------------------------------------------

#[test]
fn an_mp3_with_a_japanese_name_goes_through_import_and_is_found_again() {
    let sources = TempDir::new("jp-sources");
    let source = sources.join("音楽 テスト.mp3");
    fs::copy(fixture("tone-48k-stereo-gapless.mp3"), &source).unwrap();

    let root = TempDir::new("jp-root");
    let mut library = Library::open(root.path()).unwrap().library;
    let outcome = library
        .import(&source, AssetKind::Bgm, &asset_info)
        .unwrap();
    assert_eq!(outcome, ImportOutcome::Imported(id("bgm-0001")));

    let asset = library.get(&id("bgm-0001")).unwrap();
    assert_eq!(asset.name(), "音楽 テスト");
    assert_eq!(asset.relative_path(), "音楽 テスト.mp3");
    assert_eq!(asset.state(), AssetState::Available);
    assert_eq!(asset.info().copied(), Some(asset_info(&source).unwrap()));
    assert_eq!(asset.info().unwrap().format, AssetFormat::Mp3);
    assert_eq!(asset.info().unwrap().frames, Some(24_000));

    let copy = library.path_of(&id("bgm-0001")).unwrap();
    assert_eq!(fs::read(&copy).unwrap(), fs::read(&source).unwrap());
    // The copy reads the same as the source.
    assert_eq!(asset_info(&copy).unwrap(), asset_info(&source).unwrap());

    // The index has it, and the second import is the same Asset.
    let reopened = Library::open(root.path()).unwrap().library;
    assert_eq!(reopened.get(&id("bgm-0001")).unwrap().info(), asset.info());
    let outcome = library
        .import(&source, AssetKind::Bgm, &asset_info)
        .unwrap();
    assert_eq!(outcome, ImportOutcome::AlreadyThere(id("bgm-0001")));
}

#[test]
fn the_limit_of_a_sound_effect_is_the_real_length_of_the_file() {
    let sources = TempDir::new("limit-sources");
    // 8 kHz mono: a minute is not big.
    let sixty = write(&sources, "sixty.wav", &wav(8_000, 1, 60 * 8_000));
    let over = write(&sources, "over.wav", &wav(8_000, 1, 61 * 8_000));
    let root = TempDir::new("limit-root");
    let mut library = Library::open(root.path()).unwrap().library;

    match library
        .import(&over, AssetKind::Se, &asset_info)
        .unwrap_err()
    {
        ImportError::TooLongForSe { limit, length } => {
            assert_eq!(limit, SE_MAX_DURATION);
            assert_eq!(length, std::time::Duration::from_secs(61));
        }
        other => panic!("{other}"),
    }
    assert!(library.import(&sixty, AssetKind::Se, &asset_info).is_ok());
    assert!(library.import(&over, AssetKind::Bgm, &asset_info).is_ok());
}

#[test]
fn files_that_are_not_sounds_are_refused_at_import_with_the_reason() {
    let sources = TempDir::new("refuse-sources");
    let text = write(&sources, "text.wav", b"not a sound");
    let missing = sources.join("missing.wav");
    let root = TempDir::new("refuse-root");
    let mut library = Library::open(root.path()).unwrap().library;

    let error = library
        .import(&text, AssetKind::Se, &asset_info)
        .unwrap_err();
    assert!(matches!(error, ImportError::NotAudio(_)), "{error}");
    let error = library
        .import(&missing, AssetKind::Se, &asset_info)
        .unwrap_err();
    assert!(
        matches!(error, ImportError::Source(ref e) if e.kind() == std::io::ErrorKind::NotFound),
        "{error}"
    );
    assert_eq!(
        fs::read_dir(library.folder(AssetKind::Se)).unwrap().count(),
        0
    );
}

#[test]
fn open_import_and_scan_work_together_on_real_files() {
    let sources = TempDir::new("flow-sources");
    let song = write(&sources, "song.wav", &wav(48_000, 2, 48_000));
    let clap = sources.join("clap.mp3");
    fs::copy(fixture("tone-44k-mono.mp3"), &clap).unwrap();

    let root = TempDir::new("flow-root");
    let opened = Library::open(root.path()).unwrap();
    assert!(opened.warnings.is_empty());
    let mut library = opened.library;
    assert_eq!(
        library.import(&song, AssetKind::Bgm, &asset_info).unwrap(),
        ImportOutcome::Imported(id("bgm-0001"))
    );
    assert_eq!(
        library.import(&clap, AssetKind::Se, &asset_info).unwrap(),
        ImportOutcome::Imported(id("se-0002"))
    );

    // Two files put in by hand: a sound, and a file that only has the name.
    let bgm = library.folder(AssetKind::Bgm);
    fs::write(bgm.join("hand.wav"), wav(44_100, 1, 88_200)).unwrap();
    fs::write(bgm.join("broken.wav"), b"not a sound").unwrap();
    let report = library.scan(&asset_info).unwrap();
    assert_eq!(report.added, [id("bgm-0003"), id("bgm-0004")]);
    assert_eq!(report.unreadable.len(), 1);
    assert_eq!(report.unreadable[0].0, id("bgm-0003"));
    assert!(matches!(report.unreadable[0].1, ProbeFailure::NotAudio(_)));
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let state = |library: &Library, text: &str| library.get(&id(text)).unwrap().state();
    assert_eq!(state(&library, "bgm-0003"), AssetState::Unreadable);
    assert_eq!(state(&library, "bgm-0004"), AssetState::Available);
    let hand = library
        .get(&id("bgm-0004"))
        .unwrap()
        .info()
        .copied()
        .unwrap();
    assert_eq!(
        (hand.format, hand.sample_rate, hand.channels, hand.frames),
        (AssetFormat::Wav, 44_100, 1, Some(88_200))
    );

    // The file of the sound effect goes away.
    fs::remove_file(library.path_of(&id("se-0002")).unwrap()).unwrap();
    let report = library.scan(&asset_info).unwrap();
    assert_eq!(report.missing, [id("se-0002")]);
    assert!(report.added.is_empty());
    assert_eq!(state(&library, "se-0002"), AssetState::Missing);

    // After a restart the index has what was read, and the states are found
    // again; a scan has nothing new to tell.
    let mut again = Library::open(root.path()).unwrap().library;
    assert_eq!(again.assets().len(), 4);
    assert_eq!(
        again.get(&id("bgm-0004")).unwrap().info().copied(),
        Some(hand)
    );
    assert_eq!(
        again
            .get(&id("se-0002"))
            .unwrap()
            .info()
            .unwrap()
            .sample_rate,
        44_100
    );
    assert_eq!(state(&again, "se-0002"), AssetState::Missing);
    assert_eq!(state(&again, "bgm-0003"), AssetState::Unreadable);
    assert!(again.scan(&asset_info).unwrap().is_empty());

    // The broken file is replaced by a good one: restored, with the same ID.
    fs::write(bgm.join("broken.wav"), wav(48_000, 2, 480)).unwrap();
    let report = again.scan(&asset_info).unwrap();
    assert_eq!(report.restored, [id("bgm-0003")]);
    let fixed = Library::open(root.path()).unwrap().library;
    assert_eq!(
        fixed.get(&id("bgm-0003")).unwrap().info().unwrap().frames,
        Some(480)
    );
}
