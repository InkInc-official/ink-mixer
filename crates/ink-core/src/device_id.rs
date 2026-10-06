//! Device identifier shared by the backend and the configuration.
//!
//! Defined here, not in `ink-backend`, so that `ink-core` does not depend on
//! `ink-backend` (see ADR-0003).

use std::fmt;

use serde::{Deserialize, Serialize};

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
///
/// It is serialized as a plain string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_string() {
        let id = DeviceId::new("alsa:sysdefault:CARD=HID");
        assert_eq!(id.as_str(), "alsa:sysdefault:CARD=HID");
        assert_eq!(DeviceId::new(id.to_string()), id);
    }

    #[test]
    fn serializes_as_plain_string() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Wrapper {
            device: DeviceId,
        }
        let wrapper = Wrapper {
            device: DeviceId::new("wasapi:{0.0.0.00000000}.{abc}"),
        };
        let text = toml::to_string(&wrapper).unwrap();
        assert_eq!(text, "device = \"wasapi:{0.0.0.00000000}.{abc}\"\n");
        assert_eq!(toml::from_str::<Wrapper>(&text).unwrap(), wrapper);
    }
}
