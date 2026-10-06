//! The audio backend abstraction.

use crate::{DeviceInfo, Result};

/// Backend-independent access to the audio devices of the system.
///
/// Implementations (for example the `cpal`-based backend) live inside this
/// crate. Upper layers use only this trait and the types in this crate, never
/// the types of the underlying library.
///
/// These methods query the OS and allocate, so call them from a control
/// thread, never from the realtime path.
///
/// # Errors
///
/// The enumeration methods return `Err` only when the audio system itself
/// cannot be queried. A device whose details cannot be read (for example,
/// because it is busy or access is denied) is left out of the list instead,
/// and the call still returns `Ok`.
pub trait AudioBackend {
    /// Lists the available input (capture) devices.
    fn enumerate_input_devices(&self) -> Result<Vec<DeviceInfo>>;

    /// Lists the available output (playback) devices.
    fn enumerate_output_devices(&self) -> Result<Vec<DeviceInfo>>;

    /// Returns the system default input device.
    ///
    /// Returns `Ok(None)` when there is no default input device; this is a
    /// normal state, not an error.
    fn default_input(&self) -> Result<Option<DeviceInfo>>;

    /// Returns the system default output device.
    ///
    /// Returns `Ok(None)` when there is no default output device; this is a
    /// normal state, not an error.
    fn default_output(&self) -> Result<Option<DeviceInfo>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendError, DeviceId};

    /// A backend with one fixed output device and no input devices.
    struct FakeBackend;

    fn speaker() -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new("fake:speaker"),
            name: "Fake Speaker".to_string(),
            channels: 2,
            sample_rates: vec![44_100, 48_000],
        }
    }

    impl AudioBackend for FakeBackend {
        fn enumerate_input_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(Vec::new())
        }

        fn enumerate_output_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(vec![speaker()])
        }

        fn default_input(&self) -> Result<Option<DeviceInfo>> {
            Ok(None)
        }

        fn default_output(&self) -> Result<Option<DeviceInfo>> {
            Ok(Some(speaker()))
        }
    }

    #[test]
    fn backend_is_object_safe() {
        let backend: Box<dyn AudioBackend> = Box::new(FakeBackend);
        assert_eq!(backend.enumerate_output_devices().unwrap(), vec![speaker()]);
        assert!(backend.enumerate_input_devices().unwrap().is_empty());
    }

    #[test]
    fn missing_default_device_is_none() {
        let backend = FakeBackend;
        assert_eq!(backend.default_input().unwrap(), None);
        assert_eq!(backend.default_output().unwrap(), Some(speaker()));
    }

    #[test]
    fn device_id_round_trips_through_string() {
        let id = DeviceId::new("alsa:hw:CARD=PCH,DEV=0");
        assert_eq!(id.as_str(), "alsa:hw:CARD=PCH,DEV=0");
        assert_eq!(DeviceId::new(id.to_string()), id);
    }

    #[test]
    fn backend_error_display_contains_message() {
        let err = BackendError::DeviceUnavailable("unplugged".to_string());
        assert!(err.to_string().contains("unplugged"));
    }
}
