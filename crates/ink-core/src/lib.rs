//! Ink Mixer core.
//!
//! Holds OS-independent types shared across the application, such as
//! the Audio Graph model (Source / Node / Mixer / Sink), configuration, and
//! the sound library (the BGM and SE folders, their Assets and the index).
//!
//! This crate must not depend on OS-specific audio APIs or on `cpal`.

pub mod graph;

mod asset;
mod atomic_file;
mod config;
mod device_id;
mod library;
mod library_import;
mod library_index;
mod library_scan;

pub use asset::{
    Asset, AssetFormat, AssetId, AssetIdError, AssetInfo, AssetKind, AssetState, ProbeFailure,
    SE_MAX_DURATION,
};
pub use config::{AudioConfig, CONFIG_VERSION, Config, ConfigError, MixerConfig};
pub use device_id::DeviceId;
pub use library::{Library, LibraryError, Opened};
pub use library_import::{ImportError, ImportOutcome};
pub use library_index::{IndexError, IndexWarning, LIBRARY_VERSION, PathProblem, path_problem};
pub use library_scan::{MAX_DEPTH, ProbeFn, ScanReport, SkipReason, Skipped};
