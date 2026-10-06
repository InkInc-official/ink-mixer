//! A peak meter shared between the audio thread and the UI.

use std::sync::atomic::{AtomicU32, Ordering};

/// Holds the largest peak (a linear level) since the UI last read it.
///
/// The audio thread writes with [`PeakMeter::record`] and the UI reads with
/// [`PeakMeter::take`], which also resets it to 0 (ADR-0007). Holding the
/// peak on screen and letting it fall slowly is done by the UI.
///
/// The value is an f32 stored as its bit pattern. For values of 0 and above,
/// a larger value has a larger bit pattern, so `fetch_max` on the bits keeps
/// the larger level.
///
/// All operations use `Ordering::Relaxed`: the meter does not publish any
/// other data, and `fetch_max` and `swap` are single read-modify-write
/// operations, so no peak is lost when they overlap.
#[derive(Debug, Default)]
pub struct PeakMeter(AtomicU32);

impl PeakMeter {
    /// Creates a meter reading 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the peak of one block. Call it once at the end of each block
    /// with a value from [`block_peak`](super::block_peak).
    ///
    /// For a stereo signal (MASTER), call `block_peak` on L and on R and
    /// record the larger of the two, once.
    ///
    /// Values of 0 or below and NaN are ignored: they never raise the meter,
    /// and the bit pattern of -0.0 would otherwise compare as a huge value.
    ///
    /// Safe to call on the realtime path: it does not allocate or block.
    pub fn record(&self, peak: f32) {
        if peak > 0.0 {
            self.0.fetch_max(peak.to_bits(), Ordering::Relaxed);
        }
    }

    /// Returns the largest peak recorded since the last call, and resets the
    /// meter to 0.
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn take_returns_largest_recorded_peak() {
        let meter = PeakMeter::new();
        meter.record(0.5);
        meter.record(0.2);
        meter.record(0.4);
        assert_eq!(meter.take(), 0.5);
    }

    #[test]
    fn take_resets_to_zero() {
        let meter = PeakMeter::new();
        meter.record(0.8);
        assert_eq!(meter.take(), 0.8);
        assert_eq!(meter.take(), 0.0);

        meter.record(0.1);
        assert_eq!(meter.take(), 0.1);
    }

    #[test]
    fn zero_negative_and_nan_are_ignored() {
        let meter = PeakMeter::new();
        meter.record(0.3);
        meter.record(0.0);
        meter.record(-0.0);
        meter.record(-1.0);
        meter.record(f32::NAN);
        assert_eq!(meter.take(), 0.3);

        meter.record(-0.0);
        meter.record(f32::NAN);
        assert_eq!(meter.take(), 0.0);
    }

    #[test]
    fn peak_recorded_on_another_thread_is_read() {
        let meter = Arc::new(PeakMeter::new());
        let writer = Arc::clone(&meter);
        thread::spawn(move || {
            for i in 1..=100 {
                writer.record(i as f32 / 100.0);
            }
        })
        .join()
        .unwrap();
        assert_eq!(meter.take(), 1.0);
    }
}
