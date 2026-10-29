//! The model of the sound library (ADR-0010): what an Asset is.
//!
//! An Asset is a BGM or a sound effect (SE) file in the sound folders of Ink
//! Mixer. The settings keep its [`AssetId`], never its path (AGENTS.md §4, 6).
//!
//! This module only holds the types. It does not read files and it does not
//! know about codecs: the information of a file ([`AssetInfo`]) is given by
//! the caller, who reads it with `ink-media`.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The longest sound effect (SE) that is taken in: 60 seconds. A longer sound
/// is for the BGM (ADR-0009).
pub const SE_MAX_DURATION: Duration = Duration::from_secs(60);

/// The kind of an Asset. It is decided by the folder the file is in (`bgm/`
/// or `se/`): a file moved to the other folder is another Asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    /// A song (any length).
    Bgm,
    /// A sound effect (up to [`SE_MAX_DURATION`]).
    Se,
}

impl AssetKind {
    /// Both kinds, in the order of the folders.
    pub const ALL: [AssetKind; 2] = [AssetKind::Bgm, AssetKind::Se];

    /// Returns the name of the folder of this kind (`"bgm"` or `"se"`).
    pub fn folder_name(self) -> &'static str {
        match self {
            Self::Bgm => "bgm",
            Self::Se => "se",
        }
    }

    /// Returns the start of the IDs of this kind (the same as the name of the
    /// folder).
    fn id_prefix(self) -> &'static str {
        self.folder_name()
    }

    fn from_id_prefix(prefix: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.id_prefix() == prefix)
    }
}

/// The ID of an Asset: `"bgm-0005"`, `"se-0012"`.
///
/// The kind, a `-`, and a number written with at least 4 digits (`0005`, and
/// `12345` when it is longer). The number starts at 1. There is only one way
/// to write an ID, so two texts are the same ID if and only if they are the
/// same text.
///
/// IDs are made by the library, which never uses a number twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AssetId {
    kind: AssetKind,
    number: u64,
}

/// Why a text is not an [`AssetId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AssetIdError {
    /// The text is empty.
    Empty,
    /// There is no `-` between the kind and the number.
    Malformed,
    /// The text does not start with `bgm` or `se` (in small letters).
    UnknownKind,
    /// The number is missing, or has something other than the digits 0 to 9
    /// (a sign, a space, a full width digit).
    NotANumber,
    /// The number does not fit in 64 bits.
    TooLarge,
    /// The number is 0. IDs start at 1.
    Zero,
    /// The number is not written the one way: it has fewer than 4 digits
    /// (`bgm-5`), or a 0 in front that is not needed (`bgm-00005`).
    NotCanonical,
}

impl fmt::Display for AssetIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the ID is empty",
            Self::Malformed => "the ID must be a kind, a `-`, and a number (for example `se-0012`)",
            Self::UnknownKind => "the ID must start with `bgm` or `se`",
            Self::NotANumber => "the number of the ID must be the digits 0 to 9 only",
            Self::TooLarge => "the number of the ID is too large",
            Self::Zero => "the number of the ID must be 1 or more",
            Self::NotCanonical => {
                "the number of the ID must have at least 4 digits and no needless 0 in front"
            }
        })
    }
}

impl std::error::Error for AssetIdError {}

impl AssetId {
    /// Makes the ID of `kind` with `number`.
    ///
    /// # Errors
    ///
    /// Returns [`AssetIdError::Zero`] if `number` is 0.
    pub fn new(kind: AssetKind, number: u64) -> Result<Self, AssetIdError> {
        if number == 0 {
            return Err(AssetIdError::Zero);
        }
        Ok(Self { kind, number })
    }

    /// Returns the kind written in the ID.
    pub fn kind(&self) -> AssetKind {
        self.kind
    }

    /// Returns the number of the ID.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// Returns whether the ID is the ID of an Asset of `kind`: the start of
    /// the ID must agree with the kind that is written beside it in the index.
    pub fn is_of_kind(&self, kind: AssetKind) -> bool {
        self.kind == kind
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{:04}", self.kind.id_prefix(), self.number)
    }
}

impl FromStr for AssetId {
    type Err = AssetIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            return Err(AssetIdError::Empty);
        }
        let (prefix, digits) = text.split_once('-').ok_or(AssetIdError::Malformed)?;
        let kind = AssetKind::from_id_prefix(prefix).ok_or(AssetIdError::UnknownKind)?;
        // Only the ASCII digits: `u64::from_str` would also take a `+`.
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AssetIdError::NotANumber);
        }
        let number: u64 = digits.parse().map_err(|_| AssetIdError::TooLarge)?;
        let id = Self::new(kind, number)?;
        // One way to write it: 4 digits at least, and no 0 that is not needed.
        if id.to_string() != text {
            return Err(AssetIdError::NotCanonical);
        }
        Ok(id)
    }
}

impl Serialize for AssetId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AssetId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse()
            .map_err(|e| D::Error::custom(format!("invalid asset ID {text:?}: {e}")))
    }
}

/// The kind of file of an Asset (Phase 2: WAV and MP3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetFormat {
    Wav,
    Mp3,
}

/// What the file says about its sound. The caller reads it (with
/// `ink-media`) and gives it to the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetInfo {
    pub format: AssetFormat,
    /// The sample rate of the file in Hz.
    pub sample_rate: u32,
    /// The number of channels of the file.
    pub channels: u16,
    /// The number of frames, if the file says it. For an MP3 file with the
    /// encoder's delay in its header, it is the sound that is played.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
}

impl AssetInfo {
    /// Returns the length of the sound, if the file says its number of
    /// frames (and its sample rate is not 0).
    pub fn duration(&self) -> Option<Duration> {
        let frames = self.frames?;
        if self.sample_rate == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(
            frames as f64 / f64::from(self.sample_rate),
        ))
    }
}

/// Whether the file of an Asset can be used. It is not saved: the library
/// finds it out when it opens and when it scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetState {
    /// The file is there and can be read.
    Available,
    /// The file is not there. The Asset stays in the index.
    Missing,
    /// The file is there and cannot be read.
    Unreadable,
}

/// Why the information of a file could not be read. The caller's function
/// gives it to the library (`ink-media` makes it from its own errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFailure {
    /// The file is not a sound this build can play (not WAV or MP3, or
    /// broken so that it cannot be told).
    NotAudio(String),
    /// The file could not be read (permission, I/O).
    Unreadable(String),
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAudio(reason) => write!(f, "not a sound that can be played: {reason}"),
            Self::Unreadable(reason) => write!(f, "cannot be read: {reason}"),
        }
    }
}

impl std::error::Error for ProbeFailure {}

/// A sound in the library.
///
/// The fields are private: read them through the accessors. Only the library
/// changes an Asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    id: AssetId,
    name: String,
    /// The path from the folder of the kind, with `/` between the parts.
    path: String,
    info: Option<AssetInfo>,
    trigger_words: Vec<String>,
    state: AssetState,
}

impl Asset {
    /// Makes an Asset with no trigger words. The caller checks that `path` is
    /// a safe relative path (no `..`, not absolute).
    pub(crate) fn new(
        id: AssetId,
        name: String,
        path: String,
        info: Option<AssetInfo>,
        state: AssetState,
    ) -> Self {
        Self {
            id,
            name,
            path,
            info,
            trigger_words: Vec::new(),
            state,
        }
    }

    /// Returns the ID.
    pub fn id(&self) -> &AssetId {
        &self.id
    }

    /// Returns the kind: the one written in the ID.
    pub fn kind(&self) -> AssetKind {
        self.id.kind()
    }

    /// Returns the name to show (at first, the name of the file without its
    /// extension).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the path of the file from the folder of its kind, with `/`
    /// between the parts. The library turns it into a path of the disk
    /// (`Library::path_of`); it is never saved as an absolute path.
    pub fn relative_path(&self) -> &str {
        &self.path
    }

    /// Returns what the file says about its sound. It is `None` for a file
    /// that has never been read: show no length. For a file that was read
    /// once, it is what was read the last time, also when the file is missing
    /// or cannot be read now.
    pub fn info(&self) -> Option<&AssetInfo> {
        self.info.as_ref()
    }

    /// Returns the trigger words (the words that play this sound when they are
    /// heard). Empty at first. Their rules are not checked here.
    pub fn trigger_words(&self) -> &[String] {
        &self.trigger_words
    }

    /// Returns whether the file can be used.
    pub fn state(&self) -> AssetState {
        self.state
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "no operation of Issue 61 renames an Asset; kept for the import page (Issue 65)"
        )
    )]
    pub(crate) fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub(crate) fn set_info(&mut self, info: Option<AssetInfo>) {
        self.info = info;
    }

    pub(crate) fn set_trigger_words(&mut self, words: Vec<String>) {
        self.trigger_words = words;
    }

    pub(crate) fn set_state(&mut self, state: AssetState) {
        self.state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(kind: AssetKind, number: u64) -> AssetId {
        AssetId::new(kind, number).unwrap()
    }

    fn info() -> AssetInfo {
        AssetInfo {
            format: AssetFormat::Wav,
            sample_rate: 48_000,
            channels: 2,
            frames: Some(96_000),
        }
    }

    // -- constants and kinds ------------------------------------------------

    #[test]
    fn a_sound_effect_is_at_most_60_seconds() {
        assert_eq!(SE_MAX_DURATION, Duration::from_secs(60));
    }

    #[test]
    fn the_kinds_have_the_names_of_their_folders() {
        assert_eq!(AssetKind::Bgm.folder_name(), "bgm");
        assert_eq!(AssetKind::Se.folder_name(), "se");
        assert_eq!(AssetKind::ALL, [AssetKind::Bgm, AssetKind::Se]);
        assert!(AssetKind::Bgm < AssetKind::Se);
    }

    // -- the ID: writing it -------------------------------------------------

    #[test]
    fn an_id_is_written_with_at_least_4_digits() {
        assert_eq!(id(AssetKind::Bgm, 5).to_string(), "bgm-0005");
        assert_eq!(id(AssetKind::Se, 12).to_string(), "se-0012");
        assert_eq!(id(AssetKind::Se, 1).to_string(), "se-0001");
        assert_eq!(id(AssetKind::Bgm, 9_999).to_string(), "bgm-9999");
        assert_eq!(id(AssetKind::Bgm, 10_000).to_string(), "bgm-10000");
        assert_eq!(
            id(AssetKind::Se, u64::MAX).to_string(),
            "se-18446744073709551615"
        );
    }

    #[test]
    fn the_number_of_an_id_starts_at_1() {
        assert_eq!(
            AssetId::new(AssetKind::Bgm, 0).unwrap_err(),
            AssetIdError::Zero
        );
        assert_eq!(id(AssetKind::Bgm, 1).number(), 1);
    }

    // -- the ID: reading it -------------------------------------------------

    #[test]
    fn the_ids_that_are_written_the_one_way_are_read() {
        for text in [
            "bgm-0005",
            "se-0012",
            "se-0001",
            "bgm-9999",
            "bgm-10000",
            "se-123456",
            "se-18446744073709551615",
        ] {
            let parsed: AssetId = text.parse().unwrap_or_else(|e| panic!("{text}: {e}"));
            // And back to the same text.
            assert_eq!(parsed.to_string(), text);
        }
        let parsed: AssetId = "se-0012".parse().unwrap();
        assert_eq!(parsed.kind(), AssetKind::Se);
        assert_eq!(parsed.number(), 12);
    }

    #[test]
    fn an_id_that_is_not_a_kind_a_dash_and_a_number_is_refused() {
        let cases: [(&str, AssetIdError); 28] = [
            // Empty and without a dash.
            ("", AssetIdError::Empty),
            ("bgm", AssetIdError::Malformed),
            ("bgm0005", AssetIdError::Malformed),
            ("0005", AssetIdError::Malformed),
            // The kind.
            ("-0005", AssetIdError::UnknownKind),
            ("sfx-0005", AssetIdError::UnknownKind),
            ("BGM-0005", AssetIdError::UnknownKind),
            ("Se-0005", AssetIdError::UnknownKind),
            (" bgm-0005", AssetIdError::UnknownKind),
            ("bgm -0005", AssetIdError::UnknownKind),
            ("ｓｅ-0005", AssetIdError::UnknownKind),
            // No number, or not only the digits 0 to 9.
            ("bgm-", AssetIdError::NotANumber),
            ("bgm-+005", AssetIdError::NotANumber),
            ("bgm-+0005", AssetIdError::NotANumber),
            ("bgm--005", AssetIdError::NotANumber),
            ("bgm- 0005", AssetIdError::NotANumber),
            ("bgm-0005 ", AssetIdError::NotANumber),
            ("bgm-0005\n", AssetIdError::NotANumber),
            ("bgm-0005-1", AssetIdError::NotANumber),
            ("bgm-0x05", AssetIdError::NotANumber),
            ("bgm-00.5", AssetIdError::NotANumber),
            ("bgm-００５", AssetIdError::NotANumber),
            ("bgm-０００５", AssetIdError::NotANumber),
            ("bgm-٠٠٠٥", AssetIdError::NotANumber),
            // Too large and zero.
            ("bgm-18446744073709551616", AssetIdError::TooLarge),
            ("bgm-0000", AssetIdError::Zero),
            ("bgm-00000", AssetIdError::Zero),
            ("se-0", AssetIdError::Zero),
        ];
        for (text, expected) in cases {
            assert_eq!(text.parse::<AssetId>(), Err(expected), "{text:?}");
        }
    }

    #[test]
    fn a_number_that_is_not_written_the_one_way_is_refused() {
        for text in [
            "bgm-5",
            "bgm-05",
            "bgm-005",
            "bgm-00005",
            "se-000012",
            "se-012345",
            "se-1",
        ] {
            assert_eq!(
                text.parse::<AssetId>(),
                Err(AssetIdError::NotCanonical),
                "{text:?}"
            );
        }
    }

    #[test]
    fn two_texts_are_never_the_same_id() {
        // The only text of the id 5 is "bgm-0005".
        let texts = ["bgm-0005", "bgm-05", "bgm-00005", "bgm-5"];
        let accepted: Vec<_> = texts
            .iter()
            .filter(|text| text.parse::<AssetId>().is_ok())
            .collect();
        assert_eq!(accepted, [&"bgm-0005"]);
    }

    // -- the ID: the kind ---------------------------------------------------

    #[test]
    fn the_start_of_an_id_must_agree_with_the_kind() {
        let bgm = id(AssetKind::Bgm, 5);
        assert!(bgm.is_of_kind(AssetKind::Bgm));
        assert!(!bgm.is_of_kind(AssetKind::Se));
        let se: AssetId = "se-0005".parse().unwrap();
        assert!(se.is_of_kind(AssetKind::Se));
        assert!(!se.is_of_kind(AssetKind::Bgm));
        assert_ne!(bgm, se, "the same number of another kind is another ID");
    }

    #[test]
    fn ids_are_ordered_by_kind_and_then_by_number() {
        let mut ids = [
            id(AssetKind::Se, 2),
            id(AssetKind::Bgm, 10),
            id(AssetKind::Se, 1),
            id(AssetKind::Bgm, 9),
        ];
        ids.sort();
        let texts: Vec<String> = ids.iter().map(ToString::to_string).collect();
        assert_eq!(texts, ["bgm-0009", "bgm-0010", "se-0001", "se-0002"]);
    }

    // -- the ID in a file ---------------------------------------------------

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Holder {
        id: AssetId,
        kind: AssetKind,
        format: AssetFormat,
    }

    #[test]
    fn an_id_a_kind_and_a_format_are_text_in_toml() {
        let holder = Holder {
            id: id(AssetKind::Se, 12),
            kind: AssetKind::Se,
            format: AssetFormat::Mp3,
        };
        let text = toml::to_string(&holder).unwrap();
        assert_eq!(text, "id = \"se-0012\"\nkind = \"se\"\nformat = \"mp3\"\n");
        assert_eq!(toml::from_str::<Holder>(&text).unwrap(), holder);
    }

    #[test]
    fn a_bad_id_in_a_file_is_an_error_that_says_why() {
        let error = toml::from_str::<Holder>("id = \"bgm-5\"\nkind = \"bgm\"\nformat = \"wav\"\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid asset ID \"bgm-5\""), "{error}");
        assert!(error.contains("at least 4 digits"), "{error}");
        // An ID that is a number and not a text.
        assert!(toml::from_str::<Holder>("id = 5\nkind = \"bgm\"\nformat = \"wav\"\n").is_err());
        // An unknown kind and format.
        assert!(
            toml::from_str::<Holder>("id = \"bgm-0005\"\nkind = \"x\"\nformat = \"wav\"\n")
                .is_err()
        );
        assert!(
            toml::from_str::<Holder>("id = \"bgm-0005\"\nkind = \"bgm\"\nformat = \"ogg\"\n")
                .is_err()
        );
    }

    // -- the information of a file ------------------------------------------

    #[test]
    fn the_length_is_the_frames_over_the_sample_rate() {
        assert_eq!(info().duration(), Some(Duration::from_secs(2)));
        let half = AssetInfo {
            frames: Some(24_000),
            ..info()
        };
        assert_eq!(half.duration(), Some(Duration::from_millis(500)));
    }

    #[test]
    fn a_file_that_does_not_say_its_frames_has_no_length() {
        let unknown = AssetInfo {
            frames: None,
            ..info()
        };
        assert_eq!(unknown.duration(), None);
        let no_rate = AssetInfo {
            sample_rate: 0,
            ..info()
        };
        assert_eq!(no_rate.duration(), None, "no division by 0");
    }

    #[test]
    fn the_information_is_written_without_the_frames_that_are_not_known() {
        let text = toml::to_string(&info()).unwrap();
        assert_eq!(
            text,
            "format = \"wav\"\nsample_rate = 48000\nchannels = 2\nframes = 96000\n"
        );
        assert_eq!(toml::from_str::<AssetInfo>(&text).unwrap(), info());

        let unknown = AssetInfo {
            format: AssetFormat::Mp3,
            frames: None,
            ..info()
        };
        let text = toml::to_string(&unknown).unwrap();
        assert!(!text.contains("frames"), "{text}");
        assert_eq!(toml::from_str::<AssetInfo>(&text).unwrap(), unknown);
    }

    // -- the Asset ----------------------------------------------------------

    fn asset() -> Asset {
        Asset::new(
            id(AssetKind::Se, 12),
            "拍手".to_string(),
            "claps/拍手.wav".to_string(),
            Some(info()),
            AssetState::Available,
        )
    }

    #[test]
    fn an_asset_is_read_through_its_accessors() {
        let asset = asset();
        assert_eq!(asset.id().to_string(), "se-0012");
        assert_eq!(asset.kind(), AssetKind::Se, "the kind is the one of the ID");
        assert_eq!(asset.name(), "拍手");
        assert_eq!(asset.relative_path(), "claps/拍手.wav");
        assert_eq!(asset.info(), Some(&info()));
        assert!(asset.trigger_words().is_empty());
        assert_eq!(asset.state(), AssetState::Available);
    }

    #[test]
    fn an_asset_that_was_never_read_has_no_information() {
        let asset = Asset::new(
            id(AssetKind::Bgm, 1),
            "broken".to_string(),
            "broken.wav".to_string(),
            None,
            AssetState::Unreadable,
        );
        assert_eq!(asset.info(), None);
        assert_eq!(asset.state(), AssetState::Unreadable);
    }

    #[test]
    fn the_library_can_change_what_changes_and_nothing_else() {
        let mut asset = asset();
        asset.set_state(AssetState::Missing);
        asset.set_info(None);
        asset.set_name("Applause".to_string());
        asset.set_trigger_words(vec!["ハクシュ".to_string(), "カンパイ".to_string()]);
        assert_eq!(asset.state(), AssetState::Missing);
        assert_eq!(asset.info(), None);
        assert_eq!(asset.name(), "Applause");
        assert_eq!(asset.trigger_words(), ["ハクシュ", "カンパイ"]);
        // The ID, the kind and the path are never changed.
        assert_eq!(asset.id().to_string(), "se-0012");
        assert_eq!(asset.relative_path(), "claps/拍手.wav");
    }

    // -- the failure of a probe ----------------------------------------------

    #[test]
    fn a_probe_failure_says_why_in_a_line() {
        assert_eq!(
            ProbeFailure::NotAudio("unsupported audio format".to_string()).to_string(),
            "not a sound that can be played: unsupported audio format"
        );
        assert_eq!(
            ProbeFailure::Unreadable("permission denied".to_string()).to_string(),
            "cannot be read: permission denied"
        );
    }

    #[test]
    fn the_types_can_move_to_another_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AssetId>();
        assert_send_sync::<Asset>();
        assert_send_sync::<AssetInfo>();
        assert_send_sync::<ProbeFailure>();
    }
}
