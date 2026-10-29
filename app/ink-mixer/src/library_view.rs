//! The sound library as the window sees it (#64): what to choose from, and
//! what a saved ID is now.
//!
//! The window owns one [`LibraryView`] and uses it on the UI thread only. It
//! only reads: it never imports, never scans, and never saves the index (that
//! is the job of the import page, #65). [`Library::open`] writes nothing when
//! the index cannot be used, so a broken or too new index is never touched
//! here. The window tells the user, in Japanese, what to do about it; it does
//! not move or reset the index itself. When the index can be used (or there
//! is none yet), opening makes the two empty folders of the sounds
//! (`sounds/bgm` and `sounds/se`) if they are not there, as ADR-0010 says; it
//! still writes no index.
//!
//! The workers get paths ([`Resolved::Found`]), never the library.

// The window uses all of this in the later steps of #64; remove this there.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "used by the window in later steps of #64")
)]

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ink_core::{Asset, AssetId, AssetKind, AssetState, IndexError, Library, LibraryError, Opened};
use tracing::warn;

/// The name of the index file, for the messages (the library owns the file).
const INDEX_FILE: &str = "library.toml";

/// Why the library cannot be used, for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    /// Which of the cases it is.
    pub kind: UnavailableKind,
    /// What is wrong, in one line of Japanese.
    pub summary_ja: String,
    /// What the user can do about it, in Japanese. `None` when there is
    /// nothing to say beyond the summary.
    pub howto_ja: Option<String>,
    /// The English detail from the library, for a bug report.
    pub detail: String,
}

/// The cases of [`Unavailable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableKind {
    /// The OS does not tell where the data folder is.
    NoFolder,
    /// The index is not TOML, has no version, or is not valid.
    BrokenIndex,
    /// The index was written by a newer Ink Mixer.
    NewerIndex,
    /// A file or folder of the library cannot be read or made.
    Io,
}

/// What the window knows about the library.
#[derive(Debug)]
pub enum LibraryView {
    /// The library is open. `warnings` is how many entries of the index were
    /// left out or changed (they are in the log; the import page shows them).
    Ready { library: Library, warnings: usize },
    /// The library cannot be used. Nothing was written.
    Unavailable(Unavailable),
}

/// One Asset to choose from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: AssetId,
    /// The display name.
    pub label: String,
    /// The length, if the file said it.
    pub length: Option<Duration>,
    pub state: AssetState,
}

impl Choice {
    /// Whether the Asset can be chosen: its file is there and can be read.
    pub fn selectable(&self) -> bool {
        self.state == AssetState::Available
    }

    /// The name for a menu, with a mark when the Asset cannot be used.
    pub fn menu_label(&self) -> String {
        match self.state {
            AssetState::Available => self.label.clone(),
            AssetState::Missing => format!("{}（ファイルが無い）", self.label),
            AssetState::Unreadable => format!("{}（読めない）", self.label),
        }
    }
}

/// What a saved ID is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The Asset is there and its file can be used.
    Found {
        /// The file, as an absolute path. For the workers; never saved.
        path: PathBuf,
        name: String,
        length: Option<Duration>,
    },
    /// The Asset is in the index but its file is not there.
    Missing { name: String },
    /// The file is there but it cannot be read.
    Unreadable { name: String },
    /// The library has no such Asset (or the library cannot be used).
    Unknown,
    /// The ID is of the other kind (a song where a sound effect is wanted).
    WrongKind,
}

impl LibraryView {
    /// Opens the library in `root`, or says why it cannot be used. `None`
    /// means the OS did not tell where the data folder is.
    ///
    /// The folders of the library are made if they are not there. The
    /// warnings of the index are logged (and counted).
    pub fn open(root: Option<&Path>) -> Self {
        let Some(root) = root else {
            return Self::Unavailable(no_folder());
        };
        match Library::open(root) {
            Ok(Opened { library, warnings }) => {
                for warning in &warnings {
                    warn!("library index: {warning}");
                }
                Self::Ready {
                    library,
                    warnings: warnings.len(),
                }
            }
            Err(error) => {
                warn!("the library cannot be used: {error}");
                Self::Unavailable(open_error_ja(root, &error))
            }
        }
    }

    /// Opens the library again: files that were put in by other means (the
    /// `library` command) are seen, and a library that cannot be used any
    /// more says so. Not a scan: the index is read, nothing is written.
    pub fn reload(&mut self, root: Option<&Path>) {
        *self = Self::open(root);
    }

    /// Returns the library, if it can be used. For the import page (#65),
    /// which will decide how to share it with a worker.
    pub fn library(&self) -> Option<&Library> {
        match self {
            Self::Ready { library, .. } => Some(library),
            Self::Unavailable(_) => None,
        }
    }

    /// Returns why the library cannot be used, if it cannot.
    pub fn unavailable(&self) -> Option<&Unavailable> {
        match self {
            Self::Ready { .. } => None,
            Self::Unavailable(unavailable) => Some(unavailable),
        }
    }

    /// Returns how many entries of the index were left out or changed.
    pub fn warnings(&self) -> usize {
        match self {
            Self::Ready { warnings, .. } => *warnings,
            Self::Unavailable(_) => 0,
        }
    }

    /// Returns the Assets of `kind` to choose from, sorted by name (small
    /// letters first, then the ID). Those whose file is missing or cannot be
    /// read are in the list too, with their state, so that the menu can mark
    /// them. Empty when the library cannot be used.
    pub fn choices(&self, kind: AssetKind) -> Vec<Choice> {
        let Some(library) = self.library() else {
            return Vec::new();
        };
        let mut choices: Vec<Choice> = library
            .assets()
            .iter()
            .filter(|asset| asset.kind() == kind)
            .map(choice_of)
            .collect();
        choices.sort_by_cached_key(|choice| (choice.label.to_lowercase(), choice.id));
        choices
    }

    /// Says what the saved `id` is now, for a place that wants an Asset of
    /// `kind`.
    pub fn resolve(&self, id: &AssetId, kind: AssetKind) -> Resolved {
        if id.kind() != kind {
            return Resolved::WrongKind;
        }
        let Some(library) = self.library() else {
            return Resolved::Unknown;
        };
        let Some(asset) = library.get(id) else {
            return Resolved::Unknown;
        };
        let name = asset.name().to_string();
        match asset.state() {
            AssetState::Missing => Resolved::Missing { name },
            AssetState::Unreadable => Resolved::Unreadable { name },
            AssetState::Available => match library.path_of(id) {
                Some(path) => Resolved::Found {
                    path,
                    name,
                    length: length_of(asset),
                },
                None => Resolved::Unknown,
            },
        }
    }
}

fn choice_of(asset: &Asset) -> Choice {
    Choice {
        id: *asset.id(),
        label: asset.name().to_string(),
        length: length_of(asset),
        state: asset.state(),
    }
}

fn length_of(asset: &Asset) -> Option<Duration> {
    asset.info().and_then(|info| info.duration())
}

fn no_folder() -> Unavailable {
    Unavailable {
        kind: UnavailableKind::NoFolder,
        summary_ja: "ライブラリが使えません：音源を置く場所（データフォルダ）が見つかりません"
            .to_string(),
        howto_ja: None,
        detail: "cannot find the data folder of this PC".to_string(),
    }
}

/// What to tell the user when [`Library::open`] failed for the library in
/// `root`: a line of Japanese, what to do when the index is the problem, and
/// the English detail.
///
/// Nothing is moved or deleted for the user. The steps tell them to move the
/// index aside themselves and not to delete it: a new index gives new IDs, and
/// the display names and the trigger words are only in the old one.
pub fn open_error_ja(root: &Path, error: &LibraryError) -> Unavailable {
    let index = root.join(INDEX_FILE);
    match error {
        LibraryError::Index(IndexError::NewerVersion(version)) => Unavailable {
            kind: UnavailableKind::NewerIndex,
            summary_ja: format!(
                "ライブラリが使えません：索引（{}）は新しい版の Ink Mixer で作られています（版 {version}）",
                index.display()
            ),
            howto_ja: Some(format!(
                "新しい版の Ink Mixer で開くのが安全です。この版で使うには、{}",
                move_aside_steps()
            )),
            detail: format!("{}: {error}", index.display()),
        },
        LibraryError::Index(_) => Unavailable {
            kind: UnavailableKind::BrokenIndex,
            summary_ja: format!(
                "ライブラリが使えません：索引（{}）を読めません",
                index.display()
            ),
            howto_ja: Some(format!("直すには、{}", move_aside_steps())),
            detail: format!("{}: {error}", index.display()),
        },
        LibraryError::Io { path, source } => Unavailable {
            kind: UnavailableKind::Io,
            summary_ja: format!(
                "ライブラリが使えません：{} を{}",
                path.display(),
                io_reason_ja(source)
            ),
            howto_ja: None,
            detail: error.to_string(),
        },
        other => Unavailable {
            kind: UnavailableKind::Io,
            summary_ja: format!(
                "ライブラリが使えません：{} を開けませんでした",
                root.display()
            ),
            howto_ja: None,
            detail: other.to_string(),
        },
    }
}

/// How to move the index aside, by hand.
fn move_aside_steps() -> String {
    format!(
        "Ink Mixer を閉じて、{INDEX_FILE} を {INDEX_FILE}.broken など別の名前に変えてください（消さないでください）。\
         開き直すと新しい索引が作られ、音源は新しい ID になり、表示名とトリガーワードは失われます。\
         保存してある曲・パッドの割り当ては別の音源を指すことがあるので、開き直したあとで確かめてください。"
    )
}

fn io_reason_ja(error: &io::Error) -> &'static str {
    if error.kind() == io::ErrorKind::PermissionDenied {
        "使う権限がありません"
    } else {
        "読み書きできません"
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ink-mixer-library-view-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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

    /// The folder of `kind`, one element at a time.
    fn folder(root: &Path, kind: AssetKind) -> PathBuf {
        root.join("sounds").join(kind.folder_name())
    }

    /// Puts a file in the folder of `kind`.
    fn put_file(root: &Path, kind: AssetKind, name: &str) {
        let folder = folder(root, kind);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(name), b"sound").unwrap();
    }

    /// An entry that was read once (it has information: 1 second at 48 kHz).
    fn entry(id: &str, kind: &str, path: &str) -> String {
        format!(
            "[[asset]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{path}\"\n\n\
             [asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 2\nframes = 48000\n\n"
        )
    }

    /// An entry that was never read: no information.
    fn entry_unread(id: &str, kind: &str, path: &str) -> String {
        format!("[[asset]]\nid = \"{id}\"\nkind = \"{kind}\"\npath = \"{path}\"\n\n")
    }

    fn index_with(entries: &[String]) -> String {
        format!("version = 1\nnext_id = 100\n\n{}", entries.concat())
    }

    /// A library with songs and sound effects in every state.
    fn library_with_everything(name: &str) -> (PathBuf, LibraryView) {
        let root = test_dir(name);
        put_file(&root, AssetKind::Bgm, "b-song.wav");
        put_file(&root, AssetKind::Bgm, "A-song.wav");
        put_file(&root, AssetKind::Bgm, "音楽 テスト.wav");
        put_file(&root, AssetKind::Bgm, "unread.wav");
        put_file(&root, AssetKind::Se, "clap.wav");
        write_index(
            &root,
            &index_with(&[
                entry("bgm-0001", "bgm", "b-song.wav"),
                entry("bgm-0002", "bgm", "A-song.wav"),
                entry("bgm-0003", "bgm", "gone.wav"),
                entry("bgm-0004", "bgm", "音楽 テスト.wav"),
                entry_unread("bgm-0005", "bgm", "unread.wav"),
                entry("se-0001", "se", "clap.wav"),
            ]),
        );
        let view = LibraryView::open(Some(&root));
        (root, view)
    }

    /// The names of the files in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // -- opening -------------------------------------------------------------------

    #[test]
    fn without_a_data_folder_the_library_cannot_be_used() {
        let view = LibraryView::open(None);
        let unavailable = view.unavailable().unwrap();
        assert_eq!(unavailable.kind, UnavailableKind::NoFolder);
        assert!(unavailable.summary_ja.contains("データフォルダ"));
        assert!(view.library().is_none());
        assert!(view.choices(AssetKind::Bgm).is_empty());
        assert_eq!(
            view.resolve(&id("bgm-0001"), AssetKind::Bgm),
            Resolved::Unknown
        );
    }

    #[test]
    fn an_empty_folder_opens_makes_the_folders_and_writes_no_index() {
        let root = test_dir("empty");
        let view = LibraryView::open(Some(&root));
        assert!(matches!(view, LibraryView::Ready { warnings: 0, .. }));
        assert!(view.unavailable().is_none());
        assert!(folder(&root, AssetKind::Bgm).is_dir());
        assert!(folder(&root, AssetKind::Se).is_dir());
        assert!(!index_path(&root).exists(), "opening saves nothing");
        assert!(view.choices(AssetKind::Bgm).is_empty());
    }

    // -- choices and resolve -------------------------------------------------------

    #[test]
    fn choices_are_of_the_kind_asked_for_sorted_by_name_and_marked() {
        let (_root, view) = library_with_everything("choices");
        let songs = view.choices(AssetKind::Bgm);
        let summary: Vec<(String, AssetState)> = songs
            .iter()
            .map(|choice| (choice.label.clone(), choice.state))
            .collect();
        assert_eq!(
            summary,
            [
                ("A-song".to_string(), AssetState::Available),
                ("b-song".to_string(), AssetState::Available),
                ("gone".to_string(), AssetState::Missing),
                ("unread".to_string(), AssetState::Unreadable),
                ("音楽 テスト".to_string(), AssetState::Available),
            ],
            "small letters first, so `A-song` is before `b-song`"
        );
        assert!(
            songs
                .iter()
                .all(|choice| choice.id.kind() == AssetKind::Bgm)
        );

        let effects = view.choices(AssetKind::Se);
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].id, id("se-0001"));
        assert_eq!(effects[0].length, Some(Duration::from_secs(1)));
    }

    #[test]
    fn a_choice_has_a_mark_and_cannot_be_chosen_when_its_file_is_not_usable() {
        let (_root, view) = library_with_everything("marks");
        let songs = view.choices(AssetKind::Bgm);
        let by_label = |label: &str| songs.iter().find(|c| c.label == label).unwrap();

        assert_eq!(by_label("b-song").menu_label(), "b-song");
        assert!(by_label("b-song").selectable());
        assert_eq!(by_label("gone").menu_label(), "gone（ファイルが無い）");
        assert!(!by_label("gone").selectable());
        assert_eq!(by_label("unread").menu_label(), "unread（読めない）");
        assert!(!by_label("unread").selectable());
        assert_eq!(by_label("音楽 テスト").menu_label(), "音楽 テスト");
        // A file that was never read has no length.
        assert_eq!(by_label("unread").length, None);
        assert_eq!(by_label("b-song").length, Some(Duration::from_secs(1)));
    }

    #[test]
    fn resolve_tells_found_missing_unreadable_unknown_and_wrong_kind() {
        let (root, view) = library_with_everything("resolve");
        let found = view.resolve(&id("bgm-0004"), AssetKind::Bgm);
        assert_eq!(
            found,
            Resolved::Found {
                path: folder(&root, AssetKind::Bgm).join("音楽 テスト.wav"),
                name: "音楽 テスト".to_string(),
                length: Some(Duration::from_secs(1)),
            }
        );
        assert_eq!(
            view.resolve(&id("bgm-0003"), AssetKind::Bgm),
            Resolved::Missing {
                name: "gone".to_string()
            }
        );
        assert_eq!(
            view.resolve(&id("bgm-0005"), AssetKind::Bgm),
            Resolved::Unreadable {
                name: "unread".to_string()
            }
        );
        assert_eq!(
            view.resolve(&id("bgm-0077"), AssetKind::Bgm),
            Resolved::Unknown
        );
        // A sound effect where a song is wanted, whether it exists or not.
        assert_eq!(
            view.resolve(&id("se-0001"), AssetKind::Bgm),
            Resolved::WrongKind
        );
        assert_eq!(
            view.resolve(&id("bgm-0001"), AssetKind::Se),
            Resolved::WrongKind
        );
        assert_eq!(
            view.resolve(&id("se-0099"), AssetKind::Bgm),
            Resolved::WrongKind
        );
    }

    // -- an index that cannot be used ------------------------------------------------

    #[test]
    fn a_broken_index_is_told_in_japanese_and_the_file_is_never_touched() {
        let cases: [(&str, &[u8]); 5] = [
            ("garbage", b"this is not toml at all"),
            ("open table", b"version = 1\n[[asset]\n"),
            ("no version", b"next_id = 3\n"),
            ("wrong type", b"version = \"one\"\n"),
            ("not utf-8", &[0xff, 0xfe, 0x00, 0x80]),
        ];
        for (name, bytes) in cases {
            let root = test_dir("broken");
            fs::write(index_path(&root), bytes).unwrap();

            let mut view = LibraryView::open(Some(&root));
            let unavailable = view.unavailable().unwrap_or_else(|| panic!("{name}"));
            assert_eq!(unavailable.kind, UnavailableKind::BrokenIndex, "{name}");
            assert_eq!(
                fs::read(index_path(&root)).unwrap(),
                bytes,
                "{name}: opened"
            );

            view.reload(Some(&root));
            assert!(view.unavailable().is_some(), "{name}");
            assert_eq!(
                fs::read(index_path(&root)).unwrap(),
                bytes,
                "{name}: reloaded"
            );

            // Nothing else was made either: no folders, no backup, no temporary file.
            assert_eq!(names(&root), [INDEX_FILE], "{name}");
        }
    }

    #[test]
    fn the_japanese_text_says_how_to_move_the_index_and_what_is_lost() {
        let root = test_dir("howto");
        write_index(&root, "garbage");
        let view = LibraryView::open(Some(&root));
        let unavailable = view.unavailable().unwrap();

        assert!(
            unavailable
                .summary_ja
                .contains(&index_path(&root).display().to_string()),
            "{}",
            unavailable.summary_ja
        );
        let howto = unavailable.howto_ja.as_deref().unwrap();
        assert!(howto.contains("library.toml.broken"), "{howto}");
        assert!(howto.contains("別の名前"), "{howto}");
        assert!(howto.contains("消さないでください"), "{howto}");
        assert!(howto.contains("新しい ID"), "{howto}");
        assert!(howto.contains("表示名"), "{howto}");
        assert!(howto.contains("トリガーワード"), "{howto}");
        assert!(howto.contains("割り当て"), "{howto}");
        // The English detail is there too.
        assert!(
            unavailable.detail.contains("library.toml"),
            "{}",
            unavailable.detail
        );
        assert!(
            unavailable.detail.contains("invalid"),
            "{}",
            unavailable.detail
        );
    }

    #[test]
    fn a_newer_index_is_told_as_newer_and_the_file_is_never_touched() {
        let root = test_dir("newer");
        let text = "version = 99\nnext_id = 4\nsomething_new = true\n";
        write_index(&root, text);

        let mut view = LibraryView::open(Some(&root));
        let unavailable = view.unavailable().unwrap();
        assert_eq!(unavailable.kind, UnavailableKind::NewerIndex);
        assert!(
            unavailable.summary_ja.contains("新しい版"),
            "{}",
            unavailable.summary_ja
        );
        assert!(
            unavailable.summary_ja.contains("99"),
            "{}",
            unavailable.summary_ja
        );
        let howto = unavailable.howto_ja.as_deref().unwrap();
        assert!(howto.contains("新しい版の Ink Mixer"), "{howto}");
        assert!(howto.contains("library.toml.broken"), "{howto}");
        assert!(howto.contains("消さないでください"), "{howto}");
        assert!(unavailable.detail.contains("99"), "{}", unavailable.detail);

        view.reload(Some(&root));
        assert!(view.unavailable().is_some());
        assert_eq!(fs::read_to_string(index_path(&root)).unwrap(), text);
        assert_eq!(names(&root), [INDEX_FILE]);
    }

    #[test]
    fn a_library_that_cannot_be_used_gives_nothing_to_choose_or_resolve() {
        let root = test_dir("nothing");
        write_index(&root, "garbage");
        let view = LibraryView::open(Some(&root));
        assert!(view.library().is_none());
        assert_eq!(view.warnings(), 0);
        assert!(view.choices(AssetKind::Bgm).is_empty());
        assert!(view.choices(AssetKind::Se).is_empty());
        assert_eq!(
            view.resolve(&id("bgm-0001"), AssetKind::Bgm),
            Resolved::Unknown
        );
        // The kind is still checked first.
        assert_eq!(
            view.resolve(&id("se-0001"), AssetKind::Bgm),
            Resolved::WrongKind
        );
    }

    #[test]
    fn a_folder_that_cannot_be_made_is_told_without_the_steps_for_the_index() {
        let root = test_dir("io");
        // A file where the folder `sounds` should be: the folders cannot be made.
        fs::write(root.join("sounds"), b"in the way").unwrap();
        let view = LibraryView::open(Some(&root));
        let unavailable = view.unavailable().unwrap();
        assert_eq!(unavailable.kind, UnavailableKind::Io);
        assert!(
            unavailable.summary_ja.contains("sounds"),
            "{}",
            unavailable.summary_ja
        );
        assert_eq!(unavailable.howto_ja, None);
        assert!(!unavailable.detail.is_empty());
    }

    #[test]
    fn a_denied_access_is_told_as_such() {
        let error = LibraryError::Io {
            path: PathBuf::from("data").join(INDEX_FILE),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let unavailable = open_error_ja(Path::new("data"), &error);
        assert_eq!(unavailable.kind, UnavailableKind::Io);
        assert!(
            unavailable.summary_ja.contains("権限"),
            "{}",
            unavailable.summary_ja
        );
        assert!(unavailable.summary_ja.contains(INDEX_FILE));
        assert_eq!(unavailable.howto_ja, None);

        let other = open_error_ja(
            Path::new("data"),
            &LibraryError::Io {
                path: PathBuf::from("data"),
                source: io::Error::from(io::ErrorKind::StorageFull),
            },
        );
        assert!(
            other.summary_ja.contains("読み書きできません"),
            "{}",
            other.summary_ja
        );
    }

    // -- reload and warnings ---------------------------------------------------------

    #[test]
    fn reload_sees_a_changed_index_and_comes_back_after_a_repair() {
        let root = test_dir("reload");
        let mut view = LibraryView::open(Some(&root));
        assert!(view.choices(AssetKind::Se).is_empty());

        // Put in by other means (the `library` command).
        put_file(&root, AssetKind::Se, "clap.wav");
        write_index(&root, &index_with(&[entry("se-0001", "se", "clap.wav")]));
        view.reload(Some(&root));
        assert_eq!(view.choices(AssetKind::Se).len(), 1);

        // Broken while the window is open: the window says so, the file stays.
        write_index(&root, "garbage");
        view.reload(Some(&root));
        assert!(view.unavailable().is_some());
        assert_eq!(fs::read(index_path(&root)).unwrap(), b"garbage");

        // The user moves it aside and the library starts again, empty.
        fs::rename(index_path(&root), root.join("library.toml.broken")).unwrap();
        view.reload(Some(&root));
        assert!(view.unavailable().is_none());
        assert!(view.choices(AssetKind::Se).is_empty());
    }

    #[test]
    fn the_entries_that_were_left_out_are_counted_and_the_others_are_kept() {
        let root = test_dir("warnings");
        put_file(&root, AssetKind::Se, "ok.wav");
        write_index(
            &root,
            &index_with(&[
                entry("se-0001", "se", "ok.wav"),
                entry("se-0002", "se", "../outside.wav"),
                entry("se-0001", "se", "again.wav"),
            ]),
        );
        let view = LibraryView::open(Some(&root));
        assert_eq!(view.warnings(), 2);
        let choices = view.choices(AssetKind::Se);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].id, id("se-0001"));
        // The window does not save the library, so the index is as it was.
        assert!(
            fs::read_to_string(index_path(&root))
                .unwrap()
                .contains("../outside.wav")
        );
    }
}
