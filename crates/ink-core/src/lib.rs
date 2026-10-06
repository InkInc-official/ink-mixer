//! Ink Mixer core.
//!
//! Holds OS-independent types shared across the application, such as
//! the Audio Graph model (Source / Node / Mixer / Sink) and configuration.
//!
//! This crate must not depend on OS-specific audio APIs or on `cpal`.

mod config;
mod device_id;

pub use config::{AudioConfig, CONFIG_VERSION, Config, ConfigError};
pub use device_id::DeviceId;
