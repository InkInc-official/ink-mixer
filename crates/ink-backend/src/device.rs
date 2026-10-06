//! Backend-independent device types.
//!
//! `DeviceId` lives in `ink-core` (ADR-0003) and is re-exported here.

pub use ink_core::DeviceId;

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
