//! The scan of the sound library (ADR-0010): find the files that were put in
//! the sound folders by hand, and bring the Assets and the files together.
//!
//! [`Library::scan`] walks `sounds/bgm` and `sounds/se`, and for each file:
//!
//! - a file that has an Asset (the same relative path) is read again with the
//!   caller's `probe`, every time, so that a file that was changed or broken
//!   is found;
//! - a file that has none gets a new Asset with the next ID;
//! - an Asset whose file was not found by the walk (it is deeper than the walk
//!   goes, or behind a link) is looked at with `metadata`: if the file is
//!   there it is read as above, and only if it is not there the Asset becomes
//!   [`AssetState::Missing`]. The Asset, its ID and what was read from the
//!   file stay in the index: nothing is taken out of it by a scan.
//!
//! What the walk does not take (and tells in [`ScanReport::skipped`]):
//!
//! - names that start with `.` are ignored without a word (hidden files, and
//!   the `.import-*.tmp` files of an import that is running);
//! - links are not followed, and are told as [`SkipReason::Symlink`]; a link
//!   that points back (a loop) is therefore harmless;
//! - folders deeper than [`MAX_DEPTH`] levels under the folder of the kind;
//! - names that are not UTF-8, names that are not safe on Windows (see
//!   [`PathProblem`]), files that are not WAV or MP3 by their extension, and
//!   a name that differs from the name of an Asset (or of another file) only
//!   in the case of its letters, which Windows would take for the same file.
//!
//! The order is fixed, so that the IDs do not depend on the order in which the
//! file system lists the files: the kind first, then the relative path in
//! lower case (and, for two paths that only differ in case, the path as it
//! is).
//!
//! The index is saved once at the end, and only if something that is written in
//! it has changed (a new Asset, or what was read from a file). A change of the
//! state alone (a file that went away or came back) is not saved: the state is
//! not part of the index. If saving fails, the library is as it was before the
//! scan, and no ID has been given: the next scan gives the same IDs.
//!
//! This does file I/O, and `probe` reads files: call it from a worker thread.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::asset::{Asset, AssetId, AssetInfo, AssetKind, AssetState, ProbeFailure};
use crate::library::{Library, LibraryError, folder_of, issue_id, join_relative, state_at};
use crate::library_index::{PathProblem, default_name, path_problem};

/// How many folders under the folder of the kind the walk enters:
/// `a/b/c/d/e/f/g/h/x.wav` is found, a file under a ninth folder is not.
pub const MAX_DEPTH: usize = 8;

/// The extensions of the files that are taken (in lower case). An import
/// takes the same ones.
pub(crate) const EXTENSIONS: [&str; 2] = ["wav", "mp3"];

/// The function that reads a file for its information, given by the caller
/// (the app gives `ink-media`'s). `ink-core` does not read sound files.
pub type ProbeFn<'a> = &'a dyn Fn(&Path) -> Result<AssetInfo, ProbeFailure>;

/// Why a file or folder was not taken by a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// The extension is not `wav` or `mp3`, or there is none.
    UnsupportedExtension,
    /// The name is not UTF-8 (only on Linux). The index is text.
    NameNotUtf8,
    /// The name cannot be used on every OS (Windows reserved names, `?`, a
    /// trailing dot, ...).
    UnsafeName(PathProblem),
    /// A folder deeper than [`MAX_DEPTH`]. It is not entered.
    TooDeep,
    /// A symbolic link (to a file or a folder). It is not followed.
    Symlink,
    /// A folder (or a name in it) could not be read: for example, no
    /// permission.
    NotReadable(io::ErrorKind),
    /// The same path as an Asset, or as another file, except for the case of
    /// the letters.
    CaseConflict,
}

/// A file or folder that a scan did not take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub kind: AssetKind,
    /// The path from the folder of the kind (with the separator of the OS).
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// What a scan changed. A list has only what changed in this scan: an Asset
/// that was missing before and still is, is not in `missing` again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// The new Assets (in the order of their IDs), whatever their state.
    pub added: Vec<AssetId>,
    /// The Assets whose file went away.
    pub missing: Vec<AssetId>,
    /// The Assets that were missing or unreadable and can be used again.
    pub restored: Vec<AssetId>,
    /// The Assets that cannot be read now, and why: the ones that could be
    /// read before, and the new ones that cannot be read.
    pub unreadable: Vec<(AssetId, ProbeFailure)>,
    /// What was not taken, ordered by kind and path.
    pub skipped: Vec<Skipped>,
}

impl ScanReport {
    /// Returns whether the scan found nothing to tell (also no skipped file).
    pub fn is_empty(&self) -> bool {
        !self.changed() && self.skipped.is_empty()
    }

    /// Returns whether an Asset was added or changed its state.
    pub fn changed(&self) -> bool {
        !(self.added.is_empty()
            && self.missing.is_empty()
            && self.restored.is_empty()
            && self.unreadable.is_empty())
    }
}

impl Library {
    /// Scans the sound folders (see the module documentation).
    ///
    /// # Errors
    ///
    /// - [`LibraryError::Io`] if the folder of a kind cannot be read, or the
    ///   index cannot be saved
    /// - [`LibraryError::OutOfIds`] if a new Asset needs an ID and the numbers
    ///   have run out
    ///
    /// In both cases the library is as it was before the scan.
    pub fn scan(&mut self, probe: ProbeFn<'_>) -> Result<ScanReport, LibraryError> {
        let mut report = ScanReport::default();

        let mut found: Vec<(AssetKind, String)> = Vec::new();
        for kind in AssetKind::ALL {
            let folder = folder_of(&self.root, kind);
            let mut walk = Walk {
                kind,
                found: Vec::new(),
                skipped: Vec::new(),
            };
            walk.directory(&folder, "", 0)
                .map_err(|source| LibraryError::Io {
                    path: folder.clone(),
                    source,
                })?;
            found.extend(walk.found.into_iter().map(|rel| (kind, rel)));
            report.skipped.extend(walk.skipped);
        }
        found.sort_by_cached_key(|(kind, rel)| (*kind, rel.to_lowercase(), rel.clone()));

        // The changes are made on a copy, and taken when they are saved.
        let mut assets = self.assets.clone();
        let mut next_id = self.next_id;
        let known = assets.len();
        let mut seen = vec![false; known];
        let mut exact: HashMap<(AssetKind, String), usize> = HashMap::new();
        let mut lower: HashMap<(AssetKind, String), usize> = HashMap::new();
        for (i, asset) in assets.iter().enumerate() {
            exact.insert((asset.kind(), asset.relative_path().to_string()), i);
            lower.insert((asset.kind(), asset.relative_path().to_lowercase()), i);
        }
        let mut info_changed = false;

        for (kind, rel) in found {
            let path = join_relative(folder_of(&self.root, kind), &rel);
            if let Some(&i) = exact.get(&(kind, rel.clone())) {
                seen[i] = true;
                refresh(&mut assets[i], &path, probe, &mut report, &mut info_changed);
                continue;
            }
            let lower_key = (kind, rel.to_lowercase());
            if lower.contains_key(&lower_key) {
                report.skipped.push(Skipped {
                    kind,
                    path: relative_path_buf(&rel),
                    reason: SkipReason::CaseConflict,
                });
                continue;
            }

            let id = issue_id(kind, &mut next_id)?;
            let (info, state) = match probe(&path) {
                Ok(info) => (Some(info), AssetState::Available),
                Err(failure) => {
                    report.unreadable.push((id, failure));
                    (None, AssetState::Unreadable)
                }
            };
            assets.push(Asset::new(id, default_name(&rel), rel.clone(), info, state));
            report.added.push(id);
            exact.insert((kind, rel), assets.len() - 1);
            lower.insert(lower_key, assets.len() - 1);
        }

        // The Assets whose files the walk did not find.
        for i in (0..known).filter(|&i| !seen[i]) {
            let path = join_relative(
                folder_of(&self.root, assets[i].kind()),
                assets[i].relative_path(),
            );
            let before = assets[i].state();
            match state_at(&path, true) {
                AssetState::Available => {
                    refresh(&mut assets[i], &path, probe, &mut report, &mut info_changed);
                }
                AssetState::Missing => {
                    assets[i].set_state(AssetState::Missing);
                    if before != AssetState::Missing {
                        report.missing.push(*assets[i].id());
                    }
                }
                AssetState::Unreadable => {
                    assets[i].set_state(AssetState::Unreadable);
                    if before != AssetState::Unreadable {
                        report.unreadable.push((
                            *assets[i].id(),
                            ProbeFailure::Unreadable("the file cannot be looked at".to_string()),
                        ));
                    }
                }
            }
        }

        report
            .skipped
            .sort_by(|a, b| (a.kind, &a.path).cmp(&(b.kind, &b.path)));

        // Only what is written in the index is a reason to write it.
        if !report.added.is_empty() || info_changed {
            self.save(&assets, next_id)?;
        }
        self.assets = assets;
        self.next_id = next_id;
        Ok(report)
    }
}

/// Reads the file of an Asset that has been found again, and sets its state
/// and what was read from it.
fn refresh(
    asset: &mut Asset,
    path: &Path,
    probe: ProbeFn<'_>,
    report: &mut ScanReport,
    info_changed: &mut bool,
) {
    let before = asset.state();
    match probe(path) {
        Ok(info) => {
            if asset.info() != Some(&info) {
                asset.set_info(Some(info));
                *info_changed = true;
            }
            asset.set_state(AssetState::Available);
            if before != AssetState::Available {
                report.restored.push(*asset.id());
            }
        }
        Err(failure) => {
            // What was read the last time stays.
            asset.set_state(AssetState::Unreadable);
            if before != AssetState::Unreadable {
                report.unreadable.push((*asset.id(), failure));
            }
        }
    }
}

/// Turns a relative path with `/` into a path of the OS.
fn relative_path_buf(rel: &str) -> PathBuf {
    if rel.is_empty() {
        PathBuf::new()
    } else {
        join_relative(PathBuf::new(), rel)
    }
}

/// The walk of the folder of one kind.
struct Walk {
    kind: AssetKind,
    /// The relative paths (with `/`) of the files that can be taken.
    found: Vec<String>,
    skipped: Vec<Skipped>,
}

impl Walk {
    /// Walks the folder `rel` (relative to `base`, the folder of the kind) at
    /// `depth` levels under it. Fails if the folder cannot be opened; what
    /// cannot be read inside it is skipped.
    fn directory(&mut self, base: &Path, rel: &str, depth: usize) -> io::Result<()> {
        let path = if rel.is_empty() {
            base.to_path_buf()
        } else {
            join_relative(base.to_path_buf(), rel)
        };
        for entry in fs::read_dir(&path)? {
            match entry {
                Ok(entry) => self.entry(base, rel, depth, &entry),
                Err(e) => {
                    self.skip(relative_path_buf(rel), SkipReason::NotReadable(e.kind()));
                    break;
                }
            }
        }
        Ok(())
    }

    fn entry(&mut self, base: &Path, rel: &str, depth: usize, entry: &fs::DirEntry) {
        let os_name = entry.file_name();
        // Hidden names, and the temporary files of an import.
        if os_name.to_string_lossy().starts_with('.') {
            return;
        }
        let shown = || {
            let mut path = relative_path_buf(rel);
            path.push(&os_name);
            path
        };
        // `DirEntry::file_type` does not follow links.
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                self.skip(shown(), SkipReason::NotReadable(e.kind()));
                return;
            }
        };
        if file_type.is_symlink() {
            self.skip(shown(), SkipReason::Symlink);
            return;
        }
        if !file_type.is_dir() && !file_type.is_file() {
            // Sockets, devices, ...
            return;
        }
        let Some(name) = os_name.to_str() else {
            self.skip(shown(), SkipReason::NameNotUtf8);
            return;
        };
        let child = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        if let Some(problem) = path_problem(&child) {
            self.skip(shown(), SkipReason::UnsafeName(problem));
            return;
        }

        if file_type.is_dir() {
            if depth + 1 > MAX_DEPTH {
                self.skip(shown(), SkipReason::TooDeep);
            } else if let Err(e) = self.directory(base, &child, depth + 1) {
                self.skip(shown(), SkipReason::NotReadable(e.kind()));
            }
            return;
        }

        let supported = name.rsplit_once('.').is_some_and(|(_, extension)| {
            EXTENSIONS
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        });
        if supported {
            self.found.push(child);
        } else {
            self.skip(shown(), SkipReason::UnsupportedExtension);
        }
    }

    fn skip(&mut self, path: PathBuf, reason: SkipReason) {
        self.skipped.push(Skipped {
            kind: self.kind,
            path,
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::asset::AssetFormat;
    use crate::atomic_file::temp_path;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh folder under the system temp dir, unique per test.
    fn test_root(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ink-core-scan-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    /// A fake `probe`: the file says what it is in its first bytes.
    /// `GOOD...` is a WAV (its frames are its size), `DENY...` cannot be read,
    /// anything else is not a sound. It remembers the paths it was asked for.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<PathBuf>>,
    }

    impl Fake {
        fn probe(&self, path: &Path) -> Result<AssetInfo, ProbeFailure> {
            self.calls.borrow_mut().push(path.to_path_buf());
            let bytes = fs::read(path).map_err(|e| ProbeFailure::Unreadable(e.to_string()))?;
            if bytes.starts_with(b"GOOD") {
                Ok(AssetInfo {
                    format: AssetFormat::Wav,
                    sample_rate: 48000,
                    channels: 2,
                    frames: Some(bytes.len() as u64),
                })
            } else if bytes.starts_with(b"DENY") {
                Err(ProbeFailure::Unreadable("denied".to_string()))
            } else {
                Err(ProbeFailure::NotAudio("not a sound".to_string()))
            }
        }

        fn count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    fn run(fake: &Fake, library: &mut Library) -> Result<ScanReport, LibraryError> {
        library.scan(&|path| fake.probe(path))
    }

    fn put(root: &Path, kind: AssetKind, relative: &str, content: &[u8]) {
        let path = join_relative(folder_of(root, kind), relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn good(root: &Path, kind: AssetKind, relative: &str) {
        put(root, kind, relative, b"GOOD sound");
    }

    fn open(root: &Path) -> Library {
        Library::open(root).unwrap().library
    }

    fn index_path(root: &Path) -> PathBuf {
        root.join("library.toml")
    }

    /// Makes saving fail: a folder where the temporary file would be.
    fn block_saving(root: &Path) {
        fs::create_dir_all(temp_path(&index_path(root)).join("inner")).unwrap();
    }

    fn unblock_saving(root: &Path) {
        fs::remove_dir_all(temp_path(&index_path(root))).unwrap();
    }

    /// `(relative path, ID)` of every Asset, in the order of the library.
    fn paths_and_ids(library: &Library) -> Vec<(String, String)> {
        library
            .assets()
            .iter()
            .map(|a| (a.relative_path().to_string(), a.id().to_string()))
            .collect()
    }

    fn state(library: &Library, text: &str) -> AssetState {
        library.get(&id(text)).unwrap().state()
    }

    fn skipped_reasons(report: &ScanReport) -> Vec<(String, SkipReason)> {
        report
            .skipped
            .iter()
            .map(|s| (s.path.to_string_lossy().replace('\\', "/"), s.reason))
            .collect()
    }

    // -- new files ---------------------------------------------------------------

    #[test]
    fn files_that_were_put_in_become_assets_with_the_next_ids() {
        let root = test_root("add");
        good(&root, AssetKind::Bgm, "b.wav");
        good(&root, AssetKind::Bgm, "album/c.mp3");
        good(&root, AssetKind::Bgm, "a.wav");
        good(&root, AssetKind::Se, "拍手 1.wav");
        let mut library = open(&root);
        let fake = Fake::default();

        let report = run(&fake, &mut library).unwrap();
        assert_eq!(
            report.added,
            [
                id("bgm-0001"),
                id("bgm-0002"),
                id("bgm-0003"),
                id("se-0004")
            ]
        );
        assert!(report.skipped.is_empty());
        assert!(report.unreadable.is_empty());
        assert_eq!(
            paths_and_ids(&library),
            [
                ("a.wav".to_string(), "bgm-0001".to_string()),
                ("album/c.mp3".to_string(), "bgm-0002".to_string()),
                ("b.wav".to_string(), "bgm-0003".to_string()),
                ("拍手 1.wav".to_string(), "se-0004".to_string()),
            ]
        );
        let se = library.get(&id("se-0004")).unwrap();
        assert_eq!(se.name(), "拍手 1");
        assert_eq!(se.state(), AssetState::Available);
        assert_eq!(se.info().unwrap().format, AssetFormat::Wav);
        assert_eq!(library.next_id, 5);
        // The probe was given the real, absolute path of each file.
        for call in fake.calls.borrow().iter() {
            assert!(call.is_absolute());
            assert!(call.starts_with(root.join("sounds")), "{}", call.display());
        }
    }

    #[test]
    fn the_ids_do_not_depend_on_the_order_the_files_were_made() {
        let names = ["B.wav", "a.wav", "c/z.wav", "c/Y.wav", "d.mp3"];
        let make = |name: &str, reversed: bool| {
            let root = test_root(name);
            let order: Vec<&str> = if reversed {
                names.iter().rev().copied().collect()
            } else {
                names.to_vec()
            };
            for path in order {
                good(&root, AssetKind::Bgm, path);
            }
            let mut library = open(&root);
            run(&Fake::default(), &mut library).unwrap();
            paths_and_ids(&library)
        };
        let forward = make("order-forward", false);
        let backward = make("order-backward", true);
        assert_eq!(forward, backward);
        assert_eq!(
            forward,
            [
                ("a.wav", "bgm-0001"),
                ("B.wav", "bgm-0002"),
                ("c/Y.wav", "bgm-0003"),
                ("c/z.wav", "bgm-0004"),
                ("d.mp3", "bgm-0005"),
            ]
            .map(|(p, i)| (p.to_string(), i.to_string()))
        );
    }

    #[test]
    fn the_same_name_in_the_two_kinds_is_two_assets() {
        let root = test_root("both-kinds");
        good(&root, AssetKind::Bgm, "same.wav");
        good(&root, AssetKind::Se, "same.wav");
        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added, [id("bgm-0001"), id("se-0002")]);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn a_file_that_cannot_be_read_is_added_without_information() {
        let root = test_root("add-bad");
        put(&root, AssetKind::Se, "broken.wav", b"BAD");
        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added, [id("se-0001")]);
        assert_eq!(report.unreadable.len(), 1);
        assert_eq!(report.unreadable[0].0, id("se-0001"));
        assert!(matches!(report.unreadable[0].1, ProbeFailure::NotAudio(_)));
        let asset = library.get(&id("se-0001")).unwrap();
        assert_eq!(asset.state(), AssetState::Unreadable);
        assert_eq!(asset.info(), None);
        // Its ID is on the disk.
        assert!(open(&root).get(&id("se-0001")).is_some());
    }

    #[test]
    fn the_ids_and_the_next_number_go_on_after_a_restart() {
        let root = test_root("restart");
        good(&root, AssetKind::Se, "one.wav");
        let mut library = open(&root);
        run(&Fake::default(), &mut library).unwrap();
        drop(library);

        good(&root, AssetKind::Se, "two.wav");
        let mut library = open(&root);
        assert_eq!(library.next_id, 2);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added, [id("se-0002")]);
        assert_eq!(
            library.get(&id("se-0001")).unwrap().relative_path(),
            "one.wav"
        );
    }

    // -- files that are known ------------------------------------------------------

    #[test]
    fn a_scan_reads_every_file_again_and_has_nothing_to_tell_when_nothing_changed() {
        let root = test_root("again");
        good(&root, AssetKind::Se, "a.wav");
        good(&root, AssetKind::Bgm, "b.wav");
        let mut library = open(&root);
        let fake = Fake::default();
        run(&fake, &mut library).unwrap();
        assert_eq!(fake.count(), 2);

        // Writing the index would fail from now on.
        block_saving(&root);
        let report = run(&fake, &mut library).unwrap();
        assert!(report.is_empty(), "{report:?}");
        assert!(!report.changed());
        assert_eq!(fake.count(), 4, "every file is read at every scan");
    }

    #[test]
    fn a_file_that_goes_away_is_missing_and_stays_in_the_index() {
        let root = test_root("missing");
        good(&root, AssetKind::Se, "a.wav");
        good(&root, AssetKind::Se, "b.wav");
        let mut library = open(&root);
        let fake = Fake::default();
        run(&fake, &mut library).unwrap();
        let info_before = library.get(&id("se-0001")).unwrap().info().copied();
        assert!(info_before.is_some());

        // A state alone is not written: saving would fail, the scan does not.
        block_saving(&root);
        fs::remove_file(root.join("sounds/se/a.wav")).unwrap();
        let report = run(&fake, &mut library).unwrap();
        assert_eq!(report.missing, [id("se-0001")]);
        assert!(report.added.is_empty());
        assert_eq!(library.assets().len(), 2, "nothing is taken out");
        let asset = library.get(&id("se-0001")).unwrap();
        assert_eq!(asset.state(), AssetState::Missing);
        assert_eq!(asset.info().copied(), info_before);

        // It is told once.
        let report = run(&fake, &mut library).unwrap();
        assert!(report.is_empty(), "{report:?}");
        assert_eq!(state(&library, "se-0001"), AssetState::Missing);

        // And it is still in the index after a restart.
        unblock_saving(&root);
        good(&root, AssetKind::Se, "c.wav");
        run(&fake, &mut library).unwrap();
        let again = open(&root);
        assert_eq!(again.assets().len(), 3);
        assert_eq!(state(&again, "se-0001"), AssetState::Missing);
    }

    #[test]
    fn a_file_that_comes_back_is_restored_with_the_same_id() {
        let root = test_root("restored");
        good(&root, AssetKind::Se, "a.wav");
        let mut library = open(&root);
        let fake = Fake::default();
        run(&fake, &mut library).unwrap();
        fs::remove_file(root.join("sounds/se/a.wav")).unwrap();
        run(&fake, &mut library).unwrap();

        good(&root, AssetKind::Se, "a.wav");
        let report = run(&fake, &mut library).unwrap();
        assert_eq!(report.restored, [id("se-0001")]);
        assert!(report.added.is_empty());
        assert_eq!(state(&library, "se-0001"), AssetState::Available);
        assert_eq!(library.assets().len(), 1);
    }

    #[test]
    fn a_file_that_cannot_be_read_any_more_keeps_what_was_read_before() {
        let root = test_root("goes-bad");
        good(&root, AssetKind::Se, "a.wav");
        let mut library = open(&root);
        let fake = Fake::default();
        run(&fake, &mut library).unwrap();
        let info_before = library.get(&id("se-0001")).unwrap().info().copied();

        block_saving(&root);
        put(&root, AssetKind::Se, "a.wav", b"DENY");
        let report = run(&fake, &mut library).unwrap();
        assert_eq!(report.unreadable.len(), 1);
        assert_eq!(report.unreadable[0].0, id("se-0001"));
        assert!(matches!(
            report.unreadable[0].1,
            ProbeFailure::Unreadable(_)
        ));
        let asset = library.get(&id("se-0001")).unwrap();
        assert_eq!(asset.state(), AssetState::Unreadable);
        assert_eq!(asset.info().copied(), info_before);

        // Told once, then restored when the file is good again.
        assert!(run(&fake, &mut library).unwrap().is_empty());
        good(&root, AssetKind::Se, "a.wav");
        let report = run(&fake, &mut library).unwrap();
        assert_eq!(report.restored, [id("se-0001")]);
    }

    #[test]
    fn a_file_that_was_never_read_is_read_when_it_is_good_and_the_index_is_saved() {
        let root = test_root("fixed");
        put(&root, AssetKind::Se, "a.wav", b"BAD");
        let mut library = open(&root);
        let fake = Fake::default();
        run(&fake, &mut library).unwrap();

        good(&root, AssetKind::Se, "a.wav");
        let report = run(&fake, &mut library).unwrap();
        assert_eq!(report.restored, [id("se-0001")]);
        assert!(library.get(&id("se-0001")).unwrap().info().is_some());
        // The information was saved.
        let again = open(&root);
        assert!(again.get(&id("se-0001")).unwrap().info().is_some());
        assert_eq!(state(&again, "se-0001"), AssetState::Available);
    }

    #[test]
    fn a_file_that_changed_gets_its_information_updated_and_saved() {
        let root = test_root("changed");
        good(&root, AssetKind::Se, "a.wav");
        let mut library = open(&root);
        let fake = Fake::default();
        run(&fake, &mut library).unwrap();
        let frames_before = library.get(&id("se-0001")).unwrap().info().unwrap().frames;

        put(
            &root,
            AssetKind::Se,
            "a.wav",
            b"GOOD a much longer sound than before",
        );
        let report = run(&fake, &mut library).unwrap();
        assert!(
            report.is_empty(),
            "only the information changed: {report:?}"
        );
        let frames = library.get(&id("se-0001")).unwrap().info().unwrap().frames;
        assert_ne!(frames, frames_before);
        assert_eq!(
            open(&root)
                .get(&id("se-0001"))
                .unwrap()
                .info()
                .unwrap()
                .frames,
            frames,
            "saved"
        );
    }

    #[test]
    fn an_asset_whose_file_the_walk_does_not_reach_is_not_called_missing() {
        let root = test_root("not-reached");
        let deep = "a/b/c/d/e/f/g/h/i/deep.wav";
        good(&root, AssetKind::Se, deep);
        good(&root, AssetKind::Se, "here.wav");
        fs::write(
            index_path(&root),
            format!(
                "version = 1\nnext_id = 5\n\n\
                 [[asset]]\nid = \"se-0001\"\nkind = \"se\"\npath = \"{deep}\"\n\n\
                 [asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 2\nframes = 10\n\n\
                 [[asset]]\nid = \"se-0002\"\nkind = \"se\"\npath = \"gone.wav\"\n\n\
                 [asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 2\nframes = 10\n"
            ),
        )
        .unwrap();
        let mut library = open(&root);
        let fake = Fake::default();

        let report = run(&fake, &mut library).unwrap();
        assert_eq!(state(&library, "se-0001"), AssetState::Available);
        assert_eq!(state(&library, "se-0002"), AssetState::Missing);
        // It was already missing when the library was opened: not told again.
        assert!(report.missing.is_empty());
        assert_eq!(report.added, [id("se-0005")]);
        assert!(
            fake.calls
                .borrow()
                .contains(&join_relative(folder_of(&root, AssetKind::Se), deep)),
            "the file was read although the walk did not find it"
        );
        assert_eq!(
            skipped_reasons(&report),
            [("a/b/c/d/e/f/g/h/i".to_string(), SkipReason::TooDeep)]
        );
    }

    // -- the walk ---------------------------------------------------------------------

    #[test]
    fn the_walk_goes_8_folders_down_and_not_9() {
        let root = test_root("depth");
        let eight = "a/b/c/d/e/f/g/h";
        good(&root, AssetKind::Se, &format!("{eight}/x.wav"));
        good(&root, AssetKind::Se, &format!("{eight}/i/y.wav"));
        let mut library = open(&root);

        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added.len(), 1);
        assert_eq!(
            library.assets()[0].relative_path(),
            format!("{eight}/x.wav")
        );
        assert_eq!(
            skipped_reasons(&report),
            [(format!("{eight}/i"), SkipReason::TooDeep)],
            "the folder is told once, not each file in it"
        );
    }

    #[test]
    fn hidden_names_and_the_temporary_files_of_an_import_are_ignored_silently() {
        let root = test_root("hidden");
        good(&root, AssetKind::Se, ".hidden.wav");
        good(&root, AssetKind::Se, ".import-1.tmp");
        good(&root, AssetKind::Se, ".git/inside.wav");
        good(&root, AssetKind::Se, "sub/.import-2.tmp");
        good(&root, AssetKind::Se, "sub/ok.wav");
        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added.len(), 1);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    }

    #[test]
    fn files_that_are_not_wav_or_mp3_are_told_and_not_taken() {
        let root = test_root("extensions");
        good(&root, AssetKind::Bgm, "notes.txt");
        good(&root, AssetKind::Bgm, "no-extension");
        good(&root, AssetKind::Bgm, "song.flac");
        good(&root, AssetKind::Bgm, "LOUD.WAV");
        good(&root, AssetKind::Bgm, "Mixed.Mp3");
        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added.len(), 2, "upper case extensions are taken");
        assert_eq!(
            skipped_reasons(&report),
            [
                ("no-extension".to_string(), SkipReason::UnsupportedExtension),
                ("notes.txt".to_string(), SkipReason::UnsupportedExtension),
                ("song.flac".to_string(), SkipReason::UnsupportedExtension),
            ]
        );
    }

    // Windows cannot create these names with `fs::write` (error 123); checked on a real machine (#33).
    #[cfg(unix)]
    #[test]
    fn names_that_windows_cannot_have_are_told_and_not_taken() {
        let root = test_root("unsafe");
        good(&root, AssetKind::Se, "CON.wav");
        good(&root, AssetKind::Se, "what?.wav");
        good(&root, AssetKind::Se, "trailing.wav.");
        good(&root, AssetKind::Se, "bad?folder/inside.wav");
        good(&root, AssetKind::Se, "fine.wav");
        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added.len(), 1);
        assert_eq!(
            skipped_reasons(&report),
            [
                (
                    "CON.wav".to_string(),
                    SkipReason::UnsafeName(PathProblem::ReservedName)
                ),
                (
                    "bad?folder".to_string(),
                    SkipReason::UnsafeName(PathProblem::ForbiddenCharacter('?'))
                ),
                (
                    "trailing.wav.".to_string(),
                    SkipReason::UnsafeName(PathProblem::EndsWithDotOrSpace)
                ),
                (
                    "what?.wav".to_string(),
                    SkipReason::UnsafeName(PathProblem::ForbiddenCharacter('?'))
                ),
            ]
        );
    }

    // Windows treats names that differ only in case as one file; checked on a real machine (#33).
    #[cfg(unix)]
    #[test]
    fn a_name_that_differs_from_an_asset_only_in_case_is_a_conflict() {
        let root = test_root("case-asset");
        good(&root, AssetKind::Se, "Song.wav");
        let mut library = open(&root);
        let fake = Fake::default();
        run(&fake, &mut library).unwrap();

        good(&root, AssetKind::Se, "song.wav");
        let report = run(&fake, &mut library).unwrap();
        assert!(report.added.is_empty());
        assert_eq!(
            skipped_reasons(&report),
            [("song.wav".to_string(), SkipReason::CaseConflict)]
        );
        assert_eq!(library.assets().len(), 1);
        assert_eq!(library.assets()[0].relative_path(), "Song.wav");
    }

    // Windows treats names that differ only in case as one file; checked on a real machine (#33).
    #[cfg(unix)]
    #[test]
    fn two_new_files_that_differ_only_in_case_give_one_asset_and_a_conflict() {
        let root = test_root("case-new");
        good(&root, AssetKind::Se, "x.wav");
        good(&root, AssetKind::Se, "X.wav");
        good(&root, AssetKind::Se, "Dir/a.wav");
        good(&root, AssetKind::Se, "dir/b.wav");
        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        // "X.wav" comes before "x.wav" and wins; "Dir/a.wav" and "dir/b.wav"
        // are different paths.
        assert_eq!(report.added.len(), 3);
        assert_eq!(
            skipped_reasons(&report),
            [("x.wav".to_string(), SkipReason::CaseConflict)]
        );
        // What was taken has no two paths that are the same on Windows.
        let mut lower: Vec<String> = library
            .assets()
            .iter()
            .map(|a| a.relative_path().to_lowercase())
            .collect();
        lower.sort();
        lower.dedup();
        assert_eq!(lower.len(), library.assets().len());
        assert!(
            library
                .assets()
                .iter()
                .any(|a| a.relative_path() == "X.wav")
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_are_told_and_not_followed() {
        use std::os::unix::fs::symlink;

        let root = test_root("links");
        good(&root, AssetKind::Se, "real.wav");
        let outside = test_root("links-outside");
        good(&outside, AssetKind::Se, "elsewhere.wav");
        let folder = folder_of(&root, AssetKind::Se);
        symlink(folder.join("real.wav"), folder.join("link.wav")).unwrap();
        symlink(outside.join("sounds/se"), folder.join("linked-folder")).unwrap();
        // A link that points back to the folder it is in.
        symlink(".", folder.join("loop")).unwrap();
        fs::create_dir_all(folder.join("sub")).unwrap();
        symlink("..", folder.join("sub/up")).unwrap();

        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added, [id("se-0001")]);
        assert_eq!(library.assets()[0].relative_path(), "real.wav");
        assert_eq!(
            skipped_reasons(&report),
            [
                ("link.wav".to_string(), SkipReason::Symlink),
                ("linked-folder".to_string(), SkipReason::Symlink),
                ("loop".to_string(), SkipReason::Symlink),
                ("sub/up".to_string(), SkipReason::Symlink),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_asset_behind_a_link_is_not_called_missing() {
        use std::os::unix::fs::symlink;

        let root = test_root("link-asset");
        good(&root, AssetKind::Se, "real.wav");
        let folder = folder_of(&root, AssetKind::Se);
        symlink(folder.join("real.wav"), folder.join("link.wav")).unwrap();
        fs::write(
            index_path(&root),
            "version = 1\nnext_id = 2\n\n\
             [[asset]]\nid = \"se-0001\"\nkind = \"se\"\npath = \"link.wav\"\n\n\
             [asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 2\nframes = 10\n",
        )
        .unwrap();
        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert!(report.missing.is_empty());
        assert_eq!(state(&library, "se-0001"), AssetState::Available);
        assert_eq!(report.added, [id("se-0002")], "real.wav is new");
        assert_eq!(
            skipped_reasons(&report),
            [("link.wav".to_string(), SkipReason::Symlink)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_told_and_not_taken() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let root = test_root("non-utf8");
        let folder = folder_of(&root, AssetKind::Se);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(OsStr::from_bytes(b"bad\xff.wav")), b"GOOD").unwrap();
        good(&root, AssetKind::Se, "fine.wav");
        let mut library = open(&root);
        let report = run(&Fake::default(), &mut library).unwrap();
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::NameNotUtf8);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_read_is_told_and_the_rest_is_scanned() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("no-permission");
        good(&root, AssetKind::Se, "locked/inside.wav");
        good(&root, AssetKind::Se, "open.wav");
        let locked = root.join("sounds/se/locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = fs::read_dir(&locked).is_ok();

        let mut library = open(&root);
        let result = run(&Fake::default(), &mut library);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let report = result.unwrap();
        // A user who can read everything (root) is not stopped by the mode.
        if !readable_anyway {
            assert_eq!(report.added.len(), 1);
            assert_eq!(
                skipped_reasons(&report),
                [(
                    "locked".to_string(),
                    SkipReason::NotReadable(io::ErrorKind::PermissionDenied)
                )]
            );
        }
    }

    #[test]
    fn a_folder_of_a_kind_that_cannot_be_read_is_an_error() {
        let root = test_root("no-folder");
        let mut library = open(&root);
        fs::remove_dir_all(root.join("sounds/se")).unwrap();
        good(&root, AssetKind::Bgm, "a.wav");
        let error = run(&Fake::default(), &mut library).unwrap_err();
        assert!(matches!(error, LibraryError::Io { .. }), "{error}");
        assert!(library.assets().is_empty(), "nothing was taken");
    }

    // -- when it cannot be saved -------------------------------------------------------

    #[test]
    fn a_scan_that_cannot_be_saved_changes_nothing_and_gives_no_id() {
        let root = test_root("save-fails");
        good(&root, AssetKind::Se, "a.wav");
        good(&root, AssetKind::Bgm, "b.wav");
        let mut library = open(&root);
        let fake = Fake::default();
        block_saving(&root);

        let error = run(&fake, &mut library).unwrap_err();
        assert!(matches!(error, LibraryError::Io { .. }), "{error}");
        assert!(library.assets().is_empty());
        assert_eq!(library.next_id, 1);
        assert!(!index_path(&root).exists());

        // The next scan gives the same IDs: none was given before.
        unblock_saving(&root);
        let report = run(&fake, &mut library).unwrap();
        assert_eq!(report.added, [id("bgm-0001"), id("se-0002")]);
        assert_eq!(open(&root).assets().len(), 2);
    }

    #[test]
    fn a_failed_save_leaves_the_states_as_they_were_too() {
        let root = test_root("save-fails-states");
        good(&root, AssetKind::Se, "a.wav");
        let mut library = open(&root);
        let fake = Fake::default();
        run(&fake, &mut library).unwrap();

        fs::remove_file(root.join("sounds/se/a.wav")).unwrap();
        good(&root, AssetKind::Se, "b.wav");
        block_saving(&root);
        assert!(run(&fake, &mut library).is_err());
        assert_eq!(state(&library, "se-0001"), AssetState::Available);
        assert_eq!(library.assets().len(), 1);
    }

    #[test]
    fn when_the_numbers_have_run_out_a_new_file_is_an_error_and_nothing_changes() {
        let root = test_root("out-of-ids");
        good(&root, AssetKind::Se, "a.wav");
        let mut library = open(&root);
        library.next_id = u64::MAX;
        let error = run(&Fake::default(), &mut library).unwrap_err();
        assert!(matches!(error, LibraryError::OutOfIds), "{error}");
        assert!(library.assets().is_empty());
        assert_eq!(library.next_id, u64::MAX);
    }

    // -- the report, the threads --------------------------------------------------------

    #[test]
    fn a_report_says_whether_it_has_anything_to_tell() {
        let mut report = ScanReport::default();
        assert!(report.is_empty());
        assert!(!report.changed());

        report.skipped.push(Skipped {
            kind: AssetKind::Se,
            path: PathBuf::from("x.txt"),
            reason: SkipReason::UnsupportedExtension,
        });
        assert!(!report.is_empty());
        assert!(!report.changed(), "a skipped file is not a change");

        report.restored.push(id("se-0001"));
        assert!(report.changed());
    }

    #[test]
    fn a_library_can_be_scanned_on_a_worker_and_come_back() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ScanReport>();
        assert_send_sync::<Skipped>();

        let root = test_root("threads");
        good(&root, AssetKind::Se, "a.wav");
        let library = open(&root);
        let worker = std::thread::spawn(move || {
            let mut library = library;
            let fake = Fake::default();
            let report = run(&fake, &mut library).unwrap();
            (library, report)
        });
        let (library, report) = worker.join().unwrap();
        assert_eq!(report.added, [id("se-0001")]);
        assert_eq!(library.assets().len(), 1);
    }
}
