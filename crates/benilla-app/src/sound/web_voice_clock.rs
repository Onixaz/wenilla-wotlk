//! The clock of a native Web Audio voice (`web_voice`): which [`PlaybackState`] it is in and where
//! in the file it plays, read off the `AudioContext` clock, so a handle answers the same questions
//! kira's does without asking the browser. Apart from Web Audio, so it is tested natively.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use kira::sound::PlaybackState;

/// What the voice is doing, with the context times its fades end at.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Playing,
    /// Fading to a pause that lands at `at`.
    Pausing {
        at: f64,
    },
    /// Held at `offset` seconds into the file.
    Paused {
        offset: f64,
    },
    /// Fading to a stop that lands at `at`.
    Stopping {
        at: f64,
    },
}

/// A voice's timeline on the context clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct VoiceClock {
    /// The file's length, seconds.
    duration: f64,
    /// Where a loop restarts, seconds; `None` plays once.
    loop_start: Option<f64>,
    /// The context time at which the file stood at `origin_offset`.
    origin_time: f64,
    origin_offset: f64,
    phase: Phase,
}

impl VoiceClock {
    /// A voice started at `now`, `offset` seconds into a `duration`-second file.
    pub(super) fn new(now: f64, offset: f64, duration: f64, loop_start: Option<f64>) -> Self {
        Self {
            duration,
            loop_start: loop_start.filter(|s| *s < duration),
            origin_time: now,
            origin_offset: offset,
            phase: Phase::Playing,
        }
    }

    /// The file offset playing at context time `t`, while the voice runs.
    fn offset_at(&self, t: f64) -> f64 {
        let raw = self.origin_offset + (t - self.origin_time).max(0.0);
        match self.loop_start {
            Some(start) if raw >= self.duration => {
                start + (raw - self.duration) % (self.duration - start)
            }
            _ => raw.min(self.duration),
        }
    }

    /// Where in the file the voice is at `now`, seconds.
    pub(super) fn position(&self, now: f64) -> f64 {
        match self.phase {
            Phase::Playing => self.offset_at(now),
            Phase::Pausing { at } | Phase::Stopping { at } => self.offset_at(now.min(at)),
            Phase::Paused { offset } => offset,
        }
    }

    /// kira's state for the voice at `now`.
    pub(super) fn state(&self, now: f64) -> PlaybackState {
        let ran_out = |t: f64| {
            self.loop_start.is_none()
                && self.origin_offset + (t - self.origin_time) >= self.duration
        };
        match self.phase {
            Phase::Playing if ran_out(now) => PlaybackState::Stopped,
            Phase::Playing => PlaybackState::Playing,
            Phase::Pausing { at } if now >= at => PlaybackState::Paused,
            Phase::Pausing { .. } => PlaybackState::Pausing,
            Phase::Paused { .. } => PlaybackState::Paused,
            Phase::Stopping { at } if now >= at || ran_out(now) => PlaybackState::Stopped,
            Phase::Stopping { .. } => PlaybackState::Stopping,
        }
    }

    /// Stop over `fade` seconds from `now`; returns when the source should end. A stopped voice
    /// stays stopped.
    pub(super) fn stop(&mut self, now: f64, fade: f64) -> f64 {
        let at = match self.phase {
            Phase::Stopping { at } => at.min(now + fade),
            _ => now + fade,
        };
        if let Phase::Paused { .. } = self.phase {
            self.phase = Phase::Stopping { at: now };
            return now;
        }
        self.phase = Phase::Stopping { at };
        at
    }

    /// Pause over `fade` seconds from `now`; returns when the source should end, or `None` when
    /// there is nothing playing to pause.
    pub(super) fn pause(&mut self, now: f64, fade: f64) -> Option<f64> {
        match self.state(now) {
            PlaybackState::Playing => {
                let at = now + fade;
                self.phase = Phase::Pausing { at };
                Some(at)
            }
            _ => None,
        }
    }

    /// Resume at `now`; returns the file offset the new source starts at, or `None` when the
    /// voice is not paused or pausing.
    pub(super) fn resume(&mut self, now: f64) -> Option<f64> {
        let offset = match self.phase {
            Phase::Paused { offset } => offset,
            Phase::Pausing { at } => self.offset_at(now.min(at)),
            _ => return None,
        };
        self.origin_time = now;
        self.origin_offset = offset;
        self.phase = Phase::Playing;
        Some(offset)
    }

    /// Settle a pause whose fade has landed, so [`Self::resume`] and [`Self::position`] read
    /// the held offset.
    pub(super) fn settle(&mut self, now: f64) {
        if let Phase::Pausing { at } = self.phase {
            if now >= at {
                self.phase = Phase::Paused {
                    offset: self.offset_at(at),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_shot_plays_through_and_stops_at_its_end() {
        let c = VoiceClock::new(10.0, 0.0, 60.0, None);
        assert_eq!(c.state(10.0), PlaybackState::Playing);
        assert!((c.position(40.0) - 30.0).abs() < 1e-9);
        assert_eq!(c.state(70.0), PlaybackState::Stopped);
        assert_eq!(c.position(80.0), 60.0);
    }

    #[test]
    fn a_loop_wraps_to_its_start_and_never_ends() {
        let c = VoiceClock::new(0.0, 0.0, 10.0, Some(2.0));
        assert!(
            (c.position(12.0) - 4.0).abs() < 1e-9,
            "{}",
            c.position(12.0)
        );
        assert_eq!(c.state(1000.0), PlaybackState::Playing);
    }

    #[test]
    fn a_start_offset_counts_toward_the_end() {
        let c = VoiceClock::new(0.0, 50.0, 60.0, None);
        assert_eq!(c.state(9.0), PlaybackState::Playing);
        assert_eq!(c.state(10.0), PlaybackState::Stopped);
    }

    #[test]
    fn a_stop_fades_then_lands() {
        let mut c = VoiceClock::new(0.0, 0.0, 60.0, None);
        assert_eq!(c.stop(5.0, 3.0), 8.0);
        assert_eq!(c.state(6.0), PlaybackState::Stopping);
        assert_eq!(c.state(8.0), PlaybackState::Stopped);
        assert!((c.position(20.0) - 8.0).abs() < 1e-9);
        // A shorter stop on top of a running fade lands sooner, never later.
        assert_eq!(c.stop(6.0, 0.5), 6.5);
    }

    #[test]
    fn a_pause_holds_its_offset_and_a_resume_continues_from_it() {
        let mut c = VoiceClock::new(0.0, 0.0, 60.0, None);
        assert_eq!(c.pause(10.0, 1.0), Some(11.0));
        assert_eq!(c.state(10.5), PlaybackState::Pausing);
        c.settle(12.0);
        assert_eq!(c.state(30.0), PlaybackState::Paused);
        assert!((c.position(30.0) - 11.0).abs() < 1e-9);
        assert_eq!(c.resume(40.0), Some(11.0));
        assert!((c.position(45.0) - 16.0).abs() < 1e-9);
        assert_eq!(c.state(40.0 + 49.0), PlaybackState::Stopped);
    }

    #[test]
    fn a_resume_mid_fade_picks_up_where_it_is_and_nothing_else_resumes() {
        let mut c = VoiceClock::new(0.0, 0.0, 60.0, None);
        c.pause(10.0, 2.0);
        assert_eq!(c.resume(11.0), Some(11.0));
        assert_eq!(c.resume(12.0), None, "a playing voice is not resumed");
        let mut s = VoiceClock::new(0.0, 0.0, 60.0, None);
        s.stop(1.0, 0.0);
        assert_eq!(s.resume(2.0), None, "a stopped voice is not resumed");
        assert_eq!(s.pause(2.0, 0.0), None);
    }

    #[test]
    fn stopping_a_paused_voice_lands_at_once() {
        let mut c = VoiceClock::new(0.0, 0.0, 60.0, None);
        c.pause(1.0, 0.0);
        c.settle(1.0);
        assert_eq!(c.stop(5.0, 2.0), 5.0);
        assert_eq!(c.state(5.0), PlaybackState::Stopped);
    }
}
