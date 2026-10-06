//! Application configuration, stored as TOML.
//!
//! This module only reads and writes a file at a given path. Choosing the
//! OS-specific location is left to the application.
//!
//! Loading and saving do file I/O: never call them from the realtime path.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DeviceId;

/// Version of the configuration format written by this build.
pub const CONFIG_VERSION: u32 = 1;

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Format version. Required in the file.
    pub version: u32,
    /// Audio device selection.
    #[serde(default)]
    pub audio: AudioConfig,
}

/// Selected audio devices. `None` means "not selected" and is not written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_device: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_device: Option<DeviceId>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            audio: AudioConfig::default(),
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
    /// Writes to a temporary file in the same directory first and then
    /// renames it, so an interrupted save does not corrupt the existing file.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string(self).map_err(|e| ConfigError::Parse(e.to_string()))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(ConfigError::Io)?;
        }
        let tmp = temp_path(path);
        fs::write(&tmp, text).map_err(ConfigError::Io)?;
        fs::rename(&tmp, path).map_err(ConfigError::Io)
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
}
