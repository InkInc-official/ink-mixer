//! The audio backend abstraction.

use crate::{
    DeviceId, DeviceInfo, InputCallback, InputStream, OutputCallback, OutputStream, Result,
    StreamConfig,
};

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

    /// Opens and starts capturing from the input device `device` with
    /// `config`.
    ///
    /// `callback` receives the captured audio on the audio thread (see
    /// [`InputCallback`]). The stream runs until the returned
    /// [`InputStream`] is dropped.
    ///
    /// # Errors
    ///
    /// - [`BackendError::DeviceUnavailable`](crate::BackendError::DeviceUnavailable)
    ///   if the device is not found or cannot be opened.
    /// - [`BackendError::UnsupportedConfig`](crate::BackendError::UnsupportedConfig)
    ///   if the device does not support the sample rate and channel count.
    fn open_input(
        &self,
        device: &DeviceId,
        config: StreamConfig,
        callback: InputCallback,
    ) -> Result<InputStream>;

    /// Opens and starts playing to the output device `device` with `config`.
    ///
    /// `callback` fills each block to be played on the audio thread (see
    /// [`OutputCallback`]). The stream runs until the returned
    /// [`OutputStream`] is dropped.
    ///
    /// # Errors
    ///
    /// - [`BackendError::DeviceUnavailable`](crate::BackendError::DeviceUnavailable)
    ///   if the device is not found or cannot be opened.
    /// - [`BackendError::UnsupportedConfig`](crate::BackendError::UnsupportedConfig)
    ///   if the device does not support the sample rate and channel count.
    fn open_output(
        &self,
        device: &DeviceId,
        config: StreamConfig,
        callback: OutputCallback,
    ) -> Result<OutputStream>;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::BackendError;
    use crate::stream::StreamStatus;

    /// A backend with one fixed output device and no listed input devices.
    ///
    /// `open_input` accepts only [`FAKE_MIC`] and `open_output` only the
    /// fake speaker; each calls the callback once, synchronously, with one
    /// frame per channel.
    struct FakeBackend;

    const FAKE_MIC: &str = "fake:mic";

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

        fn open_input(
            &self,
            device: &DeviceId,
            config: StreamConfig,
            mut callback: InputCallback,
        ) -> Result<InputStream> {
            if device.as_str() != FAKE_MIC {
                return Err(BackendError::DeviceUnavailable(device.to_string()));
            }
            let status = Arc::new(StreamStatus::new());
            let frame = vec![0.25; usize::from(config.channels)];
            status.record_callback(1);
            callback(&frame);
            Ok(InputStream::new(Box::new(()), status, config, "f32"))
        }

        fn open_output(
            &self,
            device: &DeviceId,
            config: StreamConfig,
            mut callback: OutputCallback,
        ) -> Result<OutputStream> {
            if *device != speaker().id {
                return Err(BackendError::DeviceUnavailable(device.to_string()));
            }
            let status = Arc::new(StreamStatus::new());
            let mut frame = vec![0.0; usize::from(config.channels)];
            status.record_callback(1);
            callback(&mut frame);
            Ok(OutputStream::new(Box::new(()), status, config, "f32"))
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
    fn open_input_through_trait_object() {
        let backend: Box<dyn AudioBackend> = Box::new(FakeBackend);
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let config = StreamConfig {
            sample_rate: 48_000,
            channels: 2,
        };

        let stream = backend
            .open_input(
                &DeviceId::new(FAKE_MIC),
                config,
                Box::new(move |samples: &[f32]| sink.lock().unwrap().extend_from_slice(samples)),
            )
            .unwrap();

        assert_eq!(stream.config(), config);
        assert_eq!(stream.callbacks(), 1);
        assert_eq!(stream.error(), None);
        assert_eq!(*received.lock().unwrap(), vec![0.25, 0.25]);
    }

    #[test]
    fn open_input_unknown_device_is_unavailable() {
        let config = StreamConfig {
            sample_rate: 48_000,
            channels: 1,
        };
        let result = FakeBackend.open_input(&DeviceId::new("fake:none"), config, Box::new(|_| {}));
        assert!(matches!(result, Err(BackendError::DeviceUnavailable(_))));
    }

    #[test]
    fn open_output_through_trait_object() {
        let backend: Box<dyn AudioBackend> = Box::new(FakeBackend);
        let given = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&given);
        let config = StreamConfig {
            sample_rate: 48_000,
            channels: 2,
        };

        let stream = backend
            .open_output(
                &speaker().id,
                config,
                Box::new(move |buffer: &mut [f32]| {
                    seen.lock().unwrap().extend_from_slice(buffer);
                    buffer.fill(0.5);
                }),
            )
            .unwrap();

        assert_eq!(stream.config(), config);
        assert_eq!(stream.callbacks(), 1);
        assert_eq!(stream.error(), None);
        // The callback gets a silent buffer with one frame of two channels.
        assert_eq!(*given.lock().unwrap(), vec![0.0, 0.0]);
    }

    #[test]
    fn open_output_unknown_device_is_unavailable() {
        let config = StreamConfig {
            sample_rate: 48_000,
            channels: 2,
        };
        let result = FakeBackend.open_output(&DeviceId::new("fake:none"), config, Box::new(|_| {}));
        assert!(matches!(result, Err(BackendError::DeviceUnavailable(_))));
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
