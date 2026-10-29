//! The index of the sound library (`library.toml`, ADR-0010): the text of the
//! file, and what is read from it and written to it.
//!
//! This module only turns text into entries and entries into text. It does not
//! touch the disk: the library reads the file, and saves it with
//! [`write_atomically`](crate::atomic_file::write_atomically).
//!
//! # Reading
//!
//! - A file that is not TOML, has no `version`, or has a `version` that this
//!   build does not know is an [`IndexError`]: the library is not opened, and
//!   the file is not overwritten (the same rule as `config.toml`).
//! - An entry that cannot be used (a path that could point outside the sound
//!   folders, an ID that is used twice, ...) is left out, and an
//!   [`IndexWarning`] says why. The other entries are used.
//! - What the file says of the state of a file is never read: the state is
//!   not saved.
//!
//! # Writing
//!
//! Every asset gets `trigger_words = []` (also when it has none), and the
//! `[asset.info]` table only when its information is known.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::asset::{Asset, AssetId, AssetIdError, AssetInfo, AssetKind};

/// The version of the index written by this build. Adding an optional item
/// does not change it; renaming an item, or changing its meaning or type
/// does, and then the migration is added to `parse`.
pub const LIBRARY_VERSION: u32 = 1;

/// Why an index could not be read or written. The library is not opened and
/// the file is not touched.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexError {
    /// The text is not TOML, or an item has the wrong type (the `version`,
    /// the `next_id`, or the list of assets).
    Parse(String),
    /// The text has no `version`.
    MissingVersion,
    /// The `version` is newer than [`LIBRARY_VERSION`]: the file was written
    /// by a newer Ink Mixer.
    NewerVersion(u32),
    /// The `version` is not one that any Ink Mixer has written (0, or a
    /// negative number or one that does not fit in 32 bits).
    UnsupportedVersion(i64),
    /// The index could not be turned into text.
    Write(String),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(reason) => write!(f, "invalid library index: {reason}"),
            Self::MissingVersion => f.write_str("invalid library index: it has no `version`"),
            Self::NewerVersion(version) => write!(
                f,
                "the library index has version {version}, written by a newer Ink Mixer \
                 (this build supports {LIBRARY_VERSION})"
            ),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported library index version {version}")
            }
            Self::Write(reason) => write!(f, "the library index cannot be written: {reason}"),
        }
    }
}

impl std::error::Error for IndexError {}

/// What is wrong with a relative path of an Asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathProblem {
    /// The path is empty.
    Empty,
    /// The path starts with `/`.
    Absolute,
    /// The path has a character that a file name of Windows cannot have, or
    /// that is a separator of Windows (`\`, `:`, `<`, `>`, `"`, `|`, `?`, `*`,
    /// or a control character).
    ForbiddenCharacter(char),
    /// A part of the path is empty (`a//b`, a `/` at the end).
    EmptyPart,
    /// A part of the path starts with `.` (`.`, `..`, a hidden name).
    StartsWithDot,
    /// A part of the path ends with `.` or a space (Windows drops them).
    EndsWithDotOrSpace,
    /// A part of the path is a name that Windows keeps for devices (`CON`,
    /// `NUL`, `COM1`, ...), with or without an extension.
    ReservedName,
}

impl fmt::Display for PathProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the path is empty"),
            Self::Absolute => f.write_str("the path is absolute"),
            Self::ForbiddenCharacter(c) => write!(f, "the path has the character {c:?}"),
            Self::EmptyPart => f.write_str("a part of the path is empty"),
            Self::StartsWithDot => f.write_str("a part of the path starts with `.`"),
            Self::EndsWithDotOrSpace => f.write_str("a part of the path ends with `.` or a space"),
            Self::ReservedName => f.write_str("a part of the path is a name kept by Windows"),
        }
    }
}

/// Checks a relative path of an Asset: the path from the folder of its kind,
/// with `/` between the parts. Returns what is wrong with it, or `None` if it
/// is safe: it stays inside the folder and its names can be used on Windows
/// and on Linux.
///
/// The same rule is used for a path read from the index and for the name of a
/// file that the library finds in its folders.
pub fn path_problem(path: &str) -> Option<PathProblem> {
    if path.is_empty() {
        return Some(PathProblem::Empty);
    }
    if path.starts_with('/') {
        return Some(PathProblem::Absolute);
    }
    if let Some(c) = path.chars().find(|c| is_forbidden(*c)) {
        return Some(PathProblem::ForbiddenCharacter(c));
    }
    for part in path.split('/') {
        if part.is_empty() {
            return Some(PathProblem::EmptyPart);
        }
        if part.starts_with('.') {
            return Some(PathProblem::StartsWithDot);
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Some(PathProblem::EndsWithDotOrSpace);
        }
        if is_reserved_name(part) {
            return Some(PathProblem::ReservedName);
        }
    }
    None
}

fn is_forbidden(c: char) -> bool {
    c.is_control() || matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
}

/// Whether Windows keeps the name for a device: `CON`, `PRN`, `AUX`, `NUL`,
/// `COM1` to `COM9`, `LPT1` to `LPT9`, in any case, with or without an
/// extension (`nul.wav`).
fn is_reserved_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|device| {
            stem.strip_prefix(*device)
                .is_some_and(|number| matches!(number.as_bytes(), [b'1'..=b'9']))
        })
}

/// Why an entry of the index was left out, or a value was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexWarning {
    /// The entry has the wrong shape (a missing `id`, `kind` or `path`, a
    /// value of the wrong type). `position` counts from 1.
    BadEntry { position: usize, reason: String },
    /// The `id` of the entry is not an ID.
    BadId {
        position: usize,
        text: String,
        reason: AssetIdError,
    },
    /// The start of the ID is not the `kind` of the entry.
    KindMismatch { id: AssetId, kind: AssetKind },
    /// An earlier entry has the same ID.
    DuplicateId { id: AssetId },
    /// The path of the entry is not safe (see [`path_problem`]).
    UnsafePath {
        id: AssetId,
        path: String,
        problem: PathProblem,
    },
    /// An earlier entry has the same path (compared without regard to the
    /// case), in the folder of the same kind.
    DuplicatePath {
        id: AssetId,
        path: String,
        first: AssetId,
    },
    /// The `[asset.info]` of the entry cannot be read; the entry is kept
    /// without information.
    BadInfo { id: AssetId, reason: String },
    /// The `trigger_words` of the entry are not a list of texts; the entry is
    /// kept without them.
    BadTriggerWords { id: AssetId },
    /// The `next_id` was not larger than every ID of the file (also those of
    /// entries that were left out), so it was made larger. `from` is `None`
    /// when the file has none.
    NextIdRaised { from: Option<i64>, to: u64 },
}

impl fmt::Display for IndexWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadEntry { position, reason } => {
                write!(
                    f,
                    "entry {position} of the library index is left out: {reason}"
                )
            }
            Self::BadId {
                position,
                text,
                reason,
            } => write!(
                f,
                "entry {position} of the library index is left out: {text:?} is not an ID ({reason})"
            ),
            Self::KindMismatch { id, kind } => write!(
                f,
                "{id} is left out: its kind is {}, but its ID is not of that kind",
                kind.folder_name()
            ),
            Self::DuplicateId { id } => {
                write!(f, "{id} is left out: the ID is used twice")
            }
            Self::UnsafePath { id, path, problem } => {
                write!(
                    f,
                    "{id} is left out: the path {path:?} is not safe ({problem})"
                )
            }
            Self::DuplicatePath { id, path, first } => write!(
                f,
                "{id} is left out: the path {path:?} is already the path of {first}"
            ),
            Self::BadInfo { id, reason } => {
                write!(f, "{id}: its information is ignored: {reason}")
            }
            Self::BadTriggerWords { id } => {
                write!(
                    f,
                    "{id}: its trigger words are ignored: not a list of texts"
                )
            }
            Self::NextIdRaised { from, to } => match from {
                Some(from) => write!(f, "`next_id` {from} is raised to {to}"),
                None => write!(f, "the `next_id` is set to {to}"),
            },
        }
    }
}

/// One entry of the index, as it is read. The state of the file is not part of
/// it: the library finds it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    pub(crate) id: AssetId,
    pub(crate) name: String,
    /// The path from the folder of the kind, with `/` between the parts. It
    /// is safe (see [`path_problem`]).
    pub(crate) path: String,
    pub(crate) info: Option<AssetInfo>,
    pub(crate) trigger_words: Vec<String>,
}

/// What [`parse`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedIndex {
    /// The number of the next ID. It is larger than the number of every ID in
    /// the file.
    pub(crate) next_id: u64,
    /// The entries that can be used, in the order of the file.
    pub(crate) entries: Vec<IndexEntry>,
    pub(crate) warnings: Vec<IndexWarning>,
}

/// Returns the name to show for a file that has no name in the index: the
/// name of the file without its extension.
pub(crate) fn default_name(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => file.to_string(),
    }
}

/// The shape of an entry. The id is read as a text, so that a warning can
/// say what it was; the information and the trigger words are read apart, so
/// that a mistake in them does not lose the entry.
#[derive(Deserialize)]
struct RawEntry {
    id: String,
    kind: AssetKind,
    name: Option<String>,
    path: String,
    trigger_words: Option<toml::Value>,
    info: Option<toml::Value>,
}

/// Reads the text of an index.
///
/// # Errors
///
/// An [`IndexError`] if the text is not TOML, has no `version`, has a version
/// that this build does not know (the version is looked at before anything
/// else, so a file of a newer version is not read as if it were this one), or
/// has a `next_id` or an `asset` list of the wrong type.
pub(crate) fn parse(text: &str) -> Result<ParsedIndex, IndexError> {
    let mut table: toml::Table =
        toml::from_str(text).map_err(|e| IndexError::Parse(e.to_string()))?;

    check_version(&table)?;

    let declared_next_id = match table.get("next_id") {
        None => None,
        Some(toml::Value::Integer(n)) => Some(*n),
        Some(_) => {
            return Err(IndexError::Parse(
                "`next_id` must be an integer".to_string(),
            ));
        }
    };
    let raw_entries = match table.remove("asset") {
        None => Vec::new(),
        Some(toml::Value::Array(entries)) => entries,
        Some(_) => {
            return Err(IndexError::Parse(
                "`asset` must be a list of tables (`[[asset]]`)".to_string(),
            ));
        }
    };

    let mut warnings = Vec::new();
    let mut entries: Vec<IndexEntry> = Vec::new();
    let mut ids: HashSet<AssetId> = HashSet::new();
    let mut paths: HashMap<(AssetKind, String), AssetId> = HashMap::new();
    // The largest number of every ID that could be read, also in the entries
    // that are left out: such an ID was once given, so it must not be given
    // again.
    let mut largest: u64 = 0;

    for (index, value) in raw_entries.into_iter().enumerate() {
        let position = index + 1;
        // The ID of an entry that cannot be used is still an ID that was given
        // once, if it can be read.
        if let Some(given) = value
            .get("id")
            .and_then(toml::Value::as_str)
            .and_then(|text| text.parse::<AssetId>().ok())
        {
            largest = largest.max(given.number());
        }
        let raw: RawEntry = match value.try_into() {
            Ok(raw) => raw,
            Err(e) => {
                warnings.push(IndexWarning::BadEntry {
                    position,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        let id: AssetId = match raw.id.parse() {
            Ok(id) => id,
            Err(reason) => {
                warnings.push(IndexWarning::BadId {
                    position,
                    text: raw.id,
                    reason,
                });
                continue;
            }
        };

        if !id.is_of_kind(raw.kind) {
            warnings.push(IndexWarning::KindMismatch { id, kind: raw.kind });
            continue;
        }
        if !ids.insert(id) {
            warnings.push(IndexWarning::DuplicateId { id });
            continue;
        }
        if let Some(problem) = path_problem(&raw.path) {
            warnings.push(IndexWarning::UnsafePath {
                id,
                path: raw.path,
                problem,
            });
            continue;
        }
        let key = (id.kind(), raw.path.to_lowercase());
        if let Some(first) = paths.get(&key) {
            warnings.push(IndexWarning::DuplicatePath {
                id,
                path: raw.path,
                first: *first,
            });
            continue;
        }
        paths.insert(key, id);

        let info = match raw.info {
            None => None,
            Some(value) => match value.try_into::<AssetInfo>() {
                Ok(info) => Some(info),
                Err(e) => {
                    warnings.push(IndexWarning::BadInfo {
                        id,
                        reason: e.to_string(),
                    });
                    None
                }
            },
        };
        let trigger_words = match raw.trigger_words {
            None => Vec::new(),
            Some(value) => match value.try_into::<Vec<String>>() {
                Ok(words) => words,
                Err(_) => {
                    warnings.push(IndexWarning::BadTriggerWords { id });
                    Vec::new()
                }
            },
        };
        let name = raw.name.unwrap_or_else(|| default_name(&raw.path));
        entries.push(IndexEntry {
            id,
            name,
            path: raw.path,
            info,
            trigger_words,
        });
    }

    // The next ID is larger than every ID that was ever given.
    let wanted = largest.saturating_add(1).max(1);
    let next_id = match declared_next_id.and_then(|n| u64::try_from(n).ok()) {
        Some(n) if n >= wanted => n,
        _ => {
            // A file with no `next_id` and no entry starts at 1 without a
            // warning.
            if declared_next_id.is_some() || largest > 0 {
                warnings.push(IndexWarning::NextIdRaised {
                    from: declared_next_id,
                    to: wanted,
                });
            }
            wanted
        }
    };

    Ok(ParsedIndex {
        next_id,
        entries,
        warnings,
    })
}

fn check_version(table: &toml::Table) -> Result<(), IndexError> {
    let version = match table.get("version") {
        None => return Err(IndexError::MissingVersion),
        Some(toml::Value::Integer(n)) => *n,
        Some(_) => {
            return Err(IndexError::Parse(
                "`version` must be an integer".to_string(),
            ));
        }
    };
    if version == i64::from(LIBRARY_VERSION) {
        // Older versions will be migrated here once the format changes.
        Ok(())
    } else if version > i64::from(LIBRARY_VERSION) {
        Err(IndexError::NewerVersion(
            u32::try_from(version).unwrap_or(u32::MAX),
        ))
    } else {
        // 0 or a negative number.
        Err(IndexError::UnsupportedVersion(version))
    }
}

/// The shape of the text that is written.
#[derive(Serialize)]
struct OutIndex<'a> {
    version: u32,
    next_id: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    asset: Vec<OutEntry<'a>>,
}

#[derive(Serialize)]
struct OutEntry<'a> {
    id: AssetId,
    kind: AssetKind,
    name: &'a str,
    path: &'a str,
    trigger_words: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<&'a AssetInfo>,
}

/// Writes the text of an index: `next_id` and `assets`, in their order.
///
/// # Errors
///
/// [`IndexError::Write`] if the text cannot be made (it does not happen for
/// the values of an [`Asset`]).
pub(crate) fn to_toml(next_id: u64, assets: &[Asset]) -> Result<String, IndexError> {
    let index = OutIndex {
        version: LIBRARY_VERSION,
        next_id,
        asset: assets
            .iter()
            .map(|asset| OutEntry {
                id: *asset.id(),
                kind: asset.kind(),
                name: asset.name(),
                path: asset.relative_path(),
                trigger_words: asset.trigger_words(),
                info: asset.info(),
            })
            .collect(),
    };
    toml::to_string(&index).map_err(|e| IndexError::Write(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AssetFormat, AssetState};

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    fn info() -> AssetInfo {
        AssetInfo {
            format: AssetFormat::Wav,
            sample_rate: 48_000,
            channels: 2,
            frames: Some(96_000),
        }
    }

    fn asset(id_text: &str, name: &str, path: &str, info: Option<AssetInfo>) -> Asset {
        Asset::new(
            id(id_text),
            name.to_string(),
            path.to_string(),
            info,
            AssetState::Available,
        )
    }

    /// The example of ADR-0010, without the comments.
    const ADR_EXAMPLE: &str = "\
version = 1
next_id = 13

[[asset]]
id = \"se-0012\"
kind = \"se\"
name = \"拍手\"
path = \"claps/拍手.wav\"
trigger_words = []

[asset.info]
format = \"wav\"
sample_rate = 48000
channels = 2
frames = 96000
";

    /// One entry written by hand, with the fields of the example.
    fn entry_text(fields: &str) -> String {
        format!("version = 1\nnext_id = 100\n\n[[asset]]\n{fields}")
    }

    fn parse_ok(text: &str) -> ParsedIndex {
        parse(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    // -- the text of ADR-0010 -------------------------------------------------

    #[test]
    fn the_example_of_the_adr_is_written_exactly() {
        let assets = [asset("se-0012", "拍手", "claps/拍手.wav", Some(info()))];
        assert_eq!(to_toml(13, &assets).unwrap(), ADR_EXAMPLE);
    }

    #[test]
    fn the_example_of_the_adr_is_read_with_its_comments() {
        let text = "\
version = 1
next_id = 13

[[asset]]
id = \"se-0012\"
kind = \"se\"
name = \"拍手\"                  # 表示名（既定はファイル名の拡張子を除いた部分）
path = \"claps/拍手.wav\"        # 種類のフォルダからの相対パス。区切りは常に \"/\"
trigger_words = []

[asset.info]
format = \"wav\"                 # \"wav\" | \"mp3\"
sample_rate = 48000
channels = 2
frames = 96000                 # ファイルが言わなければ書かない
";
        let parsed = parse_ok(text);
        assert_eq!(parsed.next_id, 13);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(
            parsed.entries,
            [IndexEntry {
                id: id("se-0012"),
                name: "拍手".to_string(),
                path: "claps/拍手.wav".to_string(),
                info: Some(info()),
                trigger_words: Vec::new(),
            }]
        );
        assert_eq!(parse_ok(ADR_EXAMPLE), parsed);
    }

    // -- writing --------------------------------------------------------------

    #[test]
    fn an_empty_index_is_the_version_and_the_next_id() {
        assert_eq!(to_toml(1, &[]).unwrap(), "version = 1\nnext_id = 1\n");
        let parsed = parse_ok("version = 1\nnext_id = 1\n");
        assert_eq!(parsed.next_id, 1);
        assert!(parsed.entries.is_empty() && parsed.warnings.is_empty());
    }

    #[test]
    fn the_trigger_words_are_always_written_and_the_unknown_information_is_not() {
        let assets = [asset("bgm-0001", "song", "song.mp3", None)];
        let text = to_toml(2, &assets).unwrap();
        assert_eq!(
            text,
            "\
version = 1
next_id = 2

[[asset]]
id = \"bgm-0001\"
kind = \"bgm\"
name = \"song\"
path = \"song.mp3\"
trigger_words = []
"
        );
        assert!(!text.contains("[asset.info]"));
    }

    #[test]
    fn a_frame_count_that_is_not_known_is_not_written() {
        let unknown = AssetInfo {
            format: AssetFormat::Mp3,
            frames: None,
            ..info()
        };
        let text = to_toml(2, &[asset("bgm-0001", "s", "s.mp3", Some(unknown))]).unwrap();
        assert!(text.contains("[asset.info]"), "{text}");
        assert!(text.contains("format = \"mp3\""), "{text}");
        assert!(!text.contains("frames"), "{text}");
    }

    #[test]
    fn what_is_written_is_read_back_the_same() {
        let mut with_words = asset(
            "se-0003",
            "He said \"hello\" \\ 日本語 ✓",
            "a b/音 声.mp3",
            Some(AssetInfo {
                format: AssetFormat::Mp3,
                sample_rate: 44_100,
                channels: 1,
                frames: None,
            }),
        );
        with_words.set_trigger_words(vec!["ハクシュ".to_string(), "カンパイ".to_string()]);
        let assets = [
            asset("bgm-0001", "first", "first.wav", Some(info())),
            asset("bgm-0002", "second", "album/second.mp3", None),
            with_words,
            asset("se-12345", "big", "big.wav", Some(info())),
        ];
        let text = to_toml(12_346, &assets).unwrap();
        let parsed = parse_ok(&text);

        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(parsed.next_id, 12_346);
        assert_eq!(parsed.entries.len(), 4);
        for (entry, asset) in parsed.entries.iter().zip(&assets) {
            assert_eq!(entry.id, *asset.id());
            assert_eq!(entry.name, asset.name());
            assert_eq!(entry.path, asset.relative_path());
            assert_eq!(entry.info.as_ref(), asset.info());
            assert_eq!(entry.trigger_words, asset.trigger_words());
        }
        // And written again, it is the same text.
        let again: Vec<Asset> = parsed
            .entries
            .iter()
            .map(|e| {
                let mut a = Asset::new(
                    e.id,
                    e.name.clone(),
                    e.path.clone(),
                    e.info,
                    AssetState::Available,
                );
                a.set_trigger_words(e.trigger_words.clone());
                a
            })
            .collect();
        assert_eq!(to_toml(parsed.next_id, &again).unwrap(), text);
    }

    // -- a file that cannot be opened -------------------------------------------

    #[test]
    fn a_text_that_is_not_toml_is_a_parse_error() {
        for text in [
            "broken",
            "version = ",
            "version = 1\n[[asset]\n",
            "= 1",
            "\u{0}",
        ] {
            assert!(
                matches!(parse(text), Err(IndexError::Parse(_))),
                "{text:?}: {:?}",
                parse(text)
            );
        }
    }

    #[test]
    fn a_file_with_no_version_is_refused() {
        for text in ["", "next_id = 3\n", "[[asset]]\nid = \"se-0001\"\n"] {
            assert_eq!(parse(text), Err(IndexError::MissingVersion), "{text:?}");
        }
    }

    #[test]
    fn a_version_that_is_not_a_number_is_a_parse_error() {
        for text in [
            "version = \"1\"\n",
            "version = 1.0\n",
            "version = true\n",
            "version = [1]\n",
        ] {
            assert!(matches!(parse(text), Err(IndexError::Parse(_))), "{text:?}");
        }
    }

    #[test]
    fn a_newer_version_is_refused_before_anything_else_is_read() {
        // The rest of the file is of a form that this build does not know.
        assert_eq!(
            parse("version = 2\nnext_id = \"x\"\nasset = 5\n"),
            Err(IndexError::NewerVersion(2))
        );
        assert_eq!(parse("version = 99\n"), Err(IndexError::NewerVersion(99)));
        assert_eq!(
            parse("version = 4294967296\n"),
            Err(IndexError::NewerVersion(u32::MAX))
        );
    }

    #[test]
    fn a_version_that_never_existed_is_not_supported() {
        assert_eq!(
            parse("version = 0\n"),
            Err(IndexError::UnsupportedVersion(0))
        );
        assert_eq!(
            parse("version = -1\n"),
            Err(IndexError::UnsupportedVersion(-1))
        );
    }

    #[test]
    fn the_next_id_and_the_assets_must_have_the_right_type() {
        for text in [
            "version = 1\nnext_id = \"13\"\n",
            "version = 1\nnext_id = 1.5\n",
            "version = 1\nasset = 5\n",
            "version = 1\n[asset]\nid = \"se-0001\"\n",
        ] {
            assert!(matches!(parse(text), Err(IndexError::Parse(_))), "{text:?}");
        }
    }

    #[test]
    fn a_list_of_things_that_are_not_entries_is_left_out_one_by_one() {
        let parsed = parse_ok("version = 1\nnext_id = 1\nasset = [1, \"two\"]\n");
        assert!(parsed.entries.is_empty());
        let positions: Vec<usize> = parsed
            .warnings
            .iter()
            .filter_map(|w| match w {
                IndexWarning::BadEntry { position, .. } => Some(*position),
                _ => None,
            })
            .collect();
        assert_eq!(positions, [1, 2]);
    }

    #[test]
    fn the_errors_say_what_is_wrong_in_a_line() {
        assert_eq!(
            IndexError::MissingVersion.to_string(),
            "invalid library index: it has no `version`"
        );
        assert!(
            IndexError::NewerVersion(2)
                .to_string()
                .contains("written by a newer Ink Mixer")
        );
        assert!(
            IndexError::Parse("x".into())
                .to_string()
                .starts_with("invalid library index")
        );
    }

    // -- paths that could lead out of the folder -----------------------------------

    #[test]
    fn safe_paths_are_accepted() {
        for path in [
            "a.wav",
            "claps/拍手.wav",
            "album one/track 01.mp3",
            "a/b/c/d.wav",
            "音 声.mp3",
            "with-dash_and.dots.in.name.wav",
            "console.wav",
            "con-sole/x.wav",
            "COM10.wav",
            "COM0.wav",
            "lpt.wav",
        ] {
            assert_eq!(path_problem(path), None, "{path:?}");
        }
    }

    #[test]
    fn paths_that_are_not_safe_are_refused_and_say_why() {
        let cases: [(&str, PathProblem); 26] = [
            ("", PathProblem::Empty),
            ("/etc/passwd", PathProblem::Absolute),
            ("/a.wav", PathProblem::Absolute),
            ("a\\b.wav", PathProblem::ForbiddenCharacter('\\')),
            (
                "\\\\server\\share\\a.wav",
                PathProblem::ForbiddenCharacter('\\'),
            ),
            ("C:/a.wav", PathProblem::ForbiddenCharacter(':')),
            ("a:b.wav", PathProblem::ForbiddenCharacter(':')),
            ("a?.wav", PathProblem::ForbiddenCharacter('?')),
            ("a*.wav", PathProblem::ForbiddenCharacter('*')),
            ("a|b.wav", PathProblem::ForbiddenCharacter('|')),
            ("a<b.wav", PathProblem::ForbiddenCharacter('<')),
            ("a\"b.wav", PathProblem::ForbiddenCharacter('"')),
            ("a\nb.wav", PathProblem::ForbiddenCharacter('\n')),
            ("a//b.wav", PathProblem::EmptyPart),
            ("a/", PathProblem::EmptyPart),
            ("../a.wav", PathProblem::StartsWithDot),
            ("a/../b.wav", PathProblem::StartsWithDot),
            ("./a.wav", PathProblem::StartsWithDot),
            ("a/./b.wav", PathProblem::StartsWithDot),
            (".hidden.wav", PathProblem::StartsWithDot),
            ("album/.hidden/a.wav", PathProblem::StartsWithDot),
            ("a.", PathProblem::EndsWithDotOrSpace),
            ("dir /a.wav", PathProblem::EndsWithDotOrSpace),
            ("CON", PathProblem::ReservedName),
            ("album/nul.wav", PathProblem::ReservedName),
            ("Com1.tar.gz", PathProblem::ReservedName),
        ];
        for (path, expected) in cases {
            assert_eq!(path_problem(path), Some(expected), "{path:?}");
        }
        assert_eq!(path_problem("LPT9.mp3"), Some(PathProblem::ReservedName));
        assert_eq!(path_problem("aux"), Some(PathProblem::ReservedName));
        assert_eq!(path_problem("prn.txt"), Some(PathProblem::ReservedName));
    }

    #[test]
    fn an_entry_with_an_unsafe_path_is_left_out_and_the_others_are_kept() {
        let text = "\
version = 1
next_id = 100

[[asset]]
id = \"bgm-0001\"
kind = \"bgm\"
path = \"good.wav\"

[[asset]]
id = \"bgm-0002\"
kind = \"bgm\"
path = \"../../etc/passwd\"

[[asset]]
id = \"bgm-0003\"
kind = \"bgm\"
path = \"/etc/passwd\"

[[asset]]
id = \"bgm-0004\"
kind = \"bgm\"
path = \"C:\\\\Windows\\\\a.wav\"

[[asset]]
id = \"bgm-0005\"
kind = \"bgm\"
path = \"\"

[[asset]]
id = \"bgm-0006\"
kind = \"bgm\"
path = \"also-good.mp3\"
";
        let parsed = parse_ok(text);
        let kept: Vec<String> = parsed.entries.iter().map(|e| e.id.to_string()).collect();
        assert_eq!(kept, ["bgm-0001", "bgm-0006"]);
        let problems: Vec<(String, PathProblem)> = parsed
            .warnings
            .iter()
            .filter_map(|w| match w {
                IndexWarning::UnsafePath { id, problem, .. } => Some((id.to_string(), *problem)),
                _ => None,
            })
            .collect();
        assert_eq!(
            problems,
            [
                ("bgm-0002".to_string(), PathProblem::StartsWithDot),
                ("bgm-0003".to_string(), PathProblem::Absolute),
                ("bgm-0004".to_string(), PathProblem::ForbiddenCharacter(':')),
                ("bgm-0005".to_string(), PathProblem::Empty),
            ]
        );
    }

    // -- entries that cannot be used --------------------------------------------

    #[test]
    fn an_id_that_is_used_twice_is_left_out_the_second_time() {
        let text = "\
version = 1
next_id = 100

[[asset]]
id = \"se-0007\"
kind = \"se\"
path = \"first.wav\"

[[asset]]
id = \"se-0007\"
kind = \"se\"
path = \"second.wav\"
";
        let parsed = parse_ok(text);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].path, "first.wav");
        assert_eq!(
            parsed.warnings,
            [IndexWarning::DuplicateId { id: id("se-0007") }]
        );
    }

    #[test]
    fn an_entry_whose_kind_is_not_the_one_of_its_id_is_left_out() {
        let parsed = parse_ok(&entry_text(
            "id = \"bgm-0005\"\nkind = \"se\"\npath = \"a.wav\"\n",
        ));
        assert!(parsed.entries.is_empty());
        assert_eq!(
            parsed.warnings,
            [IndexWarning::KindMismatch {
                id: id("bgm-0005"),
                kind: AssetKind::Se
            }]
        );
    }

    #[test]
    fn the_same_path_twice_is_left_out_without_regard_to_the_case() {
        let text = "\
version = 1
next_id = 100

[[asset]]
id = \"se-0001\"
kind = \"se\"
path = \"Claps/Applause.WAV\"

[[asset]]
id = \"se-0002\"
kind = \"se\"
path = \"claps/applause.wav\"

[[asset]]
id = \"se-0003\"
kind = \"se\"
path = \"Claps/Applause.WAV\"

[[asset]]
id = \"bgm-0004\"
kind = \"bgm\"
path = \"claps/applause.wav\"
";
        let parsed = parse_ok(text);
        let kept: Vec<String> = parsed.entries.iter().map(|e| e.id.to_string()).collect();
        // The same path in the folder of the other kind is another file.
        assert_eq!(kept, ["se-0001", "bgm-0004"]);
        assert_eq!(
            parsed.warnings,
            [
                IndexWarning::DuplicatePath {
                    id: id("se-0002"),
                    path: "claps/applause.wav".to_string(),
                    first: id("se-0001"),
                },
                IndexWarning::DuplicatePath {
                    id: id("se-0003"),
                    path: "Claps/Applause.WAV".to_string(),
                    first: id("se-0001"),
                },
            ]
        );
    }

    #[test]
    fn an_entry_with_a_bad_id_or_a_missing_item_is_left_out_with_its_position() {
        let text = "\
version = 1
next_id = 100

[[asset]]
id = \"se-0001\"
kind = \"se\"
path = \"ok.wav\"

[[asset]]
id = \"bgm-5\"
kind = \"bgm\"
path = \"bad-id.wav\"

[[asset]]
kind = \"bgm\"
path = \"no-id.wav\"

[[asset]]
id = \"bgm-0006\"
path = \"no-kind.wav\"

[[asset]]
id = \"bgm-0007\"
kind = \"bgm\"

[[asset]]
id = \"bgm-0008\"
kind = \"video\"
path = \"x.wav\"

[[asset]]
id = 9
kind = \"bgm\"
path = \"number.wav\"
";
        let parsed = parse_ok(text);
        assert_eq!(parsed.entries.len(), 1);
        let positions: Vec<usize> = parsed
            .warnings
            .iter()
            .filter_map(|w| match w {
                IndexWarning::BadEntry { position, .. } | IndexWarning::BadId { position, .. } => {
                    Some(*position)
                }
                _ => None,
            })
            .collect();
        assert_eq!(positions, [2, 3, 4, 5, 6, 7]);
        assert!(matches!(
            &parsed.warnings[0],
            IndexWarning::BadId { text, reason: AssetIdError::NotCanonical, .. } if text == "bgm-5"
        ));
    }

    // -- the next ID --------------------------------------------------------------

    #[test]
    fn a_next_id_that_is_not_larger_than_every_id_is_raised() {
        let fields = "id = \"se-0020\"\nkind = \"se\"\npath = \"a.wav\"\n";
        for declared in ["13", "20", "0", "-5"] {
            let text = format!("version = 1\nnext_id = {declared}\n\n[[asset]]\n{fields}");
            let parsed = parse_ok(&text);
            assert_eq!(parsed.next_id, 21, "next_id = {declared}");
            assert_eq!(parsed.entries.len(), 1);
            assert_eq!(
                parsed.warnings,
                [IndexWarning::NextIdRaised {
                    from: Some(declared.parse().unwrap()),
                    to: 21
                }]
            );
        }
    }

    #[test]
    fn a_next_id_that_is_large_enough_is_kept_without_a_warning() {
        let parsed = parse_ok(&entry_text(
            "id = \"se-0020\"\nkind = \"se\"\npath = \"a.wav\"\n",
        ));
        assert_eq!(parsed.next_id, 100);
        assert!(parsed.warnings.is_empty());
        // A gap is fine: the numbers that were given are not given again.
        let parsed = parse_ok("version = 1\nnext_id = 5000\n");
        assert_eq!(parsed.next_id, 5000);
    }

    #[test]
    fn the_ids_of_the_entries_that_were_left_out_count_too() {
        // se-0050 has an unsafe path; its number was given once, so the next
        // ID is after it.
        let text = "\
version = 1
next_id = 3

[[asset]]
id = \"se-0001\"
kind = \"se\"
path = \"ok.wav\"

[[asset]]
id = \"se-0050\"
kind = \"se\"
path = \"../x.wav\"

[[asset]]
id = \"bgm-0060\"
kind = \"se\"
path = \"mismatch.wav\"
";
        let parsed = parse_ok(text);
        assert_eq!(parsed.next_id, 61);
        assert_eq!(parsed.entries.len(), 1);
    }

    #[test]
    fn a_file_with_no_next_id_gets_one() {
        let parsed = parse_ok("version = 1\n");
        assert_eq!(parsed.next_id, 1);
        assert!(parsed.warnings.is_empty());

        let text =
            "version = 1\n\n[[asset]]\nid = \"bgm-0004\"\nkind = \"bgm\"\npath = \"a.wav\"\n";
        let parsed = parse_ok(text);
        assert_eq!(parsed.next_id, 5);
        assert_eq!(
            parsed.warnings,
            [IndexWarning::NextIdRaised { from: None, to: 5 }]
        );
    }

    #[test]
    fn the_largest_id_does_not_make_the_next_id_overflow() {
        let text = format!(
            "version = 1\nnext_id = 1\n\n[[asset]]\nid = \"se-{}\"\nkind = \"se\"\npath = \"a.wav\"\n",
            u64::MAX
        );
        let parsed = parse_ok(&text);
        assert_eq!(parsed.next_id, u64::MAX);
    }

    // -- the information, the trigger words, the name ------------------------------

    #[test]
    fn an_entry_without_information_has_none() {
        let parsed = parse_ok(&entry_text(
            "id = \"bgm-0001\"\nkind = \"bgm\"\npath = \"a.wav\"\n",
        ));
        assert_eq!(parsed.entries[0].info, None);
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn an_entry_without_trigger_words_has_none() {
        let parsed = parse_ok(&entry_text(
            "id = \"se-0001\"\nkind = \"se\"\npath = \"a.wav\"\n",
        ));
        assert!(parsed.entries[0].trigger_words.is_empty());
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn the_trigger_words_are_read_as_they_are() {
        let fields = "id = \"se-0001\"\nkind = \"se\"\npath = \"a.wav\"\ntrigger_words = [\"ハクシュ\", \"abc\", \"\"]\n";
        let parsed = parse_ok(&entry_text(fields));
        assert_eq!(parsed.entries[0].trigger_words, ["ハクシュ", "abc", ""]);
    }

    #[test]
    fn information_that_cannot_be_read_is_ignored_and_the_entry_is_kept() {
        let fields = "id = \"bgm-0001\"\nkind = \"bgm\"\npath = \"a.wav\"\n\n[asset.info]\nformat = \"ogg\"\nsample_rate = 48000\nchannels = 2\n";
        let parsed = parse_ok(&entry_text(fields));
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].info, None);
        assert!(matches!(
            parsed.warnings.as_slice(),
            [IndexWarning::BadInfo { id: warned, .. }] if *warned == id("bgm-0001")
        ));
    }

    #[test]
    fn trigger_words_that_are_not_texts_are_ignored_and_the_entry_is_kept() {
        for words in ["5", "\"ハクシュ\"", "[1, 2]", "[\"a\", 2]"] {
            let fields = format!(
                "id = \"se-0001\"\nkind = \"se\"\npath = \"a.wav\"\ntrigger_words = {words}\n"
            );
            let parsed = parse_ok(&entry_text(&fields));
            assert_eq!(parsed.entries.len(), 1, "{words}");
            assert!(parsed.entries[0].trigger_words.is_empty(), "{words}");
            assert_eq!(
                parsed.warnings,
                [IndexWarning::BadTriggerWords { id: id("se-0001") }],
                "{words}"
            );
        }
    }

    #[test]
    fn the_name_defaults_to_the_file_name_without_its_extension() {
        assert_eq!(default_name("claps/拍手.wav"), "拍手");
        assert_eq!(default_name("a.b.mp3"), "a.b");
        assert_eq!(default_name("noext"), "noext");
        assert_eq!(default_name("dir/noext"), "noext");
        assert_eq!(default_name("a b/c d.wav"), "c d");
        let parsed = parse_ok(&entry_text(
            "id = \"se-0001\"\nkind = \"se\"\npath = \"claps/拍手.wav\"\n",
        ));
        assert_eq!(parsed.entries[0].name, "拍手");
        let named = parse_ok(&entry_text(
            "id = \"se-0001\"\nkind = \"se\"\nname = \"Applause\"\npath = \"claps/拍手.wav\"\n",
        ));
        assert_eq!(named.entries[0].name, "Applause");
    }

    #[test]
    fn items_that_this_build_does_not_know_are_ignored() {
        let text = "\
version = 1
next_id = 3
future_item = true

[[asset]]
id = \"se-0001\"
kind = \"se\"
path = \"a.wav\"
colour = \"red\"
state = \"missing\"
";
        let parsed = parse_ok(text);
        assert_eq!(parsed.entries.len(), 1);
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn the_entries_keep_the_order_of_the_file() {
        let text = "\
version = 1
next_id = 10

[[asset]]
id = \"se-0003\"
kind = \"se\"
path = \"c.wav\"

[[asset]]
id = \"bgm-0001\"
kind = \"bgm\"
path = \"a.wav\"

[[asset]]
id = \"se-0002\"
kind = \"se\"
path = \"b.wav\"
";
        let ids: Vec<String> = parse_ok(text)
            .entries
            .iter()
            .map(|e| e.id.to_string())
            .collect();
        assert_eq!(ids, ["se-0003", "bgm-0001", "se-0002"]);
    }

    // -- the warnings say what happened ---------------------------------------------

    #[test]
    fn a_warning_says_which_entry_and_why_in_a_line() {
        assert_eq!(
            IndexWarning::UnsafePath {
                id: id("se-0002"),
                path: "../x.wav".to_string(),
                problem: PathProblem::StartsWithDot
            }
            .to_string(),
            "se-0002 is left out: the path \"../x.wav\" is not safe (a part of the path starts with `.`)"
        );
        assert_eq!(
            IndexWarning::DuplicateId { id: id("se-0007") }.to_string(),
            "se-0007 is left out: the ID is used twice"
        );
        assert_eq!(
            IndexWarning::NextIdRaised {
                from: Some(13),
                to: 21
            }
            .to_string(),
            "`next_id` 13 is raised to 21"
        );
        assert_eq!(
            IndexWarning::NextIdRaised { from: None, to: 5 }.to_string(),
            "the `next_id` is set to 5"
        );
    }
}
