//! Import for the sound library (ADR-0010): put a copy of a sound file in the
//! sound folder of its kind, and give it an Asset.
//!
//! The library does not depend on where the file came from (ARCHITECTURE.md
//! section 9): the file is copied into `sounds/bgm` or `sounds/se`, and the
//! copy is the one that is used.
//!
//! [`Library::import`] goes in this order, and stops at the first step that
//! fails, with nothing left behind:
//!
//! 1. the source is a file (a link is followed), and is not already in the
//!    sound folders;
//! 2. its extension is `wav` or `mp3` (in any case), and `probe` can read it;
//! 3. a sound effect is not longer than [`SE_MAX_DURATION`] (a length that the
//!    file does not say is let through);
//! 4. the name of the copy is chosen: the name of the source made safe for
//!    every OS (see below), and if the name is taken (in any case of the
//!    letters: Windows would take `Song.wav` and `song.wav` for the same
//!    file), `name (2).ext`, `name (3).ext`, ... up to 1,000. If the name is
//!    taken by an Asset **with the same content** (the sizes are compared
//!    first, then the bytes, from the start, with two buffers of a fixed size),
//!    nothing is copied and that Asset is returned;
//! 5. the source is copied to a temporary file in the folder
//!    (`.import-<n>.tmp`, a name that a scan ignores), flushed to the disk, and
//!    given its name without replacing a file that is there;
//! 6. the index is saved with the new Asset. If saving fails, the copy is
//!    removed.
//!
//! Whatever step fails, the temporary file is removed and the source is only
//! read. The ID is made at the start but is only given (and `next_id` only
//! moved) when the index has been saved: a failed import gives no ID.
//!
//! A file with a different name and the same content is a different Asset
//! (there is no hash). A file that is in the folder but is not in the index
//! (put there by hand and not scanned yet) is not an Asset: it only keeps its
//! name taken, so the import goes on to the next number.
//!
//! The name of the copy is the name of the source, with these changes: the
//! characters that Windows cannot have in a name (`\ / : < > " | ?` `*` and
//! control characters), and a dot at the start, become `_`; a dot or a space at
//! the end of the part before the extension become `_`; a name that Windows
//! keeps for a device (`CON`, `NUL`, `COM1`, ...) gets a `_` before it; a name
//! of more than 255 bytes is cut in the part before the extension, at the
//! border of a character, so that the extension and the ` (n)` still fit. The
//! extension keeps its case. A name that is not UTF-8 is refused.
//!
//! This copies files of any size: call it from a worker thread. There is no
//! progress and no cancel.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::asset::{Asset, AssetId, AssetKind, AssetState, ProbeFailure, SE_MAX_DURATION};
use crate::library::{Library, LibraryError, folder_of, issue_id, join_relative};
use crate::library_index::{default_name, path_problem};
use crate::library_scan::{EXTENSIONS, ProbeFn};

/// The most bytes in the name of a file (Linux; Windows counts characters, so
/// this is the stricter of the two).
const MAX_NAME_BYTES: usize = 255;

/// The most numbers tried for a name: `name`, `name (2)`, ... `name (1000)`.
const MAX_NUMBER: usize = 1000;

/// The most names tried for the temporary file.
const MAX_TEMP_NAMES: usize = 1000;

/// The size of each of the two buffers that compare the content of two files.
const COMPARE_BUFFER_BYTES: usize = 64 * 1024;

/// What an import did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum ImportOutcome {
    /// The file was copied and this is its new Asset.
    Imported(AssetId),
    /// A file with the same name and the same content is already in the
    /// library: nothing was copied, and this is its Asset.
    AlreadyThere(AssetId),
}

/// Why an import was refused or failed. Nothing is left behind in any case.
///
/// The text of an error is one line in English, for the log and the CLI. The
/// window chooses its own (Japanese) words from the variant.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImportError {
    /// The source is not a file (a folder, a device, ...).
    NotAFile,
    /// The source is in the sound folders of the library already. A scan
    /// picks it up.
    AlreadyInLibrary,
    /// The name of the source is not UTF-8 (only on Linux).
    NameNotUtf8,
    /// The extension is not `wav` or `mp3`. `extension` is what the name has
    /// (`None` if it has none).
    UnsupportedFormat { extension: Option<String> },
    /// The file is not a sound that can be played.
    NotAudio(String),
    /// The file could not be read to find out what it is.
    Unreadable(String),
    /// A sound effect longer than `limit`. Register it as a BGM instead.
    TooLongForSe { limit: Duration, length: Duration },
    /// The source could not be read (it is not there, no permission, ...).
    Source(io::Error),
    /// The sound folder could not be written (no permission, ...).
    Destination(io::Error),
    /// The disk is full (or the quota is used up) while writing the copy.
    DiskFull,
    /// The name and the 999 numbers after it are all taken.
    TooManyNames,
    /// The library could not make an ID or save the index. The copy was
    /// removed.
    Library(LibraryError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAFile => f.write_str("the source is not a file"),
            Self::AlreadyInLibrary => f.write_str("the source is already in the sound folders"),
            Self::NameNotUtf8 => f.write_str("the name of the source is not valid UTF-8"),
            Self::UnsupportedFormat { extension: Some(e) } => {
                write!(
                    f,
                    "files with the extension .{e} cannot be imported (wav or mp3)"
                )
            }
            Self::UnsupportedFormat { extension: None } => {
                f.write_str("a file with no extension cannot be imported (wav or mp3)")
            }
            Self::NotAudio(reason) => write!(f, "not a sound that can be played: {reason}"),
            Self::Unreadable(reason) => write!(f, "cannot be read: {reason}"),
            Self::TooLongForSe { limit, length } => write!(
                f,
                "a sound effect can be {} seconds at most, this one is {:.1} seconds",
                limit.as_secs(),
                length.as_secs_f64()
            ),
            Self::Source(e) => write!(f, "cannot read the source: {e}"),
            Self::Destination(e) => write!(f, "cannot write the copy: {e}"),
            Self::DiskFull => f.write_str("the disk is full"),
            Self::TooManyNames => write!(
                f,
                "the name and the numbers up to ({MAX_NUMBER}) are all taken"
            ),
            Self::Library(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(e) | Self::Destination(e) => Some(e),
            Self::Library(e) => Some(e),
            _ => None,
        }
    }
}

impl From<LibraryError> for ImportError {
    fn from(e: LibraryError) -> Self {
        Self::Library(e)
    }
}

impl Library {
    /// Copies the sound file `source` into the sound folder of `kind` and
    /// gives it an Asset (see the module documentation).
    ///
    /// `probe` reads the source (not the copy) once.
    ///
    /// # Errors
    ///
    /// See [`ImportError`]. In every case the library is as it was, the source
    /// is untouched, and no file is left in the sound folder.
    pub fn import(
        &mut self,
        source: &Path,
        kind: AssetKind,
        probe: ProbeFn<'_>,
    ) -> Result<ImportOutcome, ImportError> {
        let metadata = fs::metadata(source).map_err(ImportError::Source)?;
        if !metadata.is_file() {
            return Err(ImportError::NotAFile);
        }
        if self.is_inside(source) {
            return Err(ImportError::AlreadyInLibrary);
        }
        let file_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ImportError::NameNotUtf8)?;
        let (stem, extension) = split_name(file_name)?;

        let info = probe(source).map_err(|failure| match failure {
            ProbeFailure::NotAudio(reason) => ImportError::NotAudio(reason),
            ProbeFailure::Unreadable(reason) => ImportError::Unreadable(reason),
        })?;
        if kind == AssetKind::Se
            && let Some(length) = info.duration()
            && length > SE_MAX_DURATION
        {
            return Err(ImportError::TooLongForSe {
                limit: SE_MAX_DURATION,
                length,
            });
        }

        // The ID is made now, so that running out of numbers is found before
        // a file is copied. It is given when the index has been saved.
        let mut next_id = self.next_id;
        let id = issue_id(kind, &mut next_id)?;

        let folder = self.folder(kind);
        let mut taken = taken_names(&folder)?;
        let owners: HashMap<String, (AssetId, String)> = self
            .assets
            .iter()
            .filter(|asset| asset.kind() == kind)
            .map(|asset| {
                (
                    asset.relative_path().to_lowercase(),
                    (*asset.id(), asset.relative_path().to_string()),
                )
            })
            .collect();
        let stem = clean_stem(stem, extension);

        let mut temp: Option<TempFile> = None;
        let mut number = 1;
        let name = loop {
            if number > MAX_NUMBER {
                return Err(ImportError::TooManyNames);
            }
            let name = candidate_name(&stem, number, extension);
            let key = name.to_lowercase();
            if let Some((owner, exact)) = owners.get(&key) {
                let existing = join_relative(folder.clone(), exact);
                if same_content(source, metadata.len(), &existing)? {
                    return Ok(ImportOutcome::AlreadyThere(*owner));
                }
                number += 1;
                continue;
            }
            if taken.contains(&key) {
                number += 1;
                continue;
            }
            let copy = match &mut temp {
                Some(copy) => copy,
                slot @ None => slot.insert(copy_to_temp(source, &folder)?),
            };
            match place(&copy.path, &folder.join(&name)) {
                Ok(()) => break name,
                // Somebody took the name since it was looked at.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    taken.insert(key);
                    number += 1;
                }
                Err(e) => return Err(destination_error(e)),
            }
        };
        drop(temp);

        let mut updated = self.assets.clone();
        updated.push(Asset::new(
            id,
            default_name(&name),
            name.clone(),
            Some(info),
            AssetState::Available,
        ));
        if let Err(e) = self.save(&updated, next_id) {
            // No file without an Asset.
            let _ = fs::remove_file(folder.join(&name));
            return Err(e.into());
        }
        self.assets = updated;
        self.next_id = next_id;
        Ok(ImportOutcome::Imported(id))
    }

    /// Returns whether `source` is inside one of the sound folders. Links are
    /// resolved on both sides.
    fn is_inside(&self, source: &Path) -> bool {
        let Ok(source) = fs::canonicalize(source) else {
            return false;
        };
        AssetKind::ALL.iter().any(|kind| {
            fs::canonicalize(folder_of(&self.root, *kind))
                .is_ok_and(|folder| source.starts_with(folder))
        })
    }
}

/// Splits the name of a source into the part before the last dot and the
/// extension, which must be one that can be imported.
fn split_name(file_name: &str) -> Result<(&str, &str), ImportError> {
    match file_name.rsplit_once('.') {
        Some((stem, extension))
            if EXTENSIONS
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported)) =>
        {
            Ok((stem, extension))
        }
        Some((_, extension)) => Err(ImportError::UnsupportedFormat {
            extension: Some(extension.to_string()),
        }),
        None => Err(ImportError::UnsupportedFormat { extension: None }),
    }
}

/// Makes the part of a name before the extension safe: the same on every OS.
/// The result is not empty, and with `extension` it is a name that
/// `path_problem` accepts.
fn clean_stem(stem: &str, extension: &str) -> String {
    let mut cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\\' | '/' | ':' | '<' | '>' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    // A name that starts with a dot is hidden: a scan would ignore it.
    let dots = cleaned.len() - cleaned.trim_start_matches('.').len();
    cleaned.replace_range(..dots, &"_".repeat(dots));
    if cleaned.is_empty() {
        cleaned.push('_');
    }
    // A name that Windows keeps for a device.
    if path_problem(&format!("{cleaned}.{extension}")).is_some() {
        cleaned.insert(0, '_');
    }
    cleaned
}

/// Returns the name of the copy for `number` (1 is the name as it is, 2 is
/// ` (2)` after it, ...), of at most [`MAX_NAME_BYTES`] bytes.
fn candidate_name(stem: &str, number: usize, extension: &str) -> String {
    let suffix = if number == 1 {
        String::new()
    } else {
        format!(" ({number})")
    };
    let room = MAX_NAME_BYTES - suffix.len() - 1 - extension.len();
    let mut end = stem.len().min(room);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &stem[..end];
    // A dot or a space at the end of what is left (Windows drops them).
    let kept = cut.trim_end_matches(['.', ' ']);
    let replaced = "_".repeat(cut.len() - kept.len());
    let name = format!("{kept}{replaced}{suffix}.{extension}");
    debug_assert!(path_problem(&name).is_none(), "{name}");
    debug_assert!(name.len() <= MAX_NAME_BYTES, "{name}");
    name
}

/// Returns the names in `folder`, in lower case: they are taken, whatever is
/// at the name.
fn taken_names(folder: &Path) -> Result<HashSet<String>, ImportError> {
    let mut names = HashSet::new();
    for entry in fs::read_dir(folder).map_err(ImportError::Destination)? {
        let entry = entry.map_err(ImportError::Destination)?;
        names.insert(entry.file_name().to_string_lossy().to_lowercase());
    }
    Ok(names)
}

/// A temporary file in the sound folder. It is removed when it is dropped.
struct TempFile {
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // The error that matters is the one that made the import fail, and a
        // file that is not there (it was renamed) is nothing to remove.
        let _ = fs::remove_file(&self.path);
    }
}

/// Copies `source` to a new temporary file in `folder`, and flushes it to the
/// disk.
fn copy_to_temp(source: &Path, folder: &Path) -> Result<TempFile, ImportError> {
    let mut reader = fs::File::open(source).map_err(ImportError::Source)?;
    let (mut file, temp) = create_temp(folder)?;
    copy_stream(&mut reader, &mut file)?;
    file.sync_all().map_err(destination_error)?;
    Ok(temp)
}

/// Creates the temporary file `.import-<n>.tmp` for the smallest `n` that is
/// free (a file left by a crash is not touched).
fn create_temp(folder: &Path) -> Result<(fs::File, TempFile), ImportError> {
    for n in 1..=MAX_TEMP_NAMES {
        let path = folder.join(format!(".import-{n}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            // The guard exists from the moment the file does.
            Ok(file) => return Ok((file, TempFile { path })),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(destination_error(e)),
        }
    }
    Err(ImportError::Destination(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "all the names for a temporary file are taken",
    )))
}

/// Copies all of `reader` to `writer` in a stream (the memory is the same for
/// a file of any size), and tells a failure of the reader from a failure of
/// the writer.
fn copy_stream<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> Result<(), ImportError> {
    struct Tracked<'a, R> {
        inner: &'a mut R,
        failed: bool,
    }
    impl<R: Read> Read for Tracked<'_, R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let result = self.inner.read(buffer);
            if let Err(e) = &result
                && e.kind() != io::ErrorKind::Interrupted
            {
                self.failed = true;
            }
            result
        }
    }

    let mut tracked = Tracked {
        inner: reader,
        failed: false,
    };
    match io::copy(&mut tracked, writer) {
        Ok(_) => Ok(()),
        Err(e) if tracked.failed => Err(ImportError::Source(e)),
        Err(e) => Err(destination_error(e)),
    }
}

/// Makes the error of a write: a full disk is told apart.
fn destination_error(e: io::Error) -> ImportError {
    if matches!(
        e.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    ) {
        ImportError::DiskFull
    } else {
        ImportError::Destination(e)
    }
}

/// Gives the file at `temp` the name `dest`, without replacing a file that is
/// there (`AlreadyExists` if there is one).
///
/// A hard link does that in one step on Linux and on Windows. `rename` cannot:
/// on Windows it replaces a file that is there. Where the file system has no
/// hard links, the name is looked at and `rename` is used; between the two a
/// file could appear, which is accepted for a program with one user. The
/// temporary file is removed by its guard afterwards.
fn place(temp: &Path, dest: &Path) -> io::Result<()> {
    match fs::hard_link(temp, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if fs::symlink_metadata(dest).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(temp, dest)
        }
    }
}

/// Returns whether the file `existing` has the same content as `source`
/// (which is `size` bytes): the sizes first, then the bytes. A file that
/// cannot be read is not the same.
fn same_content(source: &Path, size: u64, existing: &Path) -> Result<bool, ImportError> {
    let Ok(metadata) = fs::metadata(existing) else {
        return Ok(false);
    };
    if !metadata.is_file() || metadata.len() != size {
        return Ok(false);
    }
    let mut first = fs::File::open(source).map_err(ImportError::Source)?;
    let Ok(mut second) = fs::File::open(existing) else {
        return Ok(false);
    };
    match streams_equal(&mut first, &mut second, COMPARE_BUFFER_BYTES) {
        Ok(equal) => Ok(equal),
        Err(ReadFailure::First(e)) => Err(ImportError::Source(e)),
        Err(ReadFailure::Second) => Ok(false),
    }
}

/// Which of the two streams failed to read.
#[derive(Debug)]
enum ReadFailure {
    First(io::Error),
    Second,
}

/// Compares two streams from the start, `buffer_len` bytes at a time (two
/// buffers, whatever the length of the streams). Stops at the first byte that
/// differs.
fn streams_equal<A: Read, B: Read>(
    first: &mut A,
    second: &mut B,
    buffer_len: usize,
) -> Result<bool, ReadFailure> {
    let mut left = vec![0_u8; buffer_len];
    let mut right = vec![0_u8; buffer_len];
    loop {
        let n = read_full(first, &mut left).map_err(ReadFailure::First)?;
        let m = read_full(second, &mut right).map_err(|_| ReadFailure::Second)?;
        if n != m || left[..n] != right[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Reads until the buffer is full or the stream ends. Returns how many bytes
/// were read.
fn read_full<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::asset::{AssetFormat, AssetInfo};
    use crate::atomic_file::temp_path;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh folder under the system temp dir, unique per test.
    fn test_root(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ink-core-import-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    fn info(frames: Option<u64>) -> AssetInfo {
        AssetInfo {
            format: AssetFormat::Wav,
            sample_rate: 48000,
            channels: 2,
            frames,
        }
    }

    /// A `probe` that reads the first bytes: `GOOD...` is a WAV of its size,
    /// `DENY...` cannot be read, anything else is not a sound.
    fn probe_file(path: &Path) -> Result<AssetInfo, ProbeFailure> {
        let bytes = fs::read(path).map_err(|e| ProbeFailure::Unreadable(e.to_string()))?;
        if bytes.starts_with(b"GOOD") {
            Ok(info(Some(bytes.len() as u64)))
        } else if bytes.starts_with(b"DENY") {
            Err(ProbeFailure::Unreadable("denied".to_string()))
        } else {
            Err(ProbeFailure::NotAudio("not a sound".to_string()))
        }
    }

    /// A library in a fresh root, and a folder for the sources.
    fn setup(name: &str) -> (PathBuf, Library, PathBuf) {
        let root = test_root(name);
        let library = Library::open(&root).unwrap().library;
        let sources = test_root(&format!("{name}-sources"));
        (root, library, sources)
    }

    /// Makes a source file and returns its path.
    fn source(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn import(
        library: &mut Library,
        path: &Path,
        kind: AssetKind,
    ) -> Result<ImportOutcome, ImportError> {
        library.import(path, kind, &probe_file)
    }

    fn imported(outcome: Result<ImportOutcome, ImportError>) -> AssetId {
        match outcome {
            Ok(ImportOutcome::Imported(id)) => id,
            other => panic!("expected Imported, got {other:?}"),
        }
    }

    /// The names in a folder, sorted.
    fn names(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn index_path(root: &Path) -> PathBuf {
        root.join("library.toml")
    }

    /// Makes saving fail: a folder where the temporary file would be.
    fn block_saving(root: &Path) {
        fs::create_dir_all(temp_path(&index_path(root)).join("inner")).unwrap();
    }

    // -- the plain import -------------------------------------------------------

    #[test]
    fn a_file_is_copied_and_becomes_an_asset_and_the_source_is_untouched() {
        let (root, mut library, sources) = setup("plain");
        let path = source(&sources, "Song One.wav", b"GOOD the sound");
        let calls = std::cell::Cell::new(Vec::new());
        let probe = |p: &Path| {
            let mut seen = calls.take();
            seen.push(p.to_path_buf());
            calls.set(seen);
            probe_file(p)
        };

        let outcome = library.import(&path, AssetKind::Bgm, &probe).unwrap();
        assert_eq!(outcome, ImportOutcome::Imported(id("bgm-0001")));
        assert_eq!(
            calls.take(),
            std::slice::from_ref(&path),
            "the source is read once"
        );

        let copy = root.join("sounds/bgm/Song One.wav");
        assert_eq!(fs::read(&copy).unwrap(), b"GOOD the sound");
        assert_eq!(fs::read(&path).unwrap(), b"GOOD the sound");
        assert_eq!(names(&root.join("sounds/bgm")), ["Song One.wav"]);

        let asset = library.get(&id("bgm-0001")).unwrap();
        assert_eq!(asset.name(), "Song One");
        assert_eq!(asset.relative_path(), "Song One.wav");
        assert_eq!(asset.state(), AssetState::Available);
        assert_eq!(asset.info().unwrap().frames, Some(14));
        assert_eq!(library.path_of(&id("bgm-0001")).unwrap(), copy);
        assert_eq!(library.next_id, 2);

        // It is in the index, and a scan has nothing to add.
        let mut again = Library::open(&root).unwrap().library;
        assert_eq!(again.get(&id("bgm-0001")).unwrap().name(), "Song One");
        let report = again.scan(&probe_file).unwrap();
        assert!(report.is_empty(), "{report:?}");
    }

    #[test]
    fn japanese_names_and_spaces_are_kept() {
        let (root, mut library, sources) = setup("japanese");
        let path = source(&sources, "音楽 テスト 🎵.mp3", b"GOOD mp3");
        let id = imported(import(&mut library, &path, AssetKind::Bgm));
        assert_eq!(
            library.get(&id).unwrap().relative_path(),
            "音楽 テスト 🎵.mp3"
        );
        assert!(root.join("sounds/bgm/音楽 テスト 🎵.mp3").is_file());
        assert_eq!(library.get(&id).unwrap().name(), "音楽 テスト 🎵");
    }

    #[test]
    fn the_kind_decides_the_folder() {
        let (root, mut library, sources) = setup("kinds");
        let path = source(&sources, "clap.wav", b"GOOD clap");
        let bgm = imported(import(&mut library, &path, AssetKind::Bgm));
        let se = imported(import(&mut library, &path, AssetKind::Se));
        assert_eq!((bgm, se), (id("bgm-0001"), id("se-0002")));
        assert!(root.join("sounds/bgm/clap.wav").is_file());
        assert!(root.join("sounds/se/clap.wav").is_file());
    }

    #[test]
    fn a_name_is_taken_in_any_case_of_the_extension() {
        let (root, mut library, sources) = setup("upper-extension");
        let path = source(&sources, "LOUD.WAV", b"GOOD loud");
        imported(import(&mut library, &path, AssetKind::Se));
        assert!(
            root.join("sounds/se/LOUD.WAV").is_file(),
            "the case is kept"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_followed_and_the_target_is_copied() {
        let (root, mut library, sources) = setup("link-source");
        let target = source(&sources, "target.wav", b"GOOD target");
        let link = sources.join("link.wav");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        imported(import(&mut library, &link, AssetKind::Se));
        let copy = root.join("sounds/se/link.wav");
        assert!(!copy.symlink_metadata().unwrap().is_symlink());
        assert_eq!(fs::read(copy).unwrap(), b"GOOD target");
        assert!(link.symlink_metadata().unwrap().is_symlink());
    }

    // -- the same name --------------------------------------------------------------

    #[test]
    fn the_same_file_twice_is_there_already_and_is_not_copied_again() {
        let (root, mut library, sources) = setup("twice");
        let path = source(&sources, "x.wav", b"GOOD same");
        let first = imported(import(&mut library, &path, AssetKind::Se));

        let again = source(&test_root("twice-other"), "x.wav", b"GOOD same");
        let outcome = import(&mut library, &again, AssetKind::Se).unwrap();
        assert_eq!(outcome, ImportOutcome::AlreadyThere(first));
        assert_eq!(library.assets().len(), 1);
        assert_eq!(library.next_id, 2, "no ID was given");
        assert_eq!(names(&root.join("sounds/se")), ["x.wav"]);
    }

    #[test]
    fn the_same_name_with_other_content_gets_the_next_numbers_one_by_one() {
        let (root, mut library, sources) = setup("numbers");
        let a = source(&sources.join("a"), "x.wav", b"GOOD one");
        let b = source(&sources.join("b"), "x.wav", b"GOOD two");
        let c = source(&sources.join("c"), "x.wav", b"GOOD three");
        imported(import(&mut library, &a, AssetKind::Se));
        imported(import(&mut library, &b, AssetKind::Se));
        imported(import(&mut library, &c, AssetKind::Se));
        assert_eq!(
            names(&root.join("sounds/se")),
            ["x (2).wav", "x (3).wav", "x.wav"]
        );
        assert_eq!(fs::read(root.join("sounds/se/x.wav")).unwrap(), b"GOOD one");
        assert_eq!(
            fs::read(root.join("sounds/se/x (2).wav")).unwrap(),
            b"GOOD two"
        );
        assert_eq!(
            fs::read(root.join("sounds/se/x (3).wav")).unwrap(),
            b"GOOD three"
        );
        assert_eq!(library.get(&id("se-0002")).unwrap().name(), "x (2)");

        // A file that is the same as the second one is found at its number.
        let again = source(&sources.join("d"), "x.wav", b"GOOD two");
        assert_eq!(
            import(&mut library, &again, AssetKind::Se).unwrap(),
            ImportOutcome::AlreadyThere(id("se-0002"))
        );
    }

    #[test]
    fn files_of_the_same_size_are_told_apart_by_their_bytes() {
        let (root, mut library, sources) = setup("same-size");
        let a = source(&sources.join("a"), "x.wav", b"GOOD 0123456789");
        let b = source(&sources.join("b"), "x.wav", b"GOOD 0123456780");
        imported(import(&mut library, &a, AssetKind::Se));
        imported(import(&mut library, &b, AssetKind::Se));
        assert_eq!(names(&root.join("sounds/se")), ["x (2).wav", "x.wav"]);
    }

    #[test]
    fn a_name_is_taken_whatever_the_case_of_its_letters() {
        let (root, mut library, sources) = setup("case");
        let a = source(&sources.join("a"), "Song.wav", b"GOOD first");
        let first = imported(import(&mut library, &a, AssetKind::Se));

        // Other content: the next number, not a second "song.wav".
        let b = source(&sources.join("b"), "song.wav", b"GOOD second");
        imported(import(&mut library, &b, AssetKind::Se));
        assert_eq!(names(&root.join("sounds/se")), ["Song.wav", "song (2).wav"]);

        // The same content: it is the Asset that has the name in other case.
        let c = source(&sources.join("c"), "SONG.WAV", b"GOOD first");
        assert_eq!(
            import(&mut library, &c, AssetKind::Se).unwrap(),
            ImportOutcome::AlreadyThere(first)
        );
    }

    #[test]
    fn a_file_put_in_by_hand_keeps_its_name_taken_and_is_not_touched() {
        let (root, mut library, sources) = setup("by-hand");
        let by_hand = root.join("sounds/se/Hand.wav");
        fs::write(&by_hand, b"GOOD by hand").unwrap();
        // The same name in other case and the very same content: it is not an
        // Asset, so it only takes the name.
        let path = source(&sources, "hand.wav", b"GOOD by hand");
        imported(import(&mut library, &path, AssetKind::Se));
        assert_eq!(names(&root.join("sounds/se")), ["Hand.wav", "hand (2).wav"]);
        assert_eq!(fs::read(by_hand).unwrap(), b"GOOD by hand");
    }

    #[test]
    fn an_asset_whose_file_is_gone_keeps_its_name_taken() {
        let (root, mut library, sources) = setup("gone");
        let path = source(&sources, "x.wav", b"GOOD content");
        imported(import(&mut library, &path, AssetKind::Se));
        fs::remove_file(root.join("sounds/se/x.wav")).unwrap();
        let outcome = import(&mut library, &path, AssetKind::Se);
        imported(outcome);
        assert_eq!(names(&root.join("sounds/se")), ["x (2).wav"]);
        assert_eq!(library.assets().len(), 2);
    }

    #[test]
    fn a_different_name_with_the_same_content_is_another_asset() {
        let (_root, mut library, sources) = setup("other-name");
        let a = source(&sources, "a.wav", b"GOOD same");
        let b = source(&sources, "b.wav", b"GOOD same");
        let first = imported(import(&mut library, &a, AssetKind::Se));
        let second = imported(import(&mut library, &b, AssetKind::Se));
        assert_ne!(first, second);
        assert_eq!(library.assets().len(), 2);
    }

    #[test]
    fn when_the_numbers_are_used_up_the_import_is_refused() {
        let (root, mut library, sources) = setup("too-many");
        let folder = root.join("sounds/se");
        fs::write(folder.join("x.wav"), b"taken").unwrap();
        for n in 2..=MAX_NUMBER {
            fs::write(folder.join(format!("x ({n}).wav")), b"taken").unwrap();
        }
        let before = names(&folder);
        let path = source(&sources, "x.wav", b"GOOD new");
        let error = import(&mut library, &path, AssetKind::Se).unwrap_err();
        assert!(matches!(error, ImportError::TooManyNames), "{error}");
        assert_eq!(names(&folder), before, "no file, no temporary file");
        assert_eq!(library.next_id, 1);
    }

    // -- the names of the copies ----------------------------------------------------

    #[test]
    fn names_that_windows_cannot_have_are_made_safe_and_the_scan_accepts_them() {
        let (root, mut library, sources) = setup("safe-names");
        let cases = [
            ("CON.wav", "_CON.wav"),
            ("nul.mp3", "_nul.mp3"),
            ("com1.wav", "_com1.wav"),
            ("con.v2.wav", "_con.v2.wav"),
            ("a?b*c.wav", "a_b_c.wav"),
            ("say \"hi\" <3 | ok.wav", "say _hi_ _3 _ ok.wav"),
            (".hidden.wav", "_hidden.wav"),
            ("..two.wav", "__two.wav"),
            (".wav", "_.wav"),
            ("trail .wav", "trail_.wav"),
            ("dots...wav", "dots__.wav"),
            ("tab\there.wav", "tab_here.wav"),
        ];
        for (original, expected) in cases {
            let path = source(&sources, original, b"GOOD x");
            let id = imported(import(&mut library, &path, AssetKind::Se));
            assert_eq!(
                library.get(&id).unwrap().relative_path(),
                expected,
                "{original}"
            );
            assert!(
                root.join("sounds/se").join(expected).is_file(),
                "{expected}"
            );
        }
        // The names are the ones that the library accepts for a file.
        let mut again = Library::open(&root).unwrap().library;
        assert_eq!(again.assets().len(), cases.len());
        let report = again.scan(&probe_file).unwrap();
        assert!(report.is_empty(), "{report:?}");
    }

    #[test]
    fn the_same_unsafe_name_twice_is_the_same_asset() {
        let (_root, mut library, sources) = setup("unsafe-twice");
        let a = source(&sources.join("a"), "CON.wav", b"GOOD con");
        let b = source(&sources.join("b"), "CON.wav", b"GOOD con");
        let first = imported(import(&mut library, &a, AssetKind::Se));
        assert_eq!(
            import(&mut library, &b, AssetKind::Se).unwrap(),
            ImportOutcome::AlreadyThere(first)
        );
    }

    #[test]
    fn a_long_name_is_cut_at_the_border_of_a_character() {
        let (root, mut library, sources) = setup("long-name");
        // The longest name the file system takes: 83 characters of 3 bytes,
        // two more, and the extension: 255 bytes. It is kept as it is.
        let long = format!("{}ab.wav", "あ".repeat(83));
        assert_eq!(long.len(), MAX_NAME_BYTES);
        let path = source(&sources.join("a"), &long, b"GOOD one");
        let first = imported(import(&mut library, &path, AssetKind::Se));
        assert_eq!(library.get(&first).unwrap().relative_path(), long);

        // With " (2)" the name does not fit: the part before the extension is
        // cut, at the border of a character (not in the middle of one).
        let other = source(&sources.join("b"), &long, b"GOOD two");
        let second = imported(import(&mut library, &other, AssetKind::Se));
        let second_name = library.get(&second).unwrap().relative_path().to_string();
        assert_eq!(
            second_name,
            format!("{} (2).wav", "あ".repeat(82)),
            "247 bytes are left for the part before ` (2).wav`, 82 characters"
        );
        assert!(second_name.len() <= MAX_NAME_BYTES);
        assert!(root.join("sounds/se").join(&second_name).is_file());
    }

    #[test]
    fn a_name_of_exactly_255_bytes_is_not_cut() {
        let (_root, mut library, sources) = setup("exact-name");
        let exact = format!("{}.wav", "a".repeat(251));
        assert_eq!(exact.len(), 255);
        let path = source(&sources, &exact, b"GOOD x");
        let id = imported(import(&mut library, &path, AssetKind::Se));
        assert_eq!(library.get(&id).unwrap().relative_path(), exact);
    }

    #[test]
    fn the_names_that_are_made_always_pass_the_rule_of_the_library() {
        let stems = [
            "", ".", "..", " ", "a ", "a.", "CON", "con.", "LPT9", "x:y", "a/b", "\u{0}", "あ ",
        ];
        for stem in stems {
            for extension in ["wav", "MP3"] {
                let cleaned = clean_stem(stem, extension);
                for number in [1, 2, 10, MAX_NUMBER] {
                    let name = candidate_name(&cleaned, number, extension);
                    assert_eq!(path_problem(&name), None, "{stem:?} -> {name:?}");
                    assert!(name.len() <= MAX_NAME_BYTES);
                }
            }
        }
        let huge = "é".repeat(500);
        let name = candidate_name(&clean_stem(&huge, "wav"), MAX_NUMBER, "wav");
        assert!(name.len() <= MAX_NAME_BYTES);
        assert!(name.ends_with(" (1000).wav"));
    }

    // -- what is refused ------------------------------------------------------------

    #[test]
    fn other_formats_are_refused_before_anything_is_read_or_made() {
        let (root, mut library, sources) = setup("formats");
        let probed = std::cell::Cell::new(false);
        let probe = |_: &Path| {
            probed.set(true);
            Ok(info(Some(1)))
        };
        for (name, expected) in [
            ("notes.txt", Some("txt")),
            ("song.flac", Some("flac")),
            ("no-extension", None),
            ("trailing.", Some("")),
        ] {
            let path = source(&sources, name, b"GOOD x");
            let error = library.import(&path, AssetKind::Se, &probe).unwrap_err();
            assert!(
                matches!(&error, ImportError::UnsupportedFormat { extension }
                    if extension.as_deref() == expected),
                "{name}: {error}"
            );
        }
        assert!(!probed.get());
        assert!(names(&root.join("sounds/se")).is_empty());
        assert_eq!(library.next_id, 1);
    }

    #[test]
    fn a_file_that_is_not_a_sound_is_refused_with_the_reason() {
        let (root, mut library, sources) = setup("not-audio");
        let bad = source(&sources, "bad.wav", b"BAD");
        let error = import(&mut library, &bad, AssetKind::Se).unwrap_err();
        assert!(matches!(&error, ImportError::NotAudio(r) if r == "not a sound"));
        let deny = source(&sources, "deny.wav", b"DENY");
        let error = import(&mut library, &deny, AssetKind::Se).unwrap_err();
        assert!(matches!(&error, ImportError::Unreadable(r) if r == "denied"));
        assert!(names(&root.join("sounds/se")).is_empty());
        assert_eq!(library.next_id, 1);
    }

    #[test]
    fn a_sound_effect_over_60_seconds_is_refused_and_a_bgm_is_not() {
        let (root, mut library, sources) = setup("too-long");
        let path = source(&sources, "long.wav", b"GOOD x");
        let seconds = |s: u64| move |_: &Path| Ok(info(Some(48000 * s)));

        let error = library
            .import(&path, AssetKind::Se, &seconds(61))
            .unwrap_err();
        match error {
            ImportError::TooLongForSe { limit, length } => {
                assert_eq!(limit, SE_MAX_DURATION);
                assert_eq!(length, Duration::from_secs(61));
            }
            other => panic!("{other}"),
        }
        assert!(names(&root.join("sounds/se")).is_empty());
        assert_eq!(library.next_id, 1);

        // Exactly 60 seconds is allowed, and so is a BGM of any length.
        imported(library.import(&path, AssetKind::Se, &seconds(60)));
        imported(library.import(&path, AssetKind::Bgm, &seconds(3600)));
    }

    #[test]
    fn a_length_that_the_file_does_not_say_is_let_through() {
        let (_root, mut library, sources) = setup("no-length");
        let path = source(&sources, "unknown.mp3", b"GOOD x");
        let unknown = |_: &Path| Ok(info(None));
        imported(library.import(&path, AssetKind::Se, &unknown));
    }

    #[test]
    fn a_folder_and_a_file_that_is_not_there_are_refused() {
        let (_root, mut library, sources) = setup("not-a-file");
        let folder = sources.join("folder.wav");
        fs::create_dir_all(&folder).unwrap();
        let error = import(&mut library, &folder, AssetKind::Se).unwrap_err();
        assert!(matches!(error, ImportError::NotAFile), "{error}");

        let missing = sources.join("missing.wav");
        let error = import(&mut library, &missing, AssetKind::Se).unwrap_err();
        assert!(
            matches!(&error, ImportError::Source(e) if e.kind() == io::ErrorKind::NotFound),
            "{error}"
        );
    }

    #[test]
    fn a_file_that_is_in_the_sound_folders_is_not_copied() {
        let (root, mut library, _sources) = setup("inside");
        let inside = source(&root.join("sounds/bgm"), "album/in.wav", b"GOOD x");
        let error = import(&mut library, &inside, AssetKind::Se).unwrap_err();
        assert!(matches!(error, ImportError::AlreadyInLibrary), "{error}");
        // Also through `..` and for the other kind.
        let roundabout = root.join("sounds/se/../bgm/album/in.wav");
        let error = import(&mut library, &roundabout, AssetKind::Bgm).unwrap_err();
        assert!(matches!(error, ImportError::AlreadyInLibrary), "{error}");
        assert_eq!(names(&root.join("sounds/se")), Vec::<String>::new());
        assert_eq!(library.next_id, 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_from_outside_to_a_file_inside_is_in_the_library() {
        let (root, mut library, sources) = setup("link-inside");
        let inside = source(&root.join("sounds/bgm"), "in.wav", b"GOOD x");
        let link = sources.join("link.wav");
        std::os::unix::fs::symlink(&inside, &link).unwrap();
        let error = import(&mut library, &link, AssetKind::Bgm).unwrap_err();
        assert!(matches!(error, ImportError::AlreadyInLibrary), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let (_root, mut library, sources) = setup("non-utf8");
        let path = sources.join(OsStr::from_bytes(b"bad\xff.wav"));
        fs::write(&path, b"GOOD x").unwrap();
        let error = import(&mut library, &path, AssetKind::Se).unwrap_err();
        assert!(matches!(error, ImportError::NameNotUtf8), "{error}");
    }

    // -- failures leave nothing -----------------------------------------------------

    #[test]
    fn when_the_index_cannot_be_saved_the_copy_is_removed_and_no_id_is_given() {
        let (root, mut library, sources) = setup("save-fails");
        let path = source(&sources, "x.wav", b"GOOD content");
        let folder = root.join("sounds/se");
        block_saving(&root);

        let error = import(&mut library, &path, AssetKind::Se).unwrap_err();
        assert!(
            matches!(&error, ImportError::Library(LibraryError::Io { .. })),
            "{error}"
        );
        assert!(names(&folder).is_empty(), "{:?}", names(&folder));
        assert!(library.assets().is_empty());
        assert_eq!(library.next_id, 1);
        assert_eq!(fs::read(&path).unwrap(), b"GOOD content");

        // The next import is given the ID that the failed one did not keep.
        fs::remove_dir_all(temp_path(&index_path(&root))).unwrap();
        let id = imported(import(&mut library, &path, AssetKind::Se));
        assert_eq!(id, self::id("se-0001"));
        assert_eq!(names(&folder), ["x.wav"]);
    }

    #[test]
    fn when_the_numbers_have_run_out_nothing_is_copied() {
        let (root, mut library, sources) = setup("out-of-ids");
        let path = source(&sources, "x.wav", b"GOOD x");
        library.next_id = u64::MAX;
        let error = import(&mut library, &path, AssetKind::Se).unwrap_err();
        assert!(
            matches!(error, ImportError::Library(LibraryError::OutOfIds)),
            "{error}"
        );
        assert!(names(&root.join("sounds/se")).is_empty());
    }

    #[test]
    fn a_temporary_file_that_a_crash_left_is_not_touched_and_not_used() {
        let (root, mut library, sources) = setup("stale-temp");
        let folder = root.join("sounds/se");
        fs::write(folder.join(".import-1.tmp"), b"half of an old copy").unwrap();
        let path = source(&sources, "x.wav", b"GOOD new");
        imported(import(&mut library, &path, AssetKind::Se));
        assert_eq!(names(&folder), [".import-1.tmp", "x.wav"]);
        assert_eq!(
            fs::read(folder.join(".import-1.tmp")).unwrap(),
            b"half of an old copy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_source_that_cannot_be_read_is_a_source_error_and_leaves_nothing() {
        use std::os::unix::fs::PermissionsExt as _;

        let (root, mut library, sources) = setup("source-denied");
        let path = source(&sources, "locked.wav", b"GOOD x");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = fs::File::open(&path).is_ok();
        let always_ok = |_: &Path| Ok(info(Some(1)));
        let result = library.import(&path, AssetKind::Se, &always_ok);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        // A user who can read everything (root) is not stopped by the mode.
        if !readable_anyway {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, ImportError::Source(e)
                    if e.kind() == io::ErrorKind::PermissionDenied),
                "{error}"
            );
            assert!(names(&root.join("sounds/se")).is_empty());
            assert_eq!(library.next_id, 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_written_is_a_destination_error_and_leaves_nothing() {
        use std::os::unix::fs::PermissionsExt as _;

        let (root, mut library, sources) = setup("dest-denied");
        let path = source(&sources, "x.wav", b"GOOD x");
        let folder = root.join("sounds/se");
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).unwrap();
        let writable_anyway = fs::write(folder.join(".probe"), b"x").is_ok();
        let result = import(&mut library, &path, AssetKind::Se);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        if writable_anyway {
            let _ = fs::remove_file(folder.join(".probe"));
        } else {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, ImportError::Destination(e)
                    if e.kind() == io::ErrorKind::PermissionDenied),
                "{error}"
            );
            assert!(names(&folder).is_empty());
            assert_eq!(fs::read(&path).unwrap(), b"GOOD x");
        }
    }

    #[test]
    fn a_failure_of_the_source_and_one_of_the_destination_are_told_apart() {
        struct Failing(io::ErrorKind);
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(self.0))
            }
        }
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(self.0))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut sink = Vec::new();
        let error =
            copy_stream(&mut Failing(io::ErrorKind::PermissionDenied), &mut sink).unwrap_err();
        assert!(
            matches!(&error, ImportError::Source(e) if e.kind() == io::ErrorKind::PermissionDenied)
        );

        let mut data: &[u8] = b"data";
        let error =
            copy_stream(&mut data, &mut Failing(io::ErrorKind::PermissionDenied)).unwrap_err();
        assert!(
            matches!(&error, ImportError::Destination(e) if e.kind() == io::ErrorKind::PermissionDenied)
        );

        for kind in [io::ErrorKind::StorageFull, io::ErrorKind::QuotaExceeded] {
            let mut data: &[u8] = b"data";
            let error = copy_stream(&mut data, &mut Failing(kind)).unwrap_err();
            assert!(matches!(error, ImportError::DiskFull), "{kind:?}");
        }

        // The whole stream arrives.
        let mut data: &[u8] = b"all of it";
        let mut out = Vec::new();
        copy_stream(&mut data, &mut out).unwrap();
        assert_eq!(out, b"all of it");
    }

    #[test]
    fn a_temporary_file_is_removed_when_its_guard_is_dropped() {
        let dir = test_root("guard");
        let (file, guard) = create_temp(&dir).unwrap();
        drop(file);
        assert_eq!(names(&dir), [".import-1.tmp"]);
        let (_second, second_guard) = create_temp(&dir).unwrap();
        assert_eq!(names(&dir), [".import-1.tmp", ".import-2.tmp"]);
        drop(guard);
        assert_eq!(names(&dir), [".import-2.tmp"]);
        drop(second_guard);
        assert!(names(&dir).is_empty());
    }

    #[test]
    fn a_name_is_given_without_replacing_a_file_that_is_there() {
        let dir = test_root("place");
        let temp = source(&dir, ".import-1.tmp", b"new");
        let dest = source(&dir, "x.wav", b"old");
        let error = place(&temp, &dest).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&dest).unwrap(), b"old");
        assert_eq!(fs::read(&temp).unwrap(), b"new");

        let free = dir.join("y.wav");
        place(&temp, &free).unwrap();
        assert_eq!(fs::read(&free).unwrap(), b"new");
    }

    // -- the comparison of the content ---------------------------------------------

    #[test]
    fn streams_are_compared_across_the_borders_of_the_buffer() {
        let data: Vec<u8> = (0..1000_u32).map(|n| (n % 251) as u8).collect();
        for buffer in [1, 2, 3, 7, 64, 999, 1000, 1001, 4096] {
            let mut a = data.as_slice();
            let mut b = data.as_slice();
            assert!(streams_equal(&mut a, &mut b, buffer).unwrap(), "{buffer}");

            let mut changed = data.clone();
            *changed.last_mut().unwrap() ^= 1;
            let mut a = data.as_slice();
            let mut b = changed.as_slice();
            assert!(!streams_equal(&mut a, &mut b, buffer).unwrap(), "{buffer}");

            let mut shorter = &data[..999];
            let mut a = data.as_slice();
            assert!(!streams_equal(&mut a, &mut shorter, buffer).unwrap());
        }
        let (mut empty_a, mut empty_b): (&[u8], &[u8]) = (&[], &[]);
        assert!(streams_equal(&mut empty_a, &mut empty_b, 8).unwrap());
    }

    #[test]
    fn a_reader_that_gives_a_few_bytes_at_a_time_is_compared_right() {
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let n = self.0.len().min(buffer.len()).min(3);
                buffer[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let data = b"a stream that arrives in small pieces";
        let mut whole: &[u8] = data;
        assert!(streams_equal(&mut Trickle(data), &mut whole, 16).unwrap());
    }

    #[test]
    fn a_stream_of_a_gigabyte_is_compared_with_two_small_buffers() {
        // `repeat(..).take(..)` makes the bytes without keeping them: the
        // only memory that grows with the length would be a mistake of the
        // comparison, and it has two buffers of a fixed size.
        let gigabyte = 1024 * 1024 * 1024_u64;
        let mut a = io::repeat(7).take(gigabyte);
        let mut b = io::repeat(7).take(gigabyte);
        assert!(streams_equal(&mut a, &mut b, COMPARE_BUFFER_BYTES).unwrap());

        let mut a = io::repeat(7).take(gigabyte);
        let mut b = io::repeat(7)
            .take(gigabyte - 1)
            .chain(io::repeat(8).take(1));
        assert!(!streams_equal(&mut a, &mut b, COMPARE_BUFFER_BYTES).unwrap());
    }

    #[test]
    fn a_file_of_tens_of_megabytes_is_copied_and_found_again() {
        let (root, mut library, sources) = setup("big");
        let path = sources.join("big.wav");
        let file = fs::File::create(&path).unwrap();
        file.set_len(48 * 1024 * 1024).unwrap();
        let mut file = file;
        io::Write::write_all(&mut file, b"GOOD").unwrap();
        drop(file);

        let first = imported(import(&mut library, &path, AssetKind::Bgm));
        let copy = root.join("sounds/bgm/big.wav");
        assert_eq!(fs::metadata(&copy).unwrap().len(), 48 * 1024 * 1024);
        assert_eq!(
            import(&mut library, &path, AssetKind::Bgm).unwrap(),
            ImportOutcome::AlreadyThere(first)
        );
        assert_eq!(names(&root.join("sounds/bgm")), ["big.wav"]);
    }

    // -- the errors, the threads ----------------------------------------------------

    #[test]
    fn an_error_says_what_is_wrong_in_a_line() {
        let lines = [
            ImportError::NotAFile.to_string(),
            ImportError::AlreadyInLibrary.to_string(),
            ImportError::UnsupportedFormat {
                extension: Some("flac".to_string()),
            }
            .to_string(),
            ImportError::UnsupportedFormat { extension: None }.to_string(),
            ImportError::TooLongForSe {
                limit: Duration::from_secs(60),
                length: Duration::from_millis(61_500),
            }
            .to_string(),
            ImportError::DiskFull.to_string(),
            ImportError::TooManyNames.to_string(),
            ImportError::Library(LibraryError::OutOfIds).to_string(),
        ];
        for line in &lines {
            assert!(!line.contains('\n') && !line.is_empty(), "{line}");
        }
        assert!(lines[2].contains(".flac"));
        assert_eq!(
            lines[4],
            "a sound effect can be 60 seconds at most, this one is 61.5 seconds"
        );
        assert!(lines[6].contains("(1000)"));
        let source_error = ImportError::Source(io::Error::from(io::ErrorKind::NotFound));
        assert!(std::error::Error::source(&source_error).is_some());
        assert!(std::error::Error::source(&ImportError::DiskFull).is_none());
    }

    #[test]
    fn an_import_can_run_on_a_worker() {
        fn assert_send<T: Send>() {}
        assert_send::<ImportError>();
        assert_send::<ImportOutcome>();

        let (_root, library, sources) = setup("threads");
        let path = source(&sources, "x.wav", b"GOOD x");
        let worker = std::thread::spawn(move || {
            let mut library = library;
            let outcome = library.import(&path, AssetKind::Se, &probe_file);
            (library, outcome)
        });
        let (library, outcome) = worker.join().unwrap();
        assert_eq!(outcome.unwrap(), ImportOutcome::Imported(id("se-0001")));
        assert_eq!(library.assets().len(), 1);
    }
}
