//! A frame rate cap managed by the application (ADR-0004, Spike #21).
//!
//! egui's `request_repaint_after(delay)` subtracts the predicted frame time
//! (about 16.7 ms) from `delay`, so asking for 16 ms means "now" and caps
//! nothing. Without a cap, the window repainted as fast as possible and used
//! a whole core on Windows. This limiter keeps its own schedule of frame
//! times at a fixed interval and adds the predicted frame time back.
//!
//! It takes the current time as an argument, so it can be tested without a
//! clock.

use std::time::{Duration, Instant};

/// The frame rate cap while something on screen moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameRate {
    /// 60 frames per second (the default).
    #[default]
    Fps60,
    /// 30 frames per second, for slower PCs (about half the CPU of 60).
    Fps30,
}

impl FrameRate {
    /// Every frame rate, in the order shown to the user.
    pub const ALL: [FrameRate; 2] = [FrameRate::Fps60, FrameRate::Fps30];

    /// Returns the frames per second.
    pub fn fps(self) -> u32 {
        match self {
            Self::Fps60 => 60,
            Self::Fps30 => 30,
        }
    }

    /// Returns the time between two frames.
    pub fn interval(self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.fps()))
    }

    /// Returns the label shown to the user, such as `"60 fps"`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Fps60 => "60 fps",
            Self::Fps30 => "30 fps",
        }
    }
}

/// Schedules frames at a fixed interval.
#[derive(Debug, Clone)]
pub struct FrameLimiter {
    rate: FrameRate,
    interval: Duration,
    /// When the next scheduled frame is due.
    next: Instant,
}

impl FrameLimiter {
    /// Creates a limiter for `rate` whose first frame is due at `now`.
    pub fn new(rate: FrameRate, now: Instant) -> Self {
        Self {
            rate,
            interval: rate.interval(),
            next: now,
        }
    }

    /// Returns the current frame rate.
    pub fn rate(&self) -> FrameRate {
        self.rate
    }

    /// Changes the frame rate. The schedule restarts from `now`.
    pub fn set_rate(&mut self, rate: FrameRate, now: Instant) {
        if rate != self.rate {
            self.rate = rate;
            self.interval = rate.interval();
            self.next = now;
        }
    }

    /// Returns the delay to pass to egui's `request_repaint_after` at the end
    /// of a frame that started at `frame_start`, when it is now `now` and egui
    /// predicts a frame time of `predicted_dt`.
    ///
    /// The schedule advances only on a scheduled frame, so extra frames (for
    /// example, from mouse input) do not shift it. When the frame is late by
    /// a whole interval or more, the schedule restarts from the frame instead
    /// of trying to catch up. `predicted_dt` is added back because egui
    /// subtracts it from the delay.
    pub fn delay_after(
        &mut self,
        frame_start: Instant,
        now: Instant,
        predicted_dt: Duration,
    ) -> Duration {
        if frame_start >= self.next {
            self.next += self.interval;
            if self.next <= frame_start {
                self.next = frame_start + self.interval;
            }
        }
        self.next.saturating_duration_since(now) + predicted_dt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREDICTED: Duration = Duration::from_micros(16_667);

    fn ms(value: f64) -> Duration {
        Duration::from_secs_f64(value / 1000.0)
    }

    fn assert_close(actual: Duration, expected: Duration) {
        let diff = actual.abs_diff(expected);
        assert!(
            diff <= Duration::from_micros(1),
            "{actual:?} is not {expected:?}"
        );
    }

    #[test]
    fn intervals_match_the_frame_rates() {
        assert_close(FrameRate::Fps60.interval(), ms(16.6667));
        assert_close(FrameRate::Fps30.interval(), ms(33.3333));
        assert_eq!(FrameRate::default(), FrameRate::Fps60);
        assert_eq!(FrameRate::Fps30.label(), "30 fps");
    }

    #[test]
    fn scheduled_frames_advance_by_one_interval_without_drift() {
        let t0 = Instant::now();
        let interval = FrameRate::Fps60.interval();
        let mut limiter = FrameLimiter::new(FrameRate::Fps60, t0);

        // Each frame starts 2 ms after its scheduled time and takes 3 ms.
        for k in 0..600u32 {
            let frame_start = t0 + interval * k + ms(2.0);
            let now = frame_start + ms(3.0);
            let delay = limiter.delay_after(frame_start, now, Duration::ZERO);
            assert_close(delay, interval - ms(5.0));
        }
        // After 600 frames the schedule is still anchored at t0.
        assert_eq!(limiter.next, t0 + interval * 600);
    }

    #[test]
    fn extra_frames_do_not_move_the_schedule() {
        let t0 = Instant::now();
        let interval = FrameRate::Fps60.interval();
        let mut limiter = FrameLimiter::new(FrameRate::Fps60, t0);
        limiter.delay_after(t0, t0, Duration::ZERO);
        assert_eq!(limiter.next, t0 + interval);

        // A mouse move repaints 5 ms later, before the next scheduled frame.
        let extra = t0 + ms(5.0);
        let delay = limiter.delay_after(extra, extra, Duration::ZERO);
        assert_eq!(limiter.next, t0 + interval);
        assert_close(delay, interval - ms(5.0));
    }

    #[test]
    fn a_late_frame_restarts_the_schedule() {
        let t0 = Instant::now();
        let interval = FrameRate::Fps30.interval();
        let mut limiter = FrameLimiter::new(FrameRate::Fps30, t0);

        // The window was hidden for a second; the frame is far behind.
        let late = t0 + Duration::from_secs(1);
        let delay = limiter.delay_after(late, late, Duration::ZERO);
        assert_eq!(limiter.next, late + interval);
        assert_eq!(delay, interval);
    }

    #[test]
    fn the_predicted_frame_time_is_added_back() {
        let t0 = Instant::now();
        let interval = FrameRate::Fps60.interval();
        let mut limiter = FrameLimiter::new(FrameRate::Fps60, t0);

        let delay = limiter.delay_after(t0, t0, PREDICTED);
        assert_eq!(delay, interval + PREDICTED);

        // Even when the next frame is due right now, egui still gets the
        // predicted time back and does not repaint immediately.
        let due = t0 + interval;
        let mut limiter = FrameLimiter::new(FrameRate::Fps60, t0);
        limiter.delay_after(t0, t0, Duration::ZERO);
        let delay = limiter.delay_after(t0 + ms(1.0), due, PREDICTED);
        assert_eq!(delay, PREDICTED);
    }

    #[test]
    fn changing_the_rate_restarts_the_schedule() {
        let t0 = Instant::now();
        let mut limiter = FrameLimiter::new(FrameRate::Fps60, t0);
        limiter.delay_after(t0, t0, Duration::ZERO);

        let now = t0 + ms(7.0);
        limiter.set_rate(FrameRate::Fps30, now);
        assert_eq!(limiter.rate(), FrameRate::Fps30);
        let delay = limiter.delay_after(now, now, Duration::ZERO);
        assert_eq!(delay, FrameRate::Fps30.interval());

        // Setting the same rate again changes nothing.
        let before = limiter.next;
        limiter.set_rate(FrameRate::Fps30, now + ms(1.0));
        assert_eq!(limiter.next, before);
    }
}
