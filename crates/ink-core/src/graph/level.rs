//! Level conversions: dB and linear gain, dB ranges, and block peaks.
//!
//! These are plain functions without shared state. dB values are used by the
//! UI and the configuration; the audio thread works with linear gains
//! (ADR-0007).

/// The lowest gain in dB. This value and anything below it means silence
/// (a linear gain of 0).
pub const SILENCE_DB: f32 = -60.0;

/// The range of the MIC gain.
pub const MIC_GAIN_RANGE: DbRange = DbRange::new(SILENCE_DB, 24.0);

/// The range of the master volume.
pub const MASTER_VOLUME_RANGE: DbRange = DbRange::new(SILENCE_DB, 6.0);

/// A closed range of gains in dB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DbRange {
    /// The lowest value.
    pub min: f32,
    /// The highest value.
    pub max: f32,
}

impl DbRange {
    /// Creates a range from `min` to `max` (inclusive).
    pub const fn new(min: f32, max: f32) -> Self {
        Self { min, max }
    }

    /// Limits `db` to this range. NaN becomes the lowest value.
    pub fn clamp(&self, db: f32) -> f32 {
        if db.is_nan() {
            self.min
        } else {
            db.clamp(self.min, self.max)
        }
    }
}

/// Converts dB to a linear gain.
///
/// [`SILENCE_DB`] and below, and NaN, give 0 (silence). Callers clamp the
/// value to a [`DbRange`] first.
pub fn db_to_gain(db: f32) -> f32 {
    if db > SILENCE_DB {
        10.0_f32.powf(db / 20.0)
    } else {
        0.0
    }
}

/// Converts a linear gain to dB.
///
/// 0 (silence), negative values, and NaN give negative infinity. Limiting
/// the result for display is left to the UI.
pub fn gain_to_db(gain: f32) -> f32 {
    if gain > 0.0 {
        20.0 * gain.log10()
    } else {
        f32::NEG_INFINITY
    }
}

/// Returns the largest absolute sample value in `samples` (a linear level).
///
/// NaN samples count as 0. An empty slice gives 0.
///
/// Safe to call on the realtime path: it does not allocate or block.
pub fn block_peak(samples: &[f32]) -> f32 {
    let mut peak = 0.0_f32;
    for &sample in samples {
        let level = sample.abs();
        // A comparison with NaN is false, so NaN samples are skipped.
        if level > peak {
            peak = level;
        }
    }
    peak
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32, tolerance: f32) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} is not within {tolerance} of {expected}"
        );
    }

    #[test]
    fn db_to_gain_converts_common_values() {
        assert_eq!(db_to_gain(0.0), 1.0);
        assert_close(db_to_gain(6.0), 1.995_262, 1e-5);
        assert_close(db_to_gain(-6.0), 0.501_187, 1e-5);
        assert_close(db_to_gain(24.0), 15.848_932, 1e-4);
    }

    #[test]
    fn silence_and_below_give_zero_gain() {
        assert_eq!(db_to_gain(-60.0), 0.0);
        assert_eq!(db_to_gain(-80.0), 0.0);
        assert_eq!(db_to_gain(f32::NEG_INFINITY), 0.0);
        assert_eq!(db_to_gain(f32::NAN), 0.0);
        assert!(db_to_gain(-59.9) > 0.0);
    }

    #[test]
    fn gain_to_db_converts_and_handles_silence() {
        assert_eq!(gain_to_db(1.0), 0.0);
        assert_close(gain_to_db(0.5), -6.020_6, 1e-4);
        assert_eq!(gain_to_db(0.0), f32::NEG_INFINITY);
        assert_eq!(gain_to_db(-1.0), f32::NEG_INFINITY);
        assert_eq!(gain_to_db(f32::NAN), f32::NEG_INFINITY);
    }

    #[test]
    fn db_round_trips_through_gain() {
        for tenth in -590..=240 {
            let db = tenth as f32 / 10.0;
            assert_close(gain_to_db(db_to_gain(db)), db, 1e-4);
        }
    }

    #[test]
    fn ranges_clamp_to_their_ends() {
        assert_eq!(MIC_GAIN_RANGE.clamp(30.0), 24.0);
        assert_eq!(MIC_GAIN_RANGE.clamp(-100.0), -60.0);
        assert_eq!(MIC_GAIN_RANGE.clamp(-12.5), -12.5);
        assert_eq!(MASTER_VOLUME_RANGE.clamp(10.0), 6.0);
        assert_eq!(MASTER_VOLUME_RANGE.clamp(-3.0), -3.0);
        assert_eq!(MASTER_VOLUME_RANGE.clamp(f32::NAN), -60.0);
    }

    #[test]
    fn block_peak_uses_absolute_values() {
        assert_eq!(block_peak(&[0.2, -0.7, 0.5]), 0.7);
        assert_eq!(block_peak(&[]), 0.0);
    }

    #[test]
    fn block_peak_treats_nan_as_zero() {
        assert_eq!(block_peak(&[f32::NAN, f32::NAN]), 0.0);
        assert_eq!(block_peak(&[0.1, f32::NAN, -0.4]), 0.4);
        assert_eq!(block_peak(&[f32::NAN, 0.3]), 0.3);
    }
}
