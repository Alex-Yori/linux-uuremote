//! Content-adaptive frame gating.
//!
//! Recovered from the official controlled-host engine (`libstreamer.so`, source tree
//! `nrd-streamer`), which logs:
//!
//!     create video frame dropper moving_fps:{} (raw:{}) still_fps:{}
//!     not requesting refresh frame because of soon happening idle repeat
//!
//! i.e. the official host does NOT encode at the configured frame rate. It captures
//! continuously ("raw"), then drops to a low keepalive rate (`still_fps`, observed
//! ~3 fps on an idle desktop) whenever the screen stops changing, and only returns to
//! `moving_fps` while content actually changes.
//!
//! This module is the pure decision logic; the capture backend feeds it change
//! notifications. Kept free of I/O so it can be unit-tested deterministically.

use std::time::{Duration, Instant};

/// What the pipeline should do for this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Read pixels and encode a fresh frame.
    Capture,
    /// Screen unchanged: re-emit the last frame cheaply (keeps the decoder alive).
    Keepalive,
    /// Do nothing this tick.
    Idle,
}

#[derive(Debug, Clone, Copy)]
pub struct GateConfig {
    /// Ceiling on captured frames per second while the screen is changing.
    pub moving_fps: u32,
    /// Keepalive rate while the screen is unchanged. `0` disables keepalive entirely.
    pub still_fps: u32,
    /// How long after the last observed change the screen is still considered moving.
    /// Guards against flickering between the two regimes on sparse changes.
    pub still_after: Duration,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            moving_fps: 60,
            still_fps: 3,
            still_after: Duration::from_millis(1000),
        }
    }
}

impl GateConfig {
    fn moving_interval(&self) -> Duration {
        if self.moving_fps == 0 {
            Duration::ZERO
        } else {
            Duration::from_nanos(1_000_000_000 / self.moving_fps as u64)
        }
    }

    fn still_interval(&self) -> Duration {
        if self.still_fps == 0 {
            Duration::ZERO
        } else {
            Duration::from_nanos(1_000_000_000 / self.still_fps as u64)
        }
    }
}

/// Moving/still state machine + paced rate limiter.
///
/// Pacing tracks the *ideal* next deadline rather than the last actual capture time, so
/// tick jitter does not silently shave the frame rate: at 60 fps over 1 s of 1 ms ticks
/// a "last capture + interval" limiter drifts to 59 captures, this one holds 60.
#[derive(Debug)]
pub struct FrameDropper {
    cfg: GateConfig,
    moving: bool,
    last_change: Option<Instant>,
    next_capture: Option<Instant>,
    next_keepalive: Option<Instant>,
    /// Counters useful for logs and for the capture probe's before/after report.
    pub captures: u64,
    pub keepalives: u64,
    pub idles: u64,
}

impl FrameDropper {
    pub fn new(cfg: GateConfig) -> Self {
        Self {
            cfg,
            moving: false,
            last_change: None,
            next_capture: None,
            next_keepalive: None,
            captures: 0,
            keepalives: 0,
            idles: 0,
        }
    }

    /// Current gate configuration (kept for the session layer that reconfigures the gate).
    #[allow(dead_code)]
    pub fn config(&self) -> GateConfig {
        self.cfg
    }

    /// Replace the gate configuration in place, e.g. when a remote session sets fps.
    #[allow(dead_code)]
    pub fn set_config(&mut self, cfg: GateConfig) {
        self.cfg = cfg;
    }

    pub fn is_moving(&self) -> bool {
        self.moving
    }

    /// Report that screen content changed at `now`.
    pub fn note_change(&mut self, now: Instant) {
        self.moving = true;
        self.last_change = Some(now);
    }

    /// Advance a paced clock along the ideal deadline grid.
    ///
    /// Keeping `base + gap` whenever it is still in the future preserves the frame grid
    /// (tick quantization would otherwise shave the rate: 60 fps over 1 ms ticks drifts to
    /// 59). If the grid deadline has already passed, the caller was stalled, so resync to
    /// `now + gap` and drop the missed frames instead of emitting a catch-up burst.
    fn advance(clock: &mut Option<Instant>, now: Instant, gap: Duration) {
        let base = clock.unwrap_or(now);
        let deadline = base + gap;
        *clock = Some(if deadline <= now { now + gap } else { deadline });
    }

    /// Decide what to emit at `now`.
    pub fn decide(&mut self, now: Instant) -> Action {
        // Leave the moving regime once nothing changed for `still_after`.
        if self.moving {
            if let Some(last) = self.last_change {
                if now.duration_since(last) >= self.cfg.still_after {
                    self.moving = false;
                }
            }
        }

        if self.moving {
            if self.next_capture.is_none_or(|due| now >= due) {
                Self::advance(&mut self.next_capture, now, self.cfg.moving_interval());
                // A fresh capture also serves as a keepalive.
                self.next_keepalive = Some(now + self.cfg.still_interval());
                self.captures += 1;
                return Action::Capture;
            }
            self.idles += 1;
            return Action::Idle;
        }

        if self.cfg.still_fps == 0 {
            self.idles += 1;
            return Action::Idle;
        }

        if self.next_keepalive.is_none_or(|due| now >= due) {
            Self::advance(&mut self.next_keepalive, now, self.cfg.still_interval());
            self.keepalives += 1;
            return Action::Keepalive;
        }
        self.idles += 1;
        Action::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    /// While content keeps changing, captures are capped at moving_fps — not at the
    /// tick rate of the caller.
    #[test]
    fn moving_caps_at_moving_fps() {
        let mut d = FrameDropper::new(GateConfig {
            moving_fps: 60,
            still_fps: 3,
            still_after: Duration::from_millis(1000),
        });
        for ms in 0..1000u64 {
            d.note_change(t(ms));
            d.decide(t(ms));
        }
        assert_eq!(d.captures, 60, "one second of continuous change -> 60 captures");
        assert!(d.is_moving());
    }

    /// A stop in content drops to the keepalive regime after `still_after`.
    #[test]
    fn idle_drops_to_still_fps() {
        let mut d = FrameDropper::new(GateConfig {
            moving_fps: 60,
            still_fps: 3,
            still_after: Duration::from_millis(1000),
        });
        for ms in 0..500u64 {
            d.note_change(t(ms));
            d.decide(t(ms));
        }
        let captures_at_stop = d.captures;
        // 1.5s .. 2.5s : quiet window. Transition happens at 1500ms (last change 499ms).
        for ms in 1500..2500u64 {
            d.decide(t(ms));
        }
        assert!(!d.is_moving());
        assert_eq!(d.captures, captures_at_stop, "no fresh captures while still");
        assert!(d.keepalives >= 3 && d.keepalives <= 4, "keepalive ~3fps, got {}", d.keepalives);
    }

    /// `still_fps = 0` means genuinely zero output on an idle screen.
    #[test]
    fn still_fps_zero_sends_nothing() {
        let mut d = FrameDropper::new(GateConfig {
            moving_fps: 60,
            still_fps: 0,
            still_after: Duration::from_millis(500),
        });
        d.note_change(t(0));
        d.decide(t(0));
        for ms in 600..2000u64 {
            d.decide(t(ms));
        }
        assert_eq!(d.keepalives, 0);
        assert_eq!(d.captures, 1);
    }

    /// Changes resume the moving regime immediately, without waiting a full still period.
    #[test]
    fn change_resumes_moving_immediately() {
        let mut d = FrameDropper::new(GateConfig {
            moving_fps: 60,
            still_fps: 3,
            still_after: Duration::from_millis(1000),
        });
        d.note_change(t(0));
        d.decide(t(0));
        d.decide(t(1200));
        assert!(!d.is_moving());
        d.note_change(t(1300));
        assert_eq!(d.decide(t(1301)), Action::Capture);
        assert!(d.is_moving());
    }

    /// Hysteresis: sparse changes (one per 900ms with still_after=1s) must not thrash
    /// into the keepalive regime.
    #[test]
    fn sparse_changes_do_not_thrash() {
        let mut d = FrameDropper::new(GateConfig {
            moving_fps: 60,
            still_fps: 3,
            still_after: Duration::from_millis(1000),
        });
        for second in 0..5u64 {
            d.note_change(t(second * 900));
            for ms in (second * 900)..(second * 900 + 800) {
                d.decide(t(ms));
            }
        }
        assert!(d.is_moving());
        assert_eq!(d.keepalives, 0);
    }

    /// A stalled caller gets exactly one frame on resume, not a catch-up burst.
    #[test]
    fn stall_does_not_catch_up() {
        let mut d = FrameDropper::new(GateConfig {
            moving_fps: 60,
            still_fps: 3,
            still_after: Duration::from_millis(10_000),
        });
        d.note_change(t(0));
        assert_eq!(d.decide(t(0)), Action::Capture);
        // 500 ms without any ticks, while still inside the moving window.
        let resumed = [500u64, 501, 502, 503]
            .iter()
            .filter(|&&ms| d.decide(t(ms)) == Action::Capture)
            .count();
        assert_eq!(resumed, 1, "no catch-up burst after a stall");
    }
}
