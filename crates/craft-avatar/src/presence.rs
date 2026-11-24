//! What the avatar does when the face is lost and found again.
//!
//! [`Presence`] turns "is a face tracked in this frame" into a factor from 0 to 1: how much of
//! the tracked pose the avatar shows, the rest being the idle pose. When the face is lost the
//! factor holds at its value for [`LostFace::hold`] seconds, then eases to 0 over
//! [`LostFace::fade_out`]. When a face is found it eases back to 1 over [`LostFace::fade_in`].
//! Times come from the caller, so tests drive it with any clock.

use craft_ease::{EaseDir, EaseFamily, Easing};
use serde::{Deserialize, Serialize};

use crate::AvatarError;
use crate::expression::ExpressionName;

/// Longest hold or fade, in seconds.
pub const MAX_LOST_SECONDS: f64 = 10.0;

/// The timings of losing and finding the face, in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LostFace {
    pub hold: f64,
    pub fade_out: f64,
    pub fade_in: f64,
}

impl Default for LostFace {
    /// Hold 0.5 s, ease to idle over 1 s, ease back over 0.25 s.
    fn default() -> LostFace {
        LostFace { hold: 0.5, fade_out: 1.0, fade_in: 0.25 }
    }
}

impl LostFace {
    pub fn validate(&self) -> Result<(), AvatarError> {
        for (v, what) in [(self.hold, "hold"), (self.fade_out, "fade out"), (self.fade_in, "fade in")] {
            if !(0.0..=MAX_LOST_SECONDS).contains(&v) {
                return Err(AvatarError::Invalid(format!("the face-lost {what} time must be 0 to 10 seconds")));
            }
        }
        Ok(())
    }
}

/// What the avatar shows while the face is lost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum WhenLost {
    /// Hold the last pose, then ease to the idle pose.
    #[default]
    HoldThenIdle,
    /// Keep the last tracked pose until the face returns.
    HoldLast,
    /// Ease to the idle pose with this expression turned on.
    Expression { name: ExpressionName },
}

/// Where the face tracking stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceState {
    /// A face is tracked.
    Tracking,
    /// The face was just lost and the last pose is held.
    Holding,
    /// The avatar is easing to, or showing, the idle pose.
    Lost,
    /// A face was found and the avatar is easing back to it.
    Returning,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Tracking,
    Holding { since: f64, factor: f32 },
    Leaving { since: f64, from: f32 },
    Idle,
    Returning { since: f64, from: f32 },
}

/// The lost-face state machine.
#[derive(Clone, Debug)]
pub struct Presence {
    timing: LostFace,
    hold_forever: bool,
    phase: Phase,
    factor: f32,
    last: Option<f64>,
    ease: Easing,
}

impl Presence {
    /// A machine that starts in the idle pose, with no face found yet.
    pub fn new(timing: LostFace, when_lost: &WhenLost) -> Presence {
        Presence {
            timing,
            hold_forever: matches!(when_lost, WhenLost::HoldLast),
            phase: Phase::Idle,
            factor: 0.0,
            last: None,
            ease: Easing::preset(EaseFamily::Sine, EaseDir::InOut),
        }
    }

    /// The share of the tracked pose, 0 to 1.
    pub fn factor(&self) -> f32 {
        self.factor
    }

    pub fn state(&self) -> PresenceState {
        match self.phase {
            Phase::Tracking => PresenceState::Tracking,
            Phase::Holding { .. } => PresenceState::Holding,
            Phase::Leaving { .. } | Phase::Idle => PresenceState::Lost,
            Phase::Returning { .. } => PresenceState::Returning,
        }
    }

    /// Advances to `now` (seconds) with or without a face and returns the factor. A time that
    /// is not finite, or earlier than the last one, is taken as the last one.
    pub fn step(&mut self, now: f64, face: bool) -> f32 {
        let now = match self.last {
            Some(last) if !now.is_finite() || now < last => last,
            None if !now.is_finite() => 0.0,
            _ => now,
        };
        self.last = Some(now);
        // Each pass either settles or moves to a later phase, so three passes cover a long step
        // from holding through leaving to idle.
        for _ in 0..3 {
            let next = self.next(now, face);
            if next == self.phase {
                break;
            }
            self.phase = next;
        }
        self.factor = self.value(now);
        self.factor
    }

    fn progress(&self, now: f64, since: f64, length: f64) -> f64 {
        if length <= 0.0 { 1.0 } else { ((now - since) / length).clamp(0.0, 1.0) }
    }

    fn value(&self, now: f64) -> f32 {
        match self.phase {
            Phase::Tracking => 1.0,
            Phase::Holding { factor, .. } => factor,
            Phase::Idle => 0.0,
            Phase::Leaving { since, from } => {
                let p = self.ease.apply(self.progress(now, since, self.timing.fade_out)) as f32;
                from * (1.0 - p)
            }
            Phase::Returning { since, from } => {
                let p = self.ease.apply(self.progress(now, since, self.timing.fade_in)) as f32;
                from + (1.0 - from) * p
            }
        }
    }

    fn next(&self, now: f64, face: bool) -> Phase {
        let current = self.value(now);
        match (self.phase, face) {
            (Phase::Tracking, true) => Phase::Tracking,
            (Phase::Tracking, false) => Phase::Holding { since: now, factor: 1.0 },
            (Phase::Holding { factor, .. }, true) => {
                if factor >= 1.0 {
                    Phase::Tracking
                } else {
                    Phase::Returning { since: now, from: factor }
                }
            }
            (Phase::Holding { since, factor }, false) => {
                if !self.hold_forever && now - since >= self.timing.hold {
                    Phase::Leaving { since: since + self.timing.hold, from: factor }
                } else {
                    self.phase
                }
            }
            (Phase::Leaving { since, .. }, false) => {
                if self.progress(now, since, self.timing.fade_out) >= 1.0 {
                    Phase::Idle
                } else {
                    self.phase
                }
            }
            (Phase::Leaving { .. } | Phase::Idle, true) => Phase::Returning { since: now, from: current },
            (Phase::Idle, false) => Phase::Idle,
            (Phase::Returning { since, .. }, true) => {
                if self.progress(now, since, self.timing.fade_in) >= 1.0 {
                    Phase::Tracking
                } else {
                    self.phase
                }
            }
            (Phase::Returning { .. }, false) => Phase::Holding { since: now, factor: current },
        }
    }
}
