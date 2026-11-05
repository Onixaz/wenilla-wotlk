//! The depth policy of the page's audio queue (`web_backend`): how far ahead of the
//! `AudioContext` clock the mix is rendered. Apart from Web Audio, so it is tested natively.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// The queue kept while playback is smooth, seconds: about what cpal's two buffers held.
pub(super) const MIN_LEAD_S: f64 = 0.05;
/// The deepest the queue grows, seconds.
const MAX_LEAD_S: f64 = 0.5;
/// Smooth playback for this long shrinks the queue one step, seconds: from the cap back to the
/// floor in about a minute, so a load's underruns do not leave sound effects late for long.
const SHRINK_AFTER_S: f64 = 2.0;
/// One shrink step, seconds.
const SHRINK_STEP_S: f64 = 0.01;
/// The queue's depth policy, apart from Web Audio so it is testable natively.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Lead {
    /// The queue to keep ahead of the clock, seconds.
    pub(super) secs: f64,
    /// When `secs` last changed, on the context clock.
    since: f64,
}

/// What a refill does before it renders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Refill {
    /// The cursor is ahead of the clock: render up to the lead.
    OnTime,
    /// The first refill: start the cursor just ahead of the clock.
    Start,
    /// The queue ran dry: one gap, then a deeper queue.
    Underrun,
}

impl Lead {
    pub(super) fn new(now: f64) -> Self {
        Self {
            secs: MIN_LEAD_S,
            since: now,
        }
    }

    /// The queue to keep ahead of the clock now, seconds.
    pub(super) fn target(&self) -> f64 {
        self.secs
    }

    /// Classify a refill at `now` with the cursor at `cursor` (0 before the first chunk), and
    /// adjust the depth for it.
    pub(super) fn refill(&mut self, now: f64, cursor: f64) -> Refill {
        if cursor == 0.0 {
            return Refill::Start;
        }
        if cursor <= now {
            self.secs = (self.secs * 2.0).min(MAX_LEAD_S);
            self.since = now;
            return Refill::Underrun;
        }
        if now - self.since >= SHRINK_AFTER_S && self.secs > MIN_LEAD_S {
            self.secs = (self.secs - SHRINK_STEP_S).max(MIN_LEAD_S);
            self.since = now;
        }
        Refill::OnTime
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_refill_starts_the_cursor() {
        let mut lead = Lead::new(1.0);
        assert_eq!(lead.refill(1.0, 0.0), Refill::Start);
        assert_eq!(lead.secs, MIN_LEAD_S);
    }

    #[test]
    fn an_underrun_doubles_the_queue_up_to_its_cap() {
        let mut lead = Lead::new(0.0);
        assert_eq!(lead.refill(5.0, 4.9), Refill::Underrun);
        assert_eq!(lead.secs, MIN_LEAD_S * 2.0);
        for t in 0..10 {
            lead.refill(6.0 + f64::from(t), 5.0);
        }
        assert_eq!(lead.secs, MAX_LEAD_S);
    }

    #[test]
    fn smooth_playback_shrinks_the_queue_a_step_at_a_time_to_its_floor() {
        let mut lead = Lead::new(0.0);
        lead.refill(1.0, 0.5); // underrun: 100 ms
        let deep = lead.secs;
        assert_eq!(
            lead.refill(1.0 + SHRINK_AFTER_S - 0.1, 100.0),
            Refill::OnTime
        );
        assert_eq!(lead.secs, deep, "not yet");
        lead.refill(1.0 + SHRINK_AFTER_S, 100.0);
        assert!((lead.secs - (deep - SHRINK_STEP_S)).abs() < 1e-9);
        for k in 2..20 {
            lead.refill(1.0 + SHRINK_AFTER_S * f64::from(k), 1000.0);
        }
        assert_eq!(lead.secs, MIN_LEAD_S);
    }
}
