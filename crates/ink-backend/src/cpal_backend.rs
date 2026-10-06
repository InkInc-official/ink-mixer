//! [`AudioBackend`] implementation based on `cpal` (see ADR-0002).
//!
//! This is the only module that uses `cpal`. No `cpal` type appears in its
//! public API.

use std::collections::BTreeSet;

use cpal::traits::{DeviceTrait, HostTrait};
use tracing::{debug, warn};

use crate::{AudioBackend, BackendError, DeviceId, DeviceInfo, Result};

/// Audio backend using the OS default `cpal` host
/// (Linux: ALSA, Windows: WASAPI).
pub struct CpalBackend {
    host: cpal::Host,
}

impl CpalBackend {
    /// Creates a backend on the OS default host.
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }

    fn enumerate(&self, direction: Direction) -> Result<Vec<DeviceInfo>> {
        let devices = self
            .host
            .devices()
            .map_err(|e| BackendError::HostUnavailable(e.to_string()))?;

        let mut infos = Vec::new();
        for device in devices {
            let id = match device.id() {
                Ok(id) => id.to_string(),
                Err(e) => {
                    warn!("skipping audio device without an ID: {e}");
                    continue;
                }
            };
            if !is_listed(&id) {
                debug!("skipping audio device not in the list: {id}");
                continue;
            }
            if !direction.supported_by(&device) {
                continue;
            }
            match device_info(&device, id, direction) {
                Ok(Some(info)) => infos.push(info),
                // No configuration in this direction: not a device of this kind.
                Ok(None) => {}
                Err(e) => warn!("skipping audio device: {e}"),
            }
        }
        Ok(infos)
    }

    fn default_device(&self, direction: Direction) -> Result<Option<DeviceInfo>> {
        let device = match direction {
            Direction::Input => self.host.default_input_device(),
            Direction::Output => self.host.default_output_device(),
        };
        let Some(device) = device else {
            return Ok(None);
        };
        let id = device
            .id()
            .map_err(|e| BackendError::DeviceUnavailable(e.to_string()))?
            .to_string();
        device_info(&device, id, direction)
    }
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBackend for CpalBackend {
    fn enumerate_input_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.enumerate(Direction::Input)
    }

    fn enumerate_output_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.enumerate(Direction::Output)
    }

    fn default_input(&self) -> Result<Option<DeviceInfo>> {
        self.default_device(Direction::Input)
    }

    fn default_output(&self) -> Result<Option<DeviceInfo>> {
        self.default_device(Direction::Output)
    }
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    Input,
    Output,
}

impl Direction {
    /// Cheap check that does not open the device.
    fn supported_by(self, device: &cpal::Device) -> bool {
        match self {
            Self::Input => device.supports_input(),
            Self::Output => device.supports_output(),
        }
    }
}

/// Builds the device information for one direction.
///
/// Returns `Ok(None)` when the device has no configuration in that direction.
fn device_info(
    device: &cpal::Device,
    id: String,
    direction: Direction,
) -> Result<Option<DeviceInfo>> {
    let unavailable = |e: cpal::Error| BackendError::DeviceUnavailable(format!("{id}: {e}"));

    let ranges: Vec<_> = match direction {
        Direction::Input => device
            .supported_input_configs()
            .map_err(unavailable)?
            .collect(),
        Direction::Output => device
            .supported_output_configs()
            .map_err(unavailable)?
            .collect(),
    };
    let Some(channels) = ranges.iter().map(|r| r.channels()).max() else {
        return Ok(None);
    };
    let sample_rates = expand_sample_rates(
        ranges
            .iter()
            .map(|r| (r.min_sample_rate(), r.max_sample_rate())),
    );

    let name = match device.description() {
        Ok(description) => description.name().to_string(),
        Err(_) => id.clone(),
    };

    Ok(Some(DeviceInfo {
        id: DeviceId::new(id),
        name,
        channels,
        sample_rates,
    }))
}

/// Whether a device is shown in the device lists, judged by its `cpal`
/// device ID string (`"<host>:<device>"`).
///
/// ALSA reports many plugin and virtual PCMs (`null`, `dmix`, `surround51`,
/// ...). Only the sound-server entries and per-card defaults are listed; the
/// check runs before the device is opened. Other hosts list every device.
///
/// Kept as a separate function so that a "show all devices" option can be
/// added later by bypassing it.
fn is_listed(device_id: &str) -> bool {
    let Some(pcm) = device_id.strip_prefix("alsa:") else {
        return true;
    };
    matches!(pcm, "default" | "pipewire" | "pulse") || pcm.starts_with("sysdefault:CARD=")
}

/// Lowest and highest sample rates (Hz) reported in a device's list.
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 384_000;

/// Standard rates reported when they fall inside a supported range.
const STANDARD_SAMPLE_RATES: [u32; 13] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
    352_800, 384_000,
];

/// Expands supported sample-rate ranges (`(min, max)` in Hz, inclusive) into a
/// sorted list without duplicates.
///
/// Each range contributes its own `min` and `max` (only when they lie within
/// 8000–384000 Hz) and every standard rate it contains.
fn expand_sample_rates(ranges: impl IntoIterator<Item = (u32, u32)>) -> Vec<u32> {
    let in_bounds = |rate: &u32| (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(rate);
    let mut rates = BTreeSet::new();
    for (min, max) in ranges {
        rates.extend([min, max].into_iter().filter(in_bounds));
        rates.extend(
            STANDARD_SAMPLE_RATES
                .into_iter()
                .filter(|rate| (min..=max).contains(rate)),
        );
    }
    rates.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_rate_range() {
        assert_eq!(expand_sample_rates([(48_000, 48_000)]), vec![48_000]);
    }

    #[test]
    fn narrow_range_keeps_min_and_max() {
        assert_eq!(
            expand_sample_rates([(44_100, 48_000)]),
            vec![44_100, 48_000]
        );
        assert_eq!(
            expand_sample_rates([(45_000, 47_000)]),
            vec![45_000, 47_000]
        );
    }

    #[test]
    fn wide_range_includes_standard_rates() {
        assert_eq!(
            expand_sample_rates([(8_000, 192_000)]),
            vec![
                8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
                192_000
            ]
        );
    }

    #[test]
    fn multiple_ranges_are_merged_sorted_and_deduplicated() {
        assert_eq!(
            expand_sample_rates([(96_000, 96_000), (44_100, 48_000), (48_000, 48_000)]),
            vec![44_100, 48_000, 96_000]
        );
    }

    #[test]
    fn min_and_max_outside_bounds_are_dropped() {
        assert_eq!(
            expand_sample_rates([(1, 4_000_000)]),
            STANDARD_SAMPLE_RATES.to_vec()
        );
        assert_eq!(expand_sample_rates([(4_000, 4_000)]), Vec::<u32>::new());
        assert_eq!(expand_sample_rates([(4_000, 9_000)]), vec![8_000, 9_000]);
    }

    #[test]
    fn alsa_allow_list() {
        for id in [
            "alsa:default",
            "alsa:pulse",
            "alsa:pipewire",
            "alsa:sysdefault:CARD=HID",
        ] {
            assert!(is_listed(id), "{id} should be listed");
        }
        for id in [
            "alsa:null",
            "alsa:hw:CARD=PCH,DEV=3",
            "alsa:plughw:CARD=HID,DEV=0",
            "alsa:dmix:CARD=HID,DEV=0",
            "alsa:surround51:CARD=HID,DEV=0",
            "alsa:sysdefault",
            "alsa:jack",
        ] {
            assert!(!is_listed(id), "{id} should not be listed");
        }
    }

    #[test]
    fn other_hosts_list_everything() {
        assert!(is_listed("wasapi:{0.0.0.00000000}.{abc}"));
    }
}
