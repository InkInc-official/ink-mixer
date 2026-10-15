//! Application configuration, stored as TOML.
//!
//! This module only reads and writes a file at a given path. Choosing the
//! OS-specific location is left to the application.
//!
//! Loading and saving do file I/O: never call them from the realtime path.

use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DeviceId;
use crate::graph::{MASTER_VOLUME_RANGE, MIC_GAIN_RANGE};

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
}

impl MixerConfig {
    /// Creates a config that saves all three values.
    pub fn new(mic_gain_db: f32, mic_muted: bool, master_db: f32) -> Self {
        Self {
            mic_gain_db: Some(mic_gain_db),
            mic_muted: Some(mic_muted),
            master_db: Some(master_db),
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

    /// Returns the names of the saved values that are not finite numbers,
    /// so that the application can warn about them.
    pub fn invalid_values(&self) -> Vec<&'static str> {
        [
            ("mic_gain_db", self.mic_gain_db),
            ("master_db", self.master_db),
        ]
        .into_iter()
        .filter(|(_, db)| db.is_some_and(|db| !db.is_finite()))
        .map(|(name, _)| name)
        .collect()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            audio: AudioConfig::default(),
            mixer: MixerConfig::default(),
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
        let tmp = temp_path(path);
        let result = write_synced(&tmp, text.as_bytes()).and_then(|()| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(ConfigError::Io)
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

/// Creates `path`, writes `bytes`, and flushes them to the disk.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
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
    use super::*;

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
}
