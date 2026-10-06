//! Backend-independent device types.

use std::fmt;

/// An opaque identifier for an audio device, assigned by a backend.
///
/// Upper layers must treat the contents as opaque: compare, store, and
/// display it, but never parse it.
///
/// A backend that creates `DeviceId`s must make them:
///
/// 1. unique within that backend,
/// 2. stable across application restarts, device reconnection, and OS
///    reboots where possible, so that a saved device can be found again,
/// 3. restorable from their string form with [`DeviceId::new`].
///
/// The same physical device may have the same ID for input and output; the
/// direction is given by the method that returned it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(String);

impl DeviceId {
    /// Creates an ID from its string form.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the string form of this ID, suitable for saving.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Information about one audio device in one direction (input or output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Identifier assigned by the backend. Use this, not `name`, to save a
    /// device selection.
    pub id: DeviceId,
    /// Human-readable name for display in the UI or CLI.
    pub name: String,
    /// Maximum number of channels available in this direction.
    pub channels: u16,
    /// Supported sample rates in Hz, in ascending order without duplicates.
    pub sample_rates: Vec<u32>,
}
