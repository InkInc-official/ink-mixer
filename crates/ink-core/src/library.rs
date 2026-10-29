//! The sound library (ADR-0010): the folders of Ink Mixer for BGM and sound
//! effects (SE), and the index that is the ledger of what is in them.
//!
//! ```text
//! <root>/library.toml      the index (outside the sound folders)
//! <root>/sounds/bgm/       the songs
//! <root>/sounds/se/        the sound effects
//! ```
//!
//! [`Library::open`] reads the index. Putting a file in (import), finding the
//! files that were put in by hand (scan) and the other changes are in
//! `library_import.rs` and `library_scan.rs`; they all go through the rules
//! written here:
//!
//! - **A change is made on a copy, saved, and only then taken.** When the
//!   save fails, the library is as it was: nothing in memory is ahead of the
//!   disk. An ID that was given is always an ID that is on the disk, so a
//!   number is never given twice.
//! - **A broken or too new index is never overwritten.** [`Library::open`]
//!   fails and does not touch the file (nor make the folders).
//!   [`Library::reset_index`] moves it aside when the user asks for it.
//!
//! Everything here does file I/O: call it from a control or worker thread,
//! never from the realtime path. A `Library` is `Send`, so it can be moved to a
//! worker; it is changed through `&mut self`. Two processes that save at the
//! same time do not lock each other: the later save wins.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::asset::{Asset, AssetId, AssetKind, AssetState};
use crate::atomic_file::write_atomically;
use crate::library_index::{self, IndexError, IndexWarning};

/// The name of the index file in the root.
const INDEX_FILE: &str = "library.toml";
/// The folder of the sound folders in the root.
const SOUNDS_DIR: &str = "sounds";
/// The most names tried for the backup of a broken index.
const MAX_BACKUPS: usize = 1000;

/// An error of the library.
///
/// The text of an error is one line in English, for the log and the CLI. The
/// window chooses its own (Japanese) words from the variant.
#[derive(Debug)]
#[non_exhaustive]
pub enum LibraryError {
    /// A file or folder could not be read, made, moved or written. `source`
    /// has the kind (`PermissionDenied`, `StorageFull`, ...).
    Io { path: PathBuf, source: io::Error },
    /// The index cannot be read or written. When this is the error of
    /// [`Library::open`], the file was not touched.
    Index(IndexError),
    /// There is no Asset with this ID.
    UnknownAsset(AssetId),
    /// The Asset is not missing: only an Asset whose file is missing can be
    /// forgotten.
    NotMissing(AssetId),
    /// The numbers of the IDs have run out (`next_id` is `u64::MAX`).
    OutOfIds,
    /// [`Library::reset_index`] was asked to move aside an index that can be
    /// read, or that does not exist.
    IndexNotBroken,
    /// [`Library::reset_index`] found no free name for the backup.
    NoBackupName,
}

impl LibraryError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Index(e) => e.fmt(f),
            Self::UnknownAsset(id) => write!(f, "{id} is not in the library"),
            Self::NotMissing(id) => {
                write!(f, "{id} can only be forgotten when its file is missing")
            }
            Self::OutOfIds => f.write_str("no more IDs can be made"),
            Self::IndexNotBroken => f.write_str(
                "the library index can be read (or does not exist), so it is not moved aside",
            ),
            Self::NoBackupName => write!(
                f,
                "no free name for the backup of the library index \
                 ({INDEX_FILE}.broken to {INDEX_FILE}.broken-{MAX_BACKUPS} are all taken)"
            ),
        }
    }
}

impl std::error::Error for LibraryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Index(e) => Some(e),
            _ => None,
        }
    }
}

impl From<IndexError> for LibraryError {
    fn from(e: IndexError) -> Self {
        Self::Index(e)
    }
}

/// What [`Library::open`] gives.
#[derive(Debug)]
#[must_use]
pub struct Opened {
    /// The library.
    pub library: Library,
    /// Why entries of the index were left out, or values were changed
    /// (see [`IndexWarning`]). Show them to the user: **an entry that was
    /// left out is gone from the index at the next save** (the library saves
    /// what it has, and it does not keep a copy of the file it read). A
    /// mistake in a file that was edited by hand is only told here.
    pub warnings: Vec<IndexWarning>,
}

/// The sound library: the Assets of the sound folders.
///
/// See the module documentation for how it changes and saves.
#[derive(Debug)]
pub struct Library {
    /// The root, an absolute path.
    pub(crate) root: PathBuf,
    /// The number of the next ID. It is larger than every number that was
    /// given.
    pub(crate) next_id: u64,
    /// The Assets, in the order of the index.
    pub(crate) assets: Vec<Asset>,
}

/// What was found when the index file was read.
enum IndexFile {
    Missing,
    Text(String),
    NotUtf8,
}

impl Library {
    /// Opens the library in `root`. `root` is made into an absolute path
    /// (symbolic links are not resolved).
    ///
    /// The index is read first. If there is none, the library starts empty
    /// (nothing is written until the first change). If it cannot be used,
    /// this fails and **touches nothing**: the file is not overwritten and the
    /// folders are not made. Otherwise the folders are made (if they are not
    /// there), and the state of each Asset is found from its file, without
    /// reading it (a scan does that):
    ///
    /// - the file is there and was read once: [`AssetState::Available`]
    /// - the file is there and was never read: [`AssetState::Unreadable`]
    /// - the file is not there: [`AssetState::Missing`]
    ///
    /// A file that could be read when it was scanned and cannot be read now
    /// shows as `Available` until the next scan (the state is not saved).
    ///
    /// # Errors
    ///
    /// - [`LibraryError::Index`] if the index is not TOML, has no `version`,
    ///   or has a version that this build does not know
    /// - [`LibraryError::Io`] if the index cannot be read (for example, no
    ///   permission), or the folders cannot be made
    pub fn open(root: &Path) -> Result<Opened, LibraryError> {
        let root = absolute(root)?;
        let index_path = root.join(INDEX_FILE);

        let parsed = match read_index_file(&index_path)? {
            IndexFile::Missing => None,
            IndexFile::Text(text) => Some(library_index::parse(&text)?),
            IndexFile::NotUtf8 => return Err(not_utf8()),
        };

        for kind in AssetKind::ALL {
            let folder = folder_of(&root, kind);
            fs::create_dir_all(&folder).map_err(|e| LibraryError::io(&folder, e))?;
        }

        let Some(parsed) = parsed else {
            return Ok(Opened {
                library: Self {
                    root,
                    next_id: 1,
                    assets: Vec::new(),
                },
                warnings: Vec::new(),
            });
        };
        let assets = parsed
            .entries
            .into_iter()
            .map(|entry| {
                let path = join_relative(folder_of(&root, entry.id.kind()), &entry.path);
                let state = state_at(&path, entry.info.is_some());
                let mut asset = Asset::new(entry.id, entry.name, entry.path, entry.info, state);
                asset.set_trigger_words(entry.trigger_words);
                asset
            })
            .collect();
        Ok(Opened {
            library: Self {
                root,
                next_id: parsed.next_id,
                assets,
            },
            warnings: parsed.warnings,
        })
    }

    /// Moves a broken index aside, so that [`open`](Self::open) can start
    /// with an empty one. For the user to ask for, after the window has said
    /// that the index cannot be used. The IDs of the old index are lost (and
    /// the names and trigger words): the file is kept as the backup, and a new
    /// scan gives new IDs.
    ///
    /// The backup is `library.toml.broken` in the root. If that name is taken
    /// it is `library.toml.broken-2`, then `-3`, and so on up to `-1000`:
    /// **an old backup is never overwritten**. (Between looking for a free
    /// name and moving the file there is a moment in which another process
    /// could take the name; for a program with one user, that is accepted.)
    /// The folders are not made. An index that is too new is moved aside too;
    /// the backup keeps it for the Ink Mixer that wrote it.
    ///
    /// Returns where the index went.
    ///
    /// # Errors
    ///
    /// - [`LibraryError::IndexNotBroken`] if the index can be read (also with
    ///   warnings), or does not exist. Nothing is moved.
    /// - [`LibraryError::NoBackupName`] if the 1,000 names are taken
    /// - [`LibraryError::Io`] if the index cannot be read or moved
    pub fn reset_index(root: &Path) -> Result<PathBuf, LibraryError> {
        let root = absolute(root)?;
        let index_path = root.join(INDEX_FILE);

        match read_index_file(&index_path)? {
            IndexFile::Missing => return Err(LibraryError::IndexNotBroken),
            IndexFile::Text(text) => {
                if library_index::parse(&text).is_ok() {
                    return Err(LibraryError::IndexNotBroken);
                }
            }
            // A file that is not text is broken.
            IndexFile::NotUtf8 => {}
        }

        for number in 1..=MAX_BACKUPS {
            let name = if number == 1 {
                format!("{INDEX_FILE}.broken")
            } else {
                format!("{INDEX_FILE}.broken-{number}")
            };
            let backup = root.join(name);
            match fs::symlink_metadata(&backup) {
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(LibraryError::io(&backup, e)),
            }
            fs::rename(&index_path, &backup).map_err(|e| LibraryError::io(&index_path, e))?;
            return Ok(backup);
        }
        Err(LibraryError::NoBackupName)
    }

    /// Returns the root: an absolute path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the folder of `kind` (`<root>/sounds/bgm` or `.../se`), for
    /// "open the folder".
    pub fn folder(&self, kind: AssetKind) -> PathBuf {
        folder_of(&self.root, kind)
    }

    /// Returns the Assets, in the order of the index.
    pub fn assets(&self) -> &[Asset] {
        &self.assets
    }

    /// Returns the Asset with this ID.
    pub fn get(&self, id: &AssetId) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.id() == id)
    }

    /// Returns the path of the file of this Asset, made now from the root and
    /// the relative path. It is never saved. It does not say that the file is
    /// there: see [`Asset::state`].
    ///
    /// The relative path was checked when it was read or found (it has no
    /// `..`, is not absolute and has no `\` or `:`), so the path stays inside
    /// the folder of the kind, on Windows and on Linux.
    pub fn path_of(&self, id: &AssetId) -> Option<PathBuf> {
        let asset = self.get(id)?;
        debug_assert!(library_index::path_problem(asset.relative_path()).is_none());
        Some(join_relative(
            self.folder(asset.kind()),
            asset.relative_path(),
        ))
    }

    /// Sets the trigger words of an Asset and saves. The words are kept as
    /// they are: their rules (katakana, no duplicates) are checked by whoever
    /// calls this. The same words as it has: nothing is written.
    ///
    /// # Errors
    ///
    /// - [`LibraryError::UnknownAsset`] if there is no such Asset
    /// - [`LibraryError::Io`] or [`LibraryError::Index`] if saving fails; the
    ///   Asset then has the words it had
    pub fn set_trigger_words(
        &mut self,
        id: &AssetId,
        words: Vec<String>,
    ) -> Result<(), LibraryError> {
        let position = self.position(id)?;
        if self.assets[position].trigger_words() == words.as_slice() {
            return Ok(());
        }
        let mut updated = self.assets.clone();
        updated[position].set_trigger_words(words);
        self.save(&updated, self.next_id)?;
        self.assets = updated;
        Ok(())
    }

    /// Takes an Asset whose file is missing out of the library and saves. The
    /// number of its ID is not given again.
    ///
    /// Only an Asset that is missing can be forgotten. To forget one whose file
    /// is there, the file has to go first (and the next scan then finds
    /// it missing), or it comes back at the next scan with a new ID.
    ///
    /// # Errors
    ///
    /// - [`LibraryError::UnknownAsset`] if there is no such Asset
    /// - [`LibraryError::NotMissing`] if its state is not
    ///   [`AssetState::Missing`]; nothing is changed
    /// - [`LibraryError::Io`] or [`LibraryError::Index`] if saving fails; the
    ///   Asset is then still in the library
    pub fn forget_missing(&mut self, id: &AssetId) -> Result<(), LibraryError> {
        let position = self.position(id)?;
        if self.assets[position].state() != AssetState::Missing {
            return Err(LibraryError::NotMissing(*id));
        }
        let mut updated = self.assets.clone();
        updated.remove(position);
        self.save(&updated, self.next_id)?;
        self.assets = updated;
        Ok(())
    }

    fn position(&self, id: &AssetId) -> Result<usize, LibraryError> {
        self.assets
            .iter()
            .position(|asset| asset.id() == id)
            .ok_or(LibraryError::UnknownAsset(*id))
    }

    /// Returns the index file.
    pub(crate) fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    /// Writes `assets` and `next_id` as the index, whole and in one step (the
    /// old file stays if it fails). Nothing of `self` is changed: the caller
    /// takes the new values when this has succeeded.
    pub(crate) fn save(&self, assets: &[Asset], next_id: u64) -> Result<(), LibraryError> {
        let text = library_index::to_toml(next_id, assets)?;
        let path = self.index_path();
        write_atomically(&path, text.as_bytes()).map_err(|e| LibraryError::io(&path, e))
    }
}

/// Gives the next ID of `kind` and moves `next_id` on.
///
/// # Errors
///
/// [`LibraryError::OutOfIds`] if the numbers have run out: `next_id` is
/// `u64::MAX`, which cannot be given because the one after it does not exist.
pub(crate) fn issue_id(kind: AssetKind, next_id: &mut u64) -> Result<AssetId, LibraryError> {
    if *next_id == u64::MAX {
        return Err(LibraryError::OutOfIds);
    }
    let id = AssetId::new(kind, *next_id).map_err(|_| LibraryError::OutOfIds)?;
    *next_id += 1;
    Ok(id)
}

/// Returns the state of the file of an Asset at `path`, from the file system
/// only: `has_info` is whether the file was ever read.
pub(crate) fn state_at(path: &Path, has_info: bool) -> AssetState {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            if has_info {
                AssetState::Available
            } else {
                AssetState::Unreadable
            }
        }
        // A folder, or a name that is not there (also when a part of the path
        // is a file).
        Ok(_) => AssetState::Missing,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            AssetState::Missing
        }
        // No permission to look at it, and the like.
        Err(_) => AssetState::Unreadable,
    }
}

/// Returns the folder of `kind` in `root`.
pub(crate) fn folder_of(root: &Path, kind: AssetKind) -> PathBuf {
    root.join(SOUNDS_DIR).join(kind.folder_name())
}

/// Adds a relative path (the parts divided by `/`) to `base`, one part at a
/// time, so that the separator is the one of the OS.
pub(crate) fn join_relative(mut base: PathBuf, relative: &str) -> PathBuf {
    for part in relative.split('/') {
        base.push(part);
    }
    base
}

fn absolute(root: &Path) -> Result<PathBuf, LibraryError> {
    std::path::absolute(root).map_err(|e| LibraryError::io(root, e))
}

fn read_index_file(path: &Path) -> Result<IndexFile, LibraryError> {
    match fs::read(path) {
        Ok(bytes) => Ok(match String::from_utf8(bytes) {
            Ok(text) => IndexFile::Text(text),
            Err(_) => IndexFile::NotUtf8,
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(IndexFile::Missing),
        Err(e) => Err(LibraryError::io(path, e)),
    }
}

fn not_utf8() -> LibraryError {
    LibraryError::Index(IndexError::Parse(
        "the file is not valid UTF-8 text".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::atomic_file::temp_path;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh folder under the system temp dir, unique per test.
    fn test_root(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ink-core-library-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    fn index_path(root: &Path) -> PathBuf {
        root.join(INDEX_FILE)
    }

    fn write_index(root: &Path, text: &str) {
        fs::write(index_path(root), text).unwrap();
    }

    /// Puts a file in the folder of `kind`.
    fn put_file(root: &Path, kind: AssetKind, relative: &str) {
        let path = join_relative(folder_of(root, kind), relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"sound").unwrap();
    }

    /// One entry of an index, with information (so that its file counts as
    /// read once).
    fn entry(id: &str, kind: &str, path: &str) -> String {
        format!(
            "[[asset]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{path}\"\n\n\
             [asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 2\nframes = 48000\n\n"
        )
    }

    /// One entry that was never read: no information.
    fn entry_unread(id: &str, kind: &str, path: &str) -> String {
        format!("[[asset]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{path}\"\n\n")
    }

    fn index_with(next_id: u64, entries: &[String]) -> String {
        format!("version = 1\nnext_id = {next_id}\n\n{}", entries.concat())
    }

    fn open(root: &Path) -> Library {
        Library::open(root).unwrap().library
    }

    /// Makes saving fail: a folder where the temporary file would be.
    fn block_saving(root: &Path) {
        fs::create_dir_all(temp_path(&index_path(root)).join("inner")).unwrap();
    }

    // -- open: no index -------------------------------------------------------

    #[test]
    fn a_folder_that_has_nothing_gets_the_sound_folders_and_no_index() {
        let root = test_root("fresh").join("new").join("deeper");
        let opened = Library::open(&root).unwrap();
        assert!(opened.warnings.is_empty());
        assert!(opened.library.assets().is_empty());
        assert_eq!(opened.library.next_id, 1);
        assert!(root.join("sounds").join("bgm").is_dir());
        assert!(root.join("sounds").join("se").is_dir());
        assert!(!index_path(&root).exists(), "nothing is written at open");
        assert_eq!(opened.library.root(), root);
        assert_eq!(
            opened.library.folder(AssetKind::Se),
            root.join("sounds").join("se")
        );
        assert_eq!(
            opened.library.folder(AssetKind::Bgm),
            root.join("sounds").join("bgm")
        );
    }

    #[test]
    fn folders_that_are_there_are_used_as_they_are() {
        let root = test_root("existing-folders");
        put_file(&root, AssetKind::Bgm, "kept.wav");
        let library = open(&root);
        assert!(library.assets().is_empty(), "a scan finds files, not open");
        assert!(root.join("sounds/bgm/kept.wav").is_file());
    }

    #[test]
    fn a_relative_root_becomes_an_absolute_path() {
        assert!(absolute(Path::new("a/b")).unwrap().is_absolute());
        assert!(absolute(Path::new("a/b")).unwrap().ends_with("a/b"));
        let here = std::env::temp_dir();
        assert_eq!(absolute(&here).unwrap(), here);
    }

    #[test]
    fn a_root_that_is_a_file_is_an_io_error() {
        let dir = test_root("root-is-file");
        let root = dir.join("root");
        fs::write(&root, b"not a folder").unwrap();
        assert!(matches!(Library::open(&root), Err(LibraryError::Io { .. })));
        assert_eq!(fs::read(&root).unwrap(), b"not a folder");
    }

    // -- open: an index that cannot be used -------------------------------------

    #[test]
    fn an_index_that_cannot_be_used_is_not_opened_and_not_touched() {
        type Expected = fn(&IndexError) -> bool;
        let cases: [(&str, Expected); 5] = [
            ("broken", |e| matches!(e, IndexError::Parse(_))),
            ("", |e| matches!(e, IndexError::MissingVersion)),
            ("version = 0\n", |e| {
                matches!(e, IndexError::UnsupportedVersion(0))
            }),
            ("version = 2\nanything = 1\n", |e| {
                matches!(e, IndexError::NewerVersion(2))
            }),
            ("version = \"1\"\n", |e| matches!(e, IndexError::Parse(_))),
        ];
        for (text, expected) in cases {
            let root = test_root("unusable");
            write_index(&root, text);
            let error = Library::open(&root).unwrap_err();
            assert!(
                matches!(&error, LibraryError::Index(e) if expected(e)),
                "{text:?}: {error}"
            );
            assert_eq!(fs::read_to_string(index_path(&root)).unwrap(), text);
            assert!(!root.join("sounds").exists(), "no folder is made");
            assert!(!temp_path(&index_path(&root)).exists());
        }
    }

    #[test]
    fn an_index_that_is_not_text_is_a_parse_error_and_is_not_touched() {
        let root = test_root("not-utf8");
        let bytes = [0xff_u8, 0xfe, b'v', b'e', 0x80];
        fs::write(index_path(&root), bytes).unwrap();
        assert!(matches!(
            Library::open(&root),
            Err(LibraryError::Index(IndexError::Parse(_)))
        ));
        assert_eq!(fs::read(index_path(&root)).unwrap(), bytes);
        assert!(!root.join("sounds").exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_index_that_cannot_be_read_is_an_io_error() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("no-permission");
        write_index(&root, "version = 1\n");
        fs::set_permissions(index_path(&root), fs::Permissions::from_mode(0o000)).unwrap();
        let result = Library::open(&root);
        fs::set_permissions(index_path(&root), fs::Permissions::from_mode(0o644)).unwrap();
        // A user who can read everything (root) is not stopped by the mode.
        if let Err(error) = result {
            assert!(
                matches!(&error, LibraryError::Io { source, .. }
                    if source.kind() == io::ErrorKind::PermissionDenied),
                "{error}"
            );
        }
    }

    // -- open: an index that can be used ------------------------------------------

    #[test]
    fn the_entries_of_the_index_are_the_assets_in_their_order() {
        let root = test_root("entries");
        write_index(
            &root,
            &index_with(
                9,
                &[
                    entry("se-0003", "se", "c.wav"),
                    entry("bgm-0001", "bgm", "album/a.wav"),
                    entry("se-0002", "se", "b.wav"),
                ],
            ),
        );
        let opened = Library::open(&root).unwrap();
        assert!(opened.warnings.is_empty());
        let ids: Vec<String> = opened
            .library
            .assets()
            .iter()
            .map(|a| a.id().to_string())
            .collect();
        assert_eq!(ids, ["se-0003", "bgm-0001", "se-0002"]);
        assert_eq!(opened.library.next_id, 9);
        let first = opened.library.get(&id("se-0003")).unwrap();
        assert_eq!(first.name(), "c");
        assert_eq!(first.kind(), AssetKind::Se);
        assert_eq!(opened.library.get(&id("se-0099")), None);
    }

    #[test]
    fn the_warnings_of_the_index_come_back_with_the_library() {
        let root = test_root("warnings");
        write_index(
            &root,
            &index_with(
                100,
                &[
                    entry("se-0001", "se", "ok.wav"),
                    entry("se-0002", "se", "../outside.wav"),
                ],
            ),
        );
        let opened = Library::open(&root).unwrap();
        assert_eq!(opened.library.assets().len(), 1);
        assert_eq!(opened.warnings.len(), 1);
        assert!(
            opened.warnings[0].to_string().contains("se-0002"),
            "{}",
            opened.warnings[0]
        );
    }

    #[test]
    fn the_state_is_found_from_the_file_without_reading_it() {
        let root = test_root("states");
        put_file(&root, AssetKind::Se, "read-once.wav");
        put_file(&root, AssetKind::Se, "never-read.wav");
        // A folder where a file should be, and a file where a folder should be.
        fs::create_dir_all(root.join("sounds/se/a-folder.wav")).unwrap();
        fs::write(root.join("sounds/se/a-file"), b"x").unwrap();
        write_index(
            &root,
            &index_with(
                100,
                &[
                    entry("se-0001", "se", "read-once.wav"),
                    entry_unread("se-0002", "se", "never-read.wav"),
                    entry("se-0003", "se", "gone.wav"),
                    entry("se-0004", "se", "a-folder.wav"),
                    entry("se-0005", "se", "a-file/inside.wav"),
                ],
            ),
        );
        let library = open(&root);
        let state = |text: &str| library.get(&id(text)).unwrap().state();
        assert_eq!(state("se-0001"), AssetState::Available);
        assert_eq!(state("se-0002"), AssetState::Unreadable, "never read");
        assert_eq!(state("se-0003"), AssetState::Missing);
        assert_eq!(state("se-0004"), AssetState::Missing, "a folder");
        assert_eq!(state("se-0005"), AssetState::Missing, "under a file");
    }

    #[test]
    fn the_trigger_words_of_the_index_are_kept() {
        let root = test_root("words");
        write_index(
            &root,
            "version = 1\nnext_id = 5\n\n[[asset]]\nid = \"se-0001\"\nkind = \"se\"\n\
             path = \"a.wav\"\ntrigger_words = [\"ハクシュ\"]\n",
        );
        let library = open(&root);
        assert_eq!(
            library.get(&id("se-0001")).unwrap().trigger_words(),
            ["ハクシュ"]
        );
    }

    // -- path_of ------------------------------------------------------------------

    #[test]
    fn the_path_of_an_asset_is_made_from_the_root_and_the_relative_path() {
        let root = test_root("path-of");
        write_index(
            &root,
            &index_with(
                100,
                &[
                    entry("se-0001", "se", "claps/拍手.wav"),
                    entry("bgm-0002", "bgm", "a b/音 声.mp3"),
                    entry("bgm-0003", "bgm", "plain.wav"),
                ],
            ),
        );
        let library = open(&root);
        let sounds = library.root().join("sounds");
        assert_eq!(
            library.path_of(&id("se-0001")).unwrap(),
            sounds.join("se").join("claps").join("拍手.wav")
        );
        assert_eq!(
            library.path_of(&id("bgm-0002")).unwrap(),
            sounds.join("bgm").join("a b").join("音 声.mp3")
        );
        assert_eq!(
            library.path_of(&id("bgm-0003")).unwrap(),
            sounds.join("bgm").join("plain.wav")
        );
        assert_eq!(library.path_of(&id("se-0099")), None);
    }

    #[test]
    fn a_path_stays_inside_the_folder_of_its_kind() {
        let root = test_root("path-inside");
        write_index(
            &root,
            &index_with(
                100,
                &[
                    entry("se-0001", "se", "a/b/c.wav"),
                    // These were written by hand and are left out at open.
                    entry("se-0002", "se", "../../escape.wav"),
                    entry("se-0003", "se", "/etc/passwd"),
                    entry("se-0004", "se", "C:/Windows/a.wav"),
                    entry("se-0005", "se", "a\\\\..\\\\..\\\\b.wav"),
                ],
            ),
        );
        let opened = Library::open(&root).unwrap();
        assert_eq!(opened.warnings.len(), 4);
        let library = opened.library;
        let folder = library.folder(AssetKind::Se);
        for asset in library.assets() {
            let path = library.path_of(asset.id()).unwrap();
            assert!(path.starts_with(&folder), "{}", path.display());
            let parts = asset.relative_path().split('/').count();
            assert_eq!(
                path.components().count(),
                folder.components().count() + parts
            );
        }
        for left_out in ["se-0002", "se-0003", "se-0004", "se-0005"] {
            assert_eq!(library.path_of(&id(left_out)), None, "{left_out}");
        }
    }

    // -- set_trigger_words ----------------------------------------------------------

    fn library_with_one_se(name: &str) -> (PathBuf, Library) {
        let root = test_root(name);
        put_file(&root, AssetKind::Se, "a.wav");
        write_index(&root, &index_with(2, &[entry("se-0001", "se", "a.wav")]));
        let library = open(&root);
        (root, library)
    }

    #[test]
    fn trigger_words_are_set_and_saved() {
        let (root, mut library) = library_with_one_se("words-save");
        let words = vec!["ハクシュ".to_string(), "カンパイ".to_string()];
        library
            .set_trigger_words(&id("se-0001"), words.clone())
            .unwrap();
        assert_eq!(library.get(&id("se-0001")).unwrap().trigger_words(), words);

        // The next time the library is opened, they are there.
        let again = open(&root);
        assert_eq!(again.get(&id("se-0001")).unwrap().trigger_words(), words);
        assert!(!temp_path(&index_path(&root)).exists());
        // And they can be taken away.
        library
            .set_trigger_words(&id("se-0001"), Vec::new())
            .unwrap();
        assert!(
            open(&root)
                .get(&id("se-0001"))
                .unwrap()
                .trigger_words()
                .is_empty()
        );
    }

    #[test]
    fn the_same_trigger_words_write_nothing() {
        let (root, mut library) = library_with_one_se("words-same");
        let words = vec!["ハクシュ".to_string()];
        library
            .set_trigger_words(&id("se-0001"), words.clone())
            .unwrap();
        // From now on a save fails, so any write would show.
        block_saving(&root);
        library.set_trigger_words(&id("se-0001"), words).unwrap();
        assert!(
            library
                .set_trigger_words(&id("se-0001"), Vec::new())
                .is_err()
        );
    }

    #[test]
    fn trigger_words_of_an_asset_that_is_not_there_are_refused() {
        let (_root, mut library) = library_with_one_se("words-unknown");
        let error = library
            .set_trigger_words(&id("se-0009"), vec!["x".to_string()])
            .unwrap_err();
        assert!(matches!(error, LibraryError::UnknownAsset(unknown) if unknown == id("se-0009")));
    }

    #[test]
    fn a_trigger_word_change_that_cannot_be_saved_changes_nothing() {
        let (root, mut library) = library_with_one_se("words-fail");
        library
            .set_trigger_words(&id("se-0001"), vec!["old".to_string()])
            .unwrap();
        let before = fs::read_to_string(index_path(&root)).unwrap();
        block_saving(&root);

        let error = library
            .set_trigger_words(&id("se-0001"), vec!["new".to_string()])
            .unwrap_err();
        assert!(matches!(error, LibraryError::Io { .. }), "{error}");
        assert_eq!(
            library.get(&id("se-0001")).unwrap().trigger_words(),
            ["old"]
        );
        assert_eq!(fs::read_to_string(index_path(&root)).unwrap(), before);
    }

    // -- forget_missing -------------------------------------------------------------

    fn library_for_forgetting(name: &str) -> (PathBuf, Library) {
        let root = test_root(name);
        put_file(&root, AssetKind::Se, "here.wav");
        put_file(&root, AssetKind::Se, "never-read.wav");
        write_index(
            &root,
            &index_with(
                4,
                &[
                    entry("se-0001", "se", "here.wav"),
                    entry("se-0002", "se", "gone.wav"),
                    entry_unread("se-0003", "se", "never-read.wav"),
                ],
            ),
        );
        let library = open(&root);
        (root, library)
    }

    #[test]
    fn an_asset_whose_file_is_missing_is_forgotten_and_the_index_is_saved() {
        let (root, mut library) = library_for_forgetting("forget");
        library.forget_missing(&id("se-0002")).unwrap();
        assert_eq!(library.get(&id("se-0002")), None);
        assert_eq!(library.assets().len(), 2);

        let again = open(&root);
        assert_eq!(again.assets().len(), 2);
        assert_eq!(again.get(&id("se-0002")), None);
    }

    #[test]
    fn an_asset_that_is_not_missing_is_not_forgotten() {
        let (root, mut library) = library_for_forgetting("forget-not-missing");
        let before = fs::read_to_string(index_path(&root)).ok();
        for text in ["se-0001", "se-0003"] {
            let error = library.forget_missing(&id(text)).unwrap_err();
            assert!(
                matches!(error, LibraryError::NotMissing(asset) if asset == id(text)),
                "{text}: {error}"
            );
            assert!(library.get(&id(text)).is_some());
        }
        assert_eq!(library.assets().len(), 3);
        assert_eq!(fs::read_to_string(index_path(&root)).ok(), before);
        assert!(matches!(
            library.forget_missing(&id("se-0077")),
            Err(LibraryError::UnknownAsset(_))
        ));
    }

    #[test]
    fn a_number_that_was_given_is_not_given_again_after_the_asset_is_forgotten() {
        let (root, _) = library_for_forgetting("forget-number");
        // se-0003 has the largest number and is forgotten after its file goes.
        fs::remove_file(root.join("sounds/se/never-read.wav")).unwrap();
        let mut library = open(&root);
        library.forget_missing(&id("se-0003")).unwrap();
        assert_eq!(library.next_id, 4, "the next number does not go back");
        // After a restart the next number is still 4.
        let again = open(&root);
        assert_eq!(again.next_id, 4);
        assert!(again.get(&id("se-0003")).is_none());
        let mut next = again.next_id;
        assert_eq!(issue_id(AssetKind::Se, &mut next).unwrap(), id("se-0004"));
    }

    #[test]
    fn forgetting_that_cannot_be_saved_keeps_the_asset() {
        let (root, mut library) = library_for_forgetting("forget-fail");
        // Save once so that the file is the one the library wrote.
        library
            .set_trigger_words(&id("se-0001"), vec!["x".to_string()])
            .unwrap();
        let before = fs::read_to_string(index_path(&root)).unwrap();
        block_saving(&root);

        let error = library.forget_missing(&id("se-0002")).unwrap_err();
        assert!(matches!(error, LibraryError::Io { .. }), "{error}");
        assert!(
            library.get(&id("se-0002")).is_some(),
            "still in the library"
        );
        assert_eq!(fs::read_to_string(index_path(&root)).unwrap(), before);
    }

    // -- reset_index ------------------------------------------------------------------

    fn broken_root(name: &str) -> PathBuf {
        let root = test_root(name);
        write_index(&root, "this is not an index");
        root
    }

    #[test]
    fn a_broken_index_is_moved_to_a_backup_and_the_library_can_open_empty() {
        let root = broken_root("reset");
        let backup = Library::reset_index(&root).unwrap();
        assert_eq!(backup, root.join("library.toml.broken"));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "this is not an index");
        assert!(!index_path(&root).exists());
        assert!(!root.join("sounds").exists(), "no folder is made");

        let opened = Library::open(&root).unwrap();
        assert!(opened.library.assets().is_empty());
        assert!(opened.warnings.is_empty());
    }

    #[test]
    fn a_backup_that_is_there_is_never_overwritten() {
        let root = broken_root("reset-twice");
        let first = Library::reset_index(&root).unwrap();
        write_index(&root, "second broken");
        let second = Library::reset_index(&root).unwrap();
        write_index(&root, "third broken");
        let third = Library::reset_index(&root).unwrap();

        assert_eq!(first, root.join("library.toml.broken"));
        assert_eq!(second, root.join("library.toml.broken-2"));
        assert_eq!(third, root.join("library.toml.broken-3"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "this is not an index");
        assert_eq!(fs::read_to_string(&second).unwrap(), "second broken");
        assert_eq!(fs::read_to_string(&third).unwrap(), "third broken");
    }

    #[test]
    fn a_free_name_in_the_middle_is_found() {
        let root = broken_root("reset-gap");
        fs::write(root.join("library.toml.broken"), b"1").unwrap();
        fs::write(root.join("library.toml.broken-3"), b"3").unwrap();
        let backup = Library::reset_index(&root).unwrap();
        assert_eq!(backup, root.join("library.toml.broken-2"));
    }

    #[test]
    fn an_index_that_can_be_read_is_not_moved() {
        let root = test_root("reset-readable");
        let text = index_with(5, &[entry("se-0001", "se", "a.wav")]);
        write_index(&root, &text);
        assert!(matches!(
            Library::reset_index(&root),
            Err(LibraryError::IndexNotBroken)
        ));
        assert_eq!(fs::read_to_string(index_path(&root)).unwrap(), text);

        // Also an index with warnings can be read.
        let with_warning = index_with(5, &[entry("se-0001", "se", "../x.wav")]);
        write_index(&root, &with_warning);
        assert!(matches!(
            Library::reset_index(&root),
            Err(LibraryError::IndexNotBroken)
        ));
        assert_eq!(fs::read_to_string(index_path(&root)).unwrap(), with_warning);
        assert!(!root.join("library.toml.broken").exists());
    }

    #[test]
    fn a_folder_with_no_index_has_nothing_to_move() {
        let root = test_root("reset-none");
        assert!(matches!(
            Library::reset_index(&root),
            Err(LibraryError::IndexNotBroken)
        ));
    }

    #[test]
    fn an_index_of_a_newer_version_and_one_that_is_not_text_are_moved_aside() {
        let newer = test_root("reset-newer");
        write_index(&newer, "version = 7\n");
        let backup = Library::reset_index(&newer).unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), "version = 7\n");

        let binary = test_root("reset-binary");
        fs::write(index_path(&binary), [0xff_u8, 0x00, 0xfe]).unwrap();
        let backup = Library::reset_index(&binary).unwrap();
        assert_eq!(fs::read(backup).unwrap(), [0xff_u8, 0x00, 0xfe]);
    }

    #[test]
    fn when_all_the_names_are_taken_there_is_no_backup() {
        let root = broken_root("reset-full");
        fs::write(root.join("library.toml.broken"), b"x").unwrap();
        for number in 2..=MAX_BACKUPS {
            fs::write(root.join(format!("library.toml.broken-{number}")), b"x").unwrap();
        }
        assert!(matches!(
            Library::reset_index(&root),
            Err(LibraryError::NoBackupName)
        ));
        assert_eq!(
            fs::read_to_string(index_path(&root)).unwrap(),
            "this is not an index"
        );
    }

    // -- the IDs -----------------------------------------------------------------------

    #[test]
    fn ids_are_given_one_number_after_the_other_and_never_the_same_twice() {
        let mut next = 5;
        assert_eq!(issue_id(AssetKind::Bgm, &mut next).unwrap(), id("bgm-0005"));
        assert_eq!(issue_id(AssetKind::Se, &mut next).unwrap(), id("se-0006"));
        assert_eq!(next, 7);
    }

    #[test]
    fn when_the_numbers_have_run_out_no_id_is_given() {
        let mut next = u64::MAX - 1;
        assert!(issue_id(AssetKind::Se, &mut next).is_ok());
        assert_eq!(next, u64::MAX);
        assert!(matches!(
            issue_id(AssetKind::Se, &mut next),
            Err(LibraryError::OutOfIds)
        ));
        assert_eq!(next, u64::MAX, "nothing moves");
        let mut zero = 0;
        assert!(matches!(
            issue_id(AssetKind::Se, &mut zero),
            Err(LibraryError::OutOfIds)
        ));
    }

    // -- the errors, the threads ---------------------------------------------------------

    #[test]
    fn an_error_says_what_is_wrong_in_a_line() {
        let io = LibraryError::Io {
            path: PathBuf::from("/data/library.toml"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        assert_eq!(io.to_string(), "/data/library.toml: denied");
        assert!(std::error::Error::source(&io).is_some());

        let index = LibraryError::Index(IndexError::NewerVersion(2));
        assert!(index.to_string().contains("written by a newer Ink Mixer"));
        assert!(std::error::Error::source(&index).is_some());

        assert_eq!(
            LibraryError::UnknownAsset(id("se-0007")).to_string(),
            "se-0007 is not in the library"
        );
        assert_eq!(
            LibraryError::NotMissing(id("se-0007")).to_string(),
            "se-0007 can only be forgotten when its file is missing"
        );
        assert_eq!(
            LibraryError::OutOfIds.to_string(),
            "no more IDs can be made"
        );
        assert!(
            LibraryError::IndexNotBroken
                .to_string()
                .contains("not moved aside")
        );
        assert!(LibraryError::NoBackupName.to_string().contains("1000"));
    }

    #[test]
    fn a_library_can_be_moved_to_a_worker_and_back() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Library>();
        assert_send_sync::<Opened>();
        assert_send_sync::<LibraryError>();

        let (root, library) = library_with_one_se("threads");
        let worker = std::thread::spawn(move || {
            let mut library = library;
            library
                .set_trigger_words(&id("se-0001"), vec!["ハクシュ".to_string()])
                .unwrap();
            library
        });
        let library = worker.join().unwrap();
        assert_eq!(
            library.get(&id("se-0001")).unwrap().trigger_words(),
            ["ハクシュ"]
        );
        assert_eq!(
            open(&root).get(&id("se-0001")).unwrap().trigger_words(),
            ["ハクシュ"]
        );
    }
}
