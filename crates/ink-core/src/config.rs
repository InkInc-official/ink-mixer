//! Application configuration, stored as TOML.
//!
//! This module only reads and writes a file at a given path. Choosing the
//! OS-specific location is left to the application.
//!
//! Loading and saving do file I/O: never call them from the realtime path.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_file::write_atomically;
use crate::graph::{
    BGM_VOLUME_RANGE, MASTER_VOLUME_RANGE, MIC_GAIN_RANGE, SE_VOLUME_RANGE, SePolicy,
};
use crate::{AssetId, AssetKind, DeviceId};

/// Version of the configuration format written by this build.
///
/// Adding an optional item (one that has a default when missing, such as
/// `[mixer]`) does not change the version: older files still load as they
/// are. Raise it when an item changes its name, meaning, or type, or when
/// an older form can no longer be read; then add the migration to the
/// `match version` in [`Config::from_toml`].
pub const CONFIG_VERSION: u32 = 1;

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Format version. Required in the file.
    pub version: u32,
    /// Audio device selection.
    #[serde(default)]
    pub audio: AudioConfig,
    /// Mixer levels and mute. Not written while nothing is saved.
    #[serde(default, skip_serializing_if = "MixerConfig::is_empty")]
    pub mixer: MixerConfig,
    /// The song of the BGM channel. Not written while nothing is saved.
    #[serde(default, skip_serializing_if = "BgmConfig::is_empty")]
    pub bgm: BgmConfig,
    /// The sounds of the Sound Pad. Not written while nothing is saved.
    #[serde(default, skip_serializing_if = "SoundPadConfig::is_empty")]
    pub sound_pad: SoundPadConfig,
}

/// Selected audio devices. `None` means "not selected" and is not written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_device: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_device: Option<DeviceId>,
}

/// Saved mixer state. `None` means "nothing saved" and is not written; the
/// application then uses its own default.
///
/// The values are what the user set, in dB (gain, master volume) and as a
/// flag (mute). Read them through the accessors, which drop values that
/// cannot be used.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MixerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mic_gain_db: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mic_muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    master_db: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bgm_db: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bgm_muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se_db: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se_muted: Option<bool>,
}

impl MixerConfig {
    /// Creates a config that saves the MIC gain, the MIC mute and the master
    /// volume. The BGM and SE levels are added with
    /// [`with_bgm`](Self::with_bgm) and [`with_se`](Self::with_se).
    pub fn new(mic_gain_db: f32, mic_muted: bool, master_db: f32) -> Self {
        Self {
            mic_gain_db: Some(mic_gain_db),
            mic_muted: Some(mic_muted),
            master_db: Some(master_db),
            ..Self::default()
        }
    }

    /// Saves the volume in dB and the mute of the BGM channel too.
    #[must_use]
    pub fn with_bgm(self, volume_db: f32, muted: bool) -> Self {
        Self {
            bgm_db: Some(volume_db),
            bgm_muted: Some(muted),
            ..self
        }
    }

    /// Saves the volume in dB and the mute of the SE channel too.
    #[must_use]
    pub fn with_se(self, volume_db: f32, muted: bool) -> Self {
        Self {
            se_db: Some(volume_db),
            se_muted: Some(muted),
            ..self
        }
    }

    /// Returns whether nothing is saved.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Returns the saved MIC gain in dB, limited to the MIC range. A value
    /// that is not a finite number counts as not saved (NaN would otherwise
    /// become silence).
    pub fn mic_gain_db(&self) -> Option<f32> {
        self.mic_gain_db
            .filter(|db| db.is_finite())
            .map(|db| MIC_GAIN_RANGE.clamp(db))
    }

    /// Returns the saved MIC mute.
    pub fn mic_muted(&self) -> Option<bool> {
        self.mic_muted
    }

    /// Returns the saved master volume in dB, limited to the master range.
    /// A value that is not a finite number counts as not saved.
    pub fn master_db(&self) -> Option<f32> {
        self.master_db
            .filter(|db| db.is_finite())
            .map(|db| MASTER_VOLUME_RANGE.clamp(db))
    }

    /// Returns the saved BGM volume in dB, limited to the BGM range. A value
    /// that is not a finite number counts as not saved.
    pub fn bgm_db(&self) -> Option<f32> {
        self.bgm_db
            .filter(|db| db.is_finite())
            .map(|db| BGM_VOLUME_RANGE.clamp(db))
    }

    /// Returns the saved BGM mute.
    pub fn bgm_muted(&self) -> Option<bool> {
        self.bgm_muted
    }

    /// Returns the saved SE volume in dB, limited to the SE range. A value
    /// that is not a finite number counts as not saved.
    pub fn se_db(&self) -> Option<f32> {
        self.se_db
            .filter(|db| db.is_finite())
            .map(|db| SE_VOLUME_RANGE.clamp(db))
    }

    /// Returns the saved SE mute.
    pub fn se_muted(&self) -> Option<bool> {
        self.se_muted
    }

    /// Returns the names of the saved values that are not finite numbers,
    /// so that the application can warn about them.
    pub fn invalid_values(&self) -> Vec<&'static str> {
        [
            ("mic_gain_db", self.mic_gain_db),
            ("master_db", self.master_db),
            ("bgm_db", self.bgm_db),
            ("se_db", self.se_db),
        ]
        .into_iter()
        .filter(|(_, db)| db.is_some_and(|db| !db.is_finite()))
        .map(|(name, _)| name)
        .collect()
    }
}

/// The song chosen for the BGM channel and whether it loops.
///
/// The song is saved as the text of its Asset ID, never as a path (ARCHITECTURE.md
/// §9). The ID is kept as text and turned into an [`AssetId`] when it is read,
/// so an ID that was edited into something unusable does not make the whole
/// file unreadable: [`asset`](Self::asset) gives `None` for it and
/// [`invalid_values`](Self::invalid_values) says so. `None` means "nothing
/// saved" and is not written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BgmConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    asset: Option<String>,
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    looping: Option<bool>,
}

impl BgmConfig {
    /// Creates a config that saves the song (if there is one) and the loop.
    pub fn new(asset: Option<&AssetId>, looping: bool) -> Self {
        Self {
            asset: asset.map(ToString::to_string),
            looping: Some(looping),
        }
    }

    /// Returns whether nothing is saved.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Returns the saved song. A text that is not an ID, or the ID of a sound
    /// effect, counts as not saved.
    pub fn asset(&self) -> Option<AssetId> {
        self.asset
            .as_deref()
            .and_then(|text| parse_asset(text, AssetKind::Bgm))
    }

    /// Returns the saved loop setting.
    pub fn looping(&self) -> Option<bool> {
        self.looping
    }

    /// Returns the names of the saved items that cannot be used, so that the
    /// application can warn about them.
    pub fn invalid_values(&self) -> Vec<&'static str> {
        if self.asset.is_some() && self.asset().is_none() {
            vec!["bgm.asset"]
        } else {
            Vec::new()
        }
    }
}

/// The sounds on the Sound Pad: the rule for a sound that is played again,
/// and the sound and the volume of each pad.
///
/// Like [`BgmConfig`], the sounds are saved as the text of their Asset IDs,
/// and the entries are checked when they are read: an entry that cannot be
/// used is left out, and the others are kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SoundPadConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pad: Vec<PadEntry>,
}

/// One pad as it is in the file. Every item may be missing or wrong; the
/// checks are in [`SoundPadConfig::pads`]. The slot is a signed number so that
/// a negative one is a bad entry and not a bad file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PadEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slot: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    asset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gain_db: Option<f32>,
}

/// One pad that can be used: its place, its sound effect, and its volume.
#[derive(Debug, Clone, PartialEq)]
pub struct PadSetting {
    /// The place of the pad (0 is the first).
    pub slot: usize,
    /// The sound effect. Always the ID of an SE.
    pub asset: AssetId,
    /// The volume of the pad in dB, within the SE range.
    pub gain_db: f32,
}

impl SoundPadConfig {
    /// Creates a config that saves the rule and the pads.
    pub fn new(policy: SePolicy, pads: &[PadSetting]) -> Self {
        Self {
            policy: Some(policy_name(policy).to_string()),
            pad: pads
                .iter()
                .map(|pad| PadEntry {
                    slot: i64::try_from(pad.slot).ok(),
                    asset: Some(pad.asset.to_string()),
                    gain_db: Some(pad.gain_db),
                })
                .collect(),
        }
    }

    /// Returns whether nothing is saved.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Returns the saved rule for a sound that is played again. A name that is
    /// not one of the three counts as not saved.
    pub fn policy(&self) -> Option<SePolicy> {
        self.policy.as_deref().and_then(parse_policy)
    }

    /// Returns the pads that can be used, in the order of the file.
    ///
    /// An entry is left out when it has no slot, a negative slot, or a slot of
    /// `slot_count` or more; when its ID is missing, is not an ID, or is the
    /// ID of a song; or when an earlier entry has the same slot (the first one
    /// is kept). A volume that is not a finite number is 0 dB; a missing one
    /// too.
    pub fn pads(&self, slot_count: usize) -> Vec<PadSetting> {
        self.read_pads(slot_count).0
    }

    /// Returns the names of the saved items that cannot be used, so that the
    /// application can warn about them. `slot_count` is the same as for
    /// [`pads`](Self::pads).
    pub fn invalid_values(&self, slot_count: usize) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.policy.is_some() && self.policy().is_none() {
            names.push("sound_pad.policy");
        }
        if self.read_pads(slot_count).1 {
            names.push("sound_pad.pad");
        }
        names
    }

    /// Reads the entries. The second value is whether any entry was left out
    /// or changed.
    fn read_pads(&self, slot_count: usize) -> (Vec<PadSetting>, bool) {
        let mut pads: Vec<PadSetting> = Vec::new();
        let mut problem = false;
        for entry in &self.pad {
            let slot = entry
                .slot
                .and_then(|slot| usize::try_from(slot).ok())
                .filter(|&slot| slot < slot_count);
            let asset = entry
                .asset
                .as_deref()
                .and_then(|text| parse_asset(text, AssetKind::Se));
            let (Some(slot), Some(asset)) = (slot, asset) else {
                problem = true;
                continue;
            };
            if pads.iter().any(|pad| pad.slot == slot) {
                problem = true;
                continue;
            }
            let gain_db = match entry.gain_db {
                None => 0.0,
                Some(db) if db.is_finite() => SE_VOLUME_RANGE.clamp(db),
                Some(_) => {
                    problem = true;
                    0.0
                }
            };
            pads.push(PadSetting {
                slot,
                asset,
                gain_db,
            });
        }
        (pads, problem)
    }
}

/// The ID in `text`, if it is the ID of an Asset of `kind`.
fn parse_asset(text: &str, kind: AssetKind) -> Option<AssetId> {
    text.parse::<AssetId>()
        .ok()
        .filter(|id| id.is_of_kind(kind))
}

/// The name of the rule in the file.
fn policy_name(policy: SePolicy) -> &'static str {
    match policy {
        SePolicy::Overlap => "overlap",
        SePolicy::Restart => "restart",
        SePolicy::Ignore => "ignore",
    }
}

fn parse_policy(name: &str) -> Option<SePolicy> {
    [SePolicy::Overlap, SePolicy::Restart, SePolicy::Ignore]
        .into_iter()
        .find(|&policy| policy_name(policy) == name)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            audio: AudioConfig::default(),
            mixer: MixerConfig::default(),
            bgm: BgmConfig::default(),
            sound_pad: SoundPadConfig::default(),
        }
    }
}

/// Only the version, read first so that a newer file is rejected before its
/// other fields are interpreted.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

impl Config {
    /// Loads the file at `path`, or creates it with default values if it does
    /// not exist.
    ///
    /// An existing file that cannot be read is never overwritten.
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            return Self::load(path);
        }
        let config = Self::default();
        config.save(path)?;
        Ok(config)
    }

    /// Loads the file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(ConfigError::Io)?;
        Self::from_toml(&text)
    }

    /// Saves to `path`, creating the parent directory if needed.
    ///
    /// Writes to a temporary file in the same directory first, flushes it to
    /// the disk, and then renames it over the file, so an interrupted save
    /// (or a power loss) does not corrupt the existing file. When writing
    /// fails, the temporary file is removed and the existing file is kept.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string(self).map_err(|e| ConfigError::Parse(e.to_string()))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(ConfigError::Io)?;
        }
        write_atomically(path, text.as_bytes()).map_err(ConfigError::Io)
    }

    fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let parse = |e: toml::de::Error| ConfigError::Parse(e.to_string());
        let VersionOnly { version } = toml::from_str(text).map_err(parse)?;
        match version {
            CONFIG_VERSION => {}
            // Older versions will be migrated here once the format changes.
            v => return Err(ConfigError::UnsupportedVersion(v)),
        }
        toml::from_str(text).map_err(parse)
    }
}

/// An error while loading or saving the configuration.
///
/// Library error types (for example from `toml`) are converted into messages
/// so that they do not appear in this crate's API.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file could not be read or written.
    Io(io::Error),
    /// The file is not valid TOML or does not match the expected format
    /// (including a missing `version`).
    Parse(String),
    /// The file has a version this build does not support.
    UnsupportedVersion(u32),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "config file I/O error: {e}"),
            Self::Parse(msg) => write!(f, "invalid config file: {msg}"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "unsupported config version {v} (this build supports {CONFIG_VERSION})"
            ),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::atomic_file::temp_path;

    /// A fresh directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ink-core-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_file_is_created_with_defaults() {
        let path = test_dir("create").join("sub").join("config.toml");
        let config = Config::load_or_create(&path).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(Config::load(&path).unwrap().version, CONFIG_VERSION);
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn existing_file_is_loaded_not_recreated() {
        let path = test_dir("existing").join("config.toml");
        let saved = Config {
            version: CONFIG_VERSION,
            audio: AudioConfig {
                input_device: Some(DeviceId::new("alsa:sysdefault:CARD=HID")),
                output_device: Some(DeviceId::new("alsa:default")),
            },
            ..Config::default()
        };
        saved.save(&path).unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap(), saved);
    }

    #[test]
    fn unselected_devices_are_not_written() {
        let config = Config {
            version: CONFIG_VERSION,
            audio: AudioConfig {
                input_device: None,
                output_device: Some(DeviceId::new("alsa:default")),
            },
            ..Config::default()
        };
        let text = toml::to_string(&config).unwrap();
        assert_eq!(
            text,
            "version = 1\n\n[audio]\noutput_device = \"alsa:default\"\n"
        );
    }

    #[test]
    fn missing_version_is_rejected_and_file_kept() {
        let path = test_dir("no-version").join("config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = "[audio]\noutput_device = \"alsa:default\"\n";
        fs::write(&path, text).unwrap();
        assert!(matches!(
            Config::load_or_create(&path),
            Err(ConfigError::Parse(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn newer_version_is_rejected_and_file_kept() {
        let path = test_dir("newer").join("config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = "version = 99\nsomething_new = true\n";
        fs::write(&path, text).unwrap();
        assert!(matches!(
            Config::load_or_create(&path),
            Err(ConfigError::UnsupportedVersion(99))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    /// The file written by #36, before `[mixer]` existed.
    const VERSION_1_FILE: &str = "version = 1\n\n[audio]\n\
         input_device = \"alsa:sysdefault:CARD=Headset\"\n\
         output_device = \"alsa:sysdefault:CARD=Headset\"\n";

    #[test]
    fn a_version_1_file_without_mixer_still_loads() {
        let config = Config::from_toml(VERSION_1_FILE).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(
            config.audio.input_device,
            Some(DeviceId::new("alsa:sysdefault:CARD=Headset"))
        );
        assert!(config.mixer.is_empty());
        assert_eq!(config.mixer.mic_gain_db(), None);
        assert_eq!(config.mixer.mic_muted(), None);
        assert_eq!(config.mixer.master_db(), None);
    }

    #[test]
    fn an_empty_mixer_is_not_written_so_the_file_does_not_change() {
        let config = Config::from_toml(VERSION_1_FILE).unwrap();
        assert_eq!(toml::to_string(&config).unwrap(), VERSION_1_FILE);
    }

    #[test]
    fn mixer_values_round_trip() {
        let path = test_dir("mixer").join("config.toml");
        let saved = Config {
            mixer: MixerConfig::new(3.5, true, -6.5),
            ..Config::default()
        };
        saved.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), saved);

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[mixer]"), "{text}");
        assert!(text.contains("mic_gain_db = 3.5"), "{text}");
        assert!(text.contains("mic_muted = true"), "{text}");
        assert!(text.contains("master_db = -6.5"), "{text}");
    }

    #[test]
    fn values_that_are_not_multiples_of_half_a_db_are_written_short() {
        // The fader steps by 0.5 dB, but a typed or older value can be
        // anything; it should not be written as 3.0999999046325684.
        let config = Config {
            mixer: MixerConfig::new(3.1, false, -12.3),
            ..Config::default()
        };
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("mic_gain_db = 3.1\n"), "{text}");
        assert!(text.contains("master_db = -12.3\n"), "{text}");
    }

    #[test]
    fn a_partial_mixer_loads_with_the_rest_not_saved() {
        let config = Config::from_toml("version = 1\n[mixer]\nmaster_db = -3.0\n").unwrap();
        assert_eq!(config.mixer.master_db(), Some(-3.0));
        assert_eq!(config.mixer.mic_gain_db(), None);
        assert_eq!(config.mixer.mic_muted(), None);
    }

    #[test]
    fn out_of_range_values_are_limited_to_the_range() {
        let mixer = MixerConfig::new(100.0, false, 50.0);
        assert_eq!(mixer.mic_gain_db(), Some(24.0));
        assert_eq!(mixer.master_db(), Some(6.0));
        let mixer = MixerConfig::new(-500.0, false, -500.0);
        assert_eq!(mixer.mic_gain_db(), Some(-60.0));
        assert_eq!(mixer.master_db(), Some(-60.0));
    }

    #[test]
    fn nan_and_infinity_count_as_not_saved() {
        let config = Config::from_toml(
            "version = 1\n[mixer]\nmic_gain_db = nan\nmaster_db = inf\nmic_muted = true\n",
        )
        .unwrap();
        assert_eq!(config.mixer.mic_gain_db(), None, "NaN would mean silence");
        assert_eq!(config.mixer.master_db(), None);
        assert_eq!(config.mixer.mic_muted(), Some(true));
        assert_eq!(config.mixer.invalid_values(), ["mic_gain_db", "master_db"]);
        assert!(
            MixerConfig::new(0.0, false, -12.0)
                .invalid_values()
                .is_empty()
        );
    }

    #[test]
    fn unknown_items_are_ignored() {
        let config =
            Config::from_toml("version = 1\nfuture = 1\n[mixer]\nmaster_db = -3.0\nnew = true\n")
                .unwrap();
        assert_eq!(config.mixer.master_db(), Some(-3.0));
    }

    #[test]
    fn a_wrong_type_is_a_parse_error() {
        assert!(matches!(
            Config::from_toml("version = 1\n[mixer]\nmaster_db = \"loud\"\n"),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn saving_replaces_an_existing_file_and_leaves_no_temporary_file() {
        let path = test_dir("replace").join("config.toml");
        Config::default().save(&path).unwrap();
        let updated = Config {
            mixer: MixerConfig::new(0.0, false, -12.0),
            ..Config::default()
        };
        updated.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), updated);
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn a_failed_save_keeps_the_existing_file_and_removes_the_temporary_file() {
        let path = test_dir("failed").join("config.toml");
        let original = Config {
            mixer: MixerConfig::new(1.0, false, -12.0),
            ..Config::default()
        };
        original.save(&path).unwrap();
        // A directory in the way makes the final rename fail.
        let blocked = path.with_file_name("blocked.toml");
        fs::create_dir_all(blocked.join("inner")).unwrap();

        assert!(matches!(
            Config::default().save(&blocked),
            Err(ConfigError::Io(_))
        ));
        assert!(!temp_path(&blocked).exists());
        assert_eq!(Config::load(&path).unwrap(), original);
    }

    // -- the BGM and SE levels, the song, and the Sound Pad (#64) --------------------

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    fn pad(slot: usize, asset: &str, gain_db: f32) -> PadSetting {
        PadSetting {
            slot,
            asset: id(asset),
            gain_db,
        }
    }

    /// The text of a file with a `[sound_pad]` made of `entries`.
    fn pad_file(entries: &str) -> String {
        format!("version = 1\n[sound_pad]\n{entries}")
    }

    #[test]
    fn the_version_does_not_change_for_the_sounds() {
        assert_eq!(CONFIG_VERSION, 1);
    }

    #[test]
    fn a_file_from_before_the_sounds_loads_with_nothing_saved_for_them() {
        let config = Config::from_toml(
            "version = 1\n[mixer]\nmic_gain_db = 1.5\nmic_muted = false\nmaster_db = -12.0\n",
        )
        .unwrap();
        assert_eq!(config.mixer.mic_gain_db(), Some(1.5));
        assert_eq!(config.mixer.bgm_db(), None);
        assert_eq!(config.mixer.bgm_muted(), None);
        assert_eq!(config.mixer.se_db(), None);
        assert_eq!(config.mixer.se_muted(), None);
        assert!(config.bgm.is_empty());
        assert_eq!(config.bgm.asset(), None);
        assert_eq!(config.bgm.looping(), None);
        assert!(config.sound_pad.is_empty());
        assert_eq!(config.sound_pad.policy(), None);
        assert!(config.sound_pad.pads(8).is_empty());

        let config = Config::from_toml(VERSION_1_FILE).unwrap();
        assert!(config.bgm.is_empty() && config.sound_pad.is_empty());
    }

    #[test]
    fn nothing_is_written_for_sounds_that_are_not_saved() {
        // The empty `[audio]` has always been written; nothing else is.
        let text = toml::to_string(&Config::default()).unwrap();
        assert_eq!(text, "version = 1\n\n[audio]\n");
        // The old levels alone do not make a `[bgm]` or a `[sound_pad]`.
        let config = Config {
            mixer: MixerConfig::new(0.0, false, -12.0),
            ..Config::default()
        };
        let text = toml::to_string(&config).unwrap();
        assert!(!text.contains("bgm"), "{text}");
        assert!(!text.contains("se_"), "{text}");
        assert!(!text.contains("sound_pad"), "{text}");
    }

    #[test]
    fn bgm_and_se_levels_round_trip() {
        let path = test_dir("sound-levels").join("config.toml");
        let saved = Config {
            mixer: MixerConfig::new(3.5, true, -6.5)
                .with_bgm(-9.0, true)
                .with_se(2.5, false),
            ..Config::default()
        };
        saved.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, saved);
        assert_eq!(loaded.mixer.mic_gain_db(), Some(3.5));
        assert_eq!(loaded.mixer.bgm_db(), Some(-9.0));
        assert_eq!(loaded.mixer.bgm_muted(), Some(true));
        assert_eq!(loaded.mixer.se_db(), Some(2.5));
        assert_eq!(loaded.mixer.se_muted(), Some(false));

        let text = fs::read_to_string(&path).unwrap();
        for line in [
            "bgm_db = -9.0",
            "bgm_muted = true",
            "se_db = 2.5",
            "se_muted = false",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    }

    #[test]
    fn bgm_and_se_levels_are_limited_and_a_number_that_is_not_one_is_not_saved() {
        let mixer = MixerConfig::new(0.0, false, -12.0)
            .with_bgm(100.0, false)
            .with_se(-500.0, false);
        assert_eq!(mixer.bgm_db(), Some(6.0));
        assert_eq!(mixer.se_db(), Some(-60.0));

        let config = Config::from_toml(
            "version = 1\n[mixer]\nbgm_db = nan\nse_db = inf\nbgm_muted = true\n",
        )
        .unwrap();
        assert_eq!(config.mixer.bgm_db(), None, "NaN would mean silence");
        assert_eq!(config.mixer.se_db(), None);
        assert_eq!(config.mixer.bgm_muted(), Some(true));
        assert_eq!(config.mixer.invalid_values(), ["bgm_db", "se_db"]);
    }

    #[test]
    fn the_song_and_the_pads_round_trip_and_the_file_has_only_ids() {
        let path = test_dir("sounds").join("config.toml");
        let saved = Config {
            bgm: BgmConfig::new(Some(&id("bgm-0005")), true),
            sound_pad: SoundPadConfig::new(
                SePolicy::Restart,
                &[pad(0, "se-0012", -3.0), pad(3, "se-0100", 0.0)],
            ),
            ..Config::default()
        };
        saved.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, saved);
        assert_eq!(loaded.bgm.asset(), Some(id("bgm-0005")));
        assert_eq!(loaded.bgm.looping(), Some(true));
        assert_eq!(loaded.sound_pad.policy(), Some(SePolicy::Restart));
        assert_eq!(
            loaded.sound_pad.pads(8),
            [pad(0, "se-0012", -3.0), pad(3, "se-0100", 0.0)]
        );

        let text = fs::read_to_string(&path).unwrap();
        for line in [
            "[bgm]",
            "asset = \"bgm-0005\"",
            "loop = true",
            "[sound_pad]",
            "policy = \"restart\"",
            "[[sound_pad.pad]]",
            "slot = 3",
            "asset = \"se-0100\"",
            "gain_db = -3.0",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
        // Only IDs: nothing that looks like a path.
        assert!(!text.contains('/'), "{text}");
        assert!(!text.contains('\\'), "{text}");
        assert!(!text.contains("sounds"), "{text}");
        assert!(!text.contains(".wav") && !text.contains(".mp3"), "{text}");
    }

    #[test]
    fn a_song_without_a_choice_still_saves_the_loop() {
        let text = toml::to_string(&Config {
            bgm: BgmConfig::new(None, false),
            ..Config::default()
        })
        .unwrap();
        assert_eq!(text, "version = 1\n\n[audio]\n\n[bgm]\nloop = false\n");
        let config = Config::from_toml(&text).unwrap();
        assert_eq!(config.bgm.asset(), None);
        assert_eq!(config.bgm.looping(), Some(false));
        assert!(config.bgm.invalid_values().is_empty());
    }

    #[test]
    fn loop_is_the_name_in_the_file() {
        let config = Config::from_toml("version = 1\n[bgm]\nloop = true\n").unwrap();
        assert_eq!(config.bgm.looping(), Some(true));
    }

    #[test]
    fn a_song_that_is_not_the_id_of_a_song_counts_as_not_saved() {
        for text in ["garbage", "se-0001", "bgm-5", "BGM-0005", ""] {
            let config =
                Config::from_toml(&format!("version = 1\n[bgm]\nasset = \"{text}\"\n")).unwrap();
            assert_eq!(config.bgm.asset(), None, "{text:?}");
            assert_eq!(config.bgm.invalid_values(), ["bgm.asset"], "{text:?}");
        }
        let ok = Config::from_toml("version = 1\n[bgm]\nasset = \"bgm-0007\"\n").unwrap();
        assert_eq!(ok.bgm.asset(), Some(id("bgm-0007")));
        assert!(ok.bgm.invalid_values().is_empty());
    }

    #[test]
    fn pads_that_cannot_be_used_are_left_out_and_the_others_kept() {
        let text = pad_file(
            "policy = \"overlap\"\n\
             [[pad]]\nslot = 0\nasset = \"se-0001\"\ngain_db = -3.0\n\
             [[pad]]\nslot = 0\nasset = \"se-0002\"\n\
             [[pad]]\nslot = 8\nasset = \"se-0003\"\n\
             [[pad]]\nslot = -1\nasset = \"se-0004\"\n\
             [[pad]]\nslot = 1\nasset = \"bgm-0001\"\n\
             [[pad]]\nslot = 2\nasset = \"nonsense\"\n\
             [[pad]]\nslot = 3\n\
             [[pad]]\nasset = \"se-0005\"\n\
             [[pad]]\nslot = 4\nasset = \"se-0006\"\ngain_db = nan\n\
             [[pad]]\nslot = 5\nasset = \"se-0007\"\ngain_db = 100.0\n\
             [[pad]]\nslot = 6\nasset = \"se-0008\"\n",
        )
        .replace("[[pad]]", "[[sound_pad.pad]]");
        let config = Config::from_toml(&text).unwrap();
        assert_eq!(
            config.sound_pad.pads(8),
            [
                pad(0, "se-0001", -3.0),
                pad(4, "se-0006", 0.0),
                pad(5, "se-0007", 6.0),
                pad(6, "se-0008", 0.0),
            ]
        );
        assert_eq!(config.sound_pad.invalid_values(8), ["sound_pad.pad"]);
        // With room for 9 pads, slot 8 is a pad too.
        assert_eq!(config.sound_pad.pads(9).len(), 5);
    }

    #[test]
    fn pads_that_are_all_fine_give_no_warning() {
        let config = Config {
            sound_pad: SoundPadConfig::new(
                SePolicy::Ignore,
                &[pad(1, "se-0002", -60.0), pad(7, "se-0003", 6.0)],
            ),
            ..Config::default()
        };
        assert!(config.sound_pad.invalid_values(8).is_empty());
        assert_eq!(config.sound_pad.pads(8).len(), 2);
    }

    #[test]
    fn the_three_rules_have_names_and_an_unknown_name_counts_as_not_saved() {
        for (policy, name) in [
            (SePolicy::Overlap, "overlap"),
            (SePolicy::Restart, "restart"),
            (SePolicy::Ignore, "ignore"),
        ] {
            let text = toml::to_string(&Config {
                sound_pad: SoundPadConfig::new(policy, &[]),
                ..Config::default()
            })
            .unwrap();
            assert!(text.contains(&format!("policy = \"{name}\"")), "{text}");
            let config = Config::from_toml(&text).unwrap();
            assert_eq!(config.sound_pad.policy(), Some(policy));
            assert!(config.sound_pad.invalid_values(8).is_empty());
        }

        let config = Config::from_toml(&pad_file("policy = \"loud\"\n")).unwrap();
        assert_eq!(config.sound_pad.policy(), None);
        assert_eq!(config.sound_pad.invalid_values(8), ["sound_pad.policy"]);
    }

    #[test]
    fn a_wrong_type_in_the_sound_items_is_a_parse_error_like_the_other_items() {
        for text in [
            "version = 1\n[mixer]\nbgm_db = \"loud\"\n",
            "version = 1\n[mixer]\nse_muted = 1\n",
            "version = 1\n[bgm]\nasset = 5\n",
            "version = 1\n[bgm]\nloop = \"yes\"\n",
            "version = 1\n[sound_pad]\npolicy = 1\n",
            "version = 1\n[[sound_pad.pad]]\nslot = \"first\"\nasset = \"se-0001\"\n",
        ] {
            assert!(
                matches!(Config::from_toml(text), Err(ConfigError::Parse(_))),
                "{text}"
            );
        }
    }

    #[test]
    fn unknown_items_in_the_sound_tables_are_ignored() {
        let config = Config::from_toml(
            "version = 1\n[bgm]\nasset = \"bgm-0001\"\nfade = 3\n\
             [[sound_pad.pad]]\nslot = 0\nasset = \"se-0001\"\nhotkey = \"F1\"\n",
        )
        .unwrap();
        assert_eq!(config.bgm.asset(), Some(id("bgm-0001")));
        assert_eq!(config.sound_pad.pads(8), [pad(0, "se-0001", 0.0)]);
    }
}
