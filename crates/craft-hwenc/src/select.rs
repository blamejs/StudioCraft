//! The user's encoder preference, the session's cache of test results, and the choice they give.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::candidate::with_vaapi_device;
use crate::probe::seconds;
use crate::{Api, Cancel, Candidate, Clock, Codec, Input, Os, Outcome, Prober, SystemClock, Vendor, auto_order, candidate};

/// Which encoder to use.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Preference {
    /// The first candidate in [`auto_order`] whose test encode works, else the software encoder.
    Auto,
    /// The software encoder.
    Software,
    /// The candidate with this id; an error when its test encode fails.
    Named(String),
}

impl Preference {
    /// The preference a settings value names: `auto` (or empty), `software`, or a candidate id.
    pub fn parse(value: &str) -> Preference {
        match value.trim() {
            "" | "auto" => Preference::Auto,
            "software" => Preference::Software,
            id => Preference::Named(id.to_string()),
        }
    }

    /// The settings value of [`Preference::parse`].
    pub fn id(&self) -> &str {
        match self {
            Preference::Auto => "auto",
            Preference::Software => "software",
            Preference::Named(id) => id,
        }
    }
}

/// What a preference resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Hardware(&'static Candidate),
    /// The software encoder, with the candidates that were tried and what each test found.
    Software {
        tried: Vec<(&'static Candidate, Outcome)>,
    },
}

/// Why a preference gave no encoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The tests were canceled.
    Canceled,
    /// The preference names no candidate.
    Unknown(String),
    /// The named candidate did not pass its test.
    Failed { candidate: &'static Candidate, outcome: Outcome },
    /// The named candidate cannot do what the target asks: another codec, or lossless.
    Mismatch { candidate: &'static Candidate, why: &'static str },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Canceled => f.write_str("the encoder tests were canceled"),
            Error::Unknown(id) => write!(f, "there is no hardware encoder `{id}`"),
            Error::Failed { candidate, outcome } => write!(f, "{} did not pass its test encode: {outcome}", candidate.name),
            Error::Mismatch { candidate, why } => write!(f, "{} {why}", candidate.name),
        }
    }
}

impl std::error::Error for Error {}

/// One kept test: the ffmpeg program, the candidate, the input format and the encode arguments.
pub(crate) type Key = (PathBuf, &'static str, Input, Vec<String>);

/// The test outcomes of one session, per ffmpeg program, candidate, input format and encode
/// arguments. Canceled tests, a missing ffmpeg, a device with no free session, a candidate that was
/// not tested and a test stopped before the prober's own timeout are not kept, so they are tried
/// again. [`ProbeCache::save`] and [`ProbeCache::load`] keep the outcomes between sessions.
#[derive(Debug, Default)]
pub struct ProbeCache {
    pub(crate) outcomes: Mutex<HashMap<Key, Outcome>>,
    /// Candidates that failed while they encoded, per ffmpeg program, with the error.
    pub(crate) failed: Mutex<HashMap<(PathBuf, &'static str), String>>,
}

/// The encode arguments a test is kept under. A VA-API candidate's render node is left out: the
/// node its test passed on is kept apart, and the encodes that follow use it.
pub(crate) fn key_args(candidate: &Candidate, args: Vec<String>) -> Vec<String> {
    if candidate.api == Api::Vaapi { with_vaapi_device(&args, "") } else { args }
}

/// Whether `prober`'s outcome `o` is kept for the session.
fn kept(prober: &dyn Prober, o: &Outcome) -> bool {
    match o {
        // A busy device has a free session again once another program stops encoding.
        Outcome::Canceled | Outcome::FfmpegMissing(_) | Outcome::Busy(_) | Outcome::NotTested(_) => false,
        // A test that a shorter limit stopped could still finish in the prober's own time.
        Outcome::TimedOut(after) => prober.timeout().is_none_or(|t| *after >= t),
        _ => true,
    }
}

impl ProbeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The kept outcome of `candidate` with `prober`'s program and arguments for `input`.
    pub fn get(&self, prober: &dyn Prober, candidate: &'static Candidate, input: Input) -> Option<Outcome> {
        let program = prober.runs_in(candidate).to_path_buf();
        if let Some(why) = self.failed.lock().unwrap_or_else(PoisonError::into_inner).get(&(program.clone(), candidate.id)) {
            return Some(Outcome::Failed(why.clone()));
        }
        let key = (program, candidate.id, input, key_args(candidate, prober.args(candidate, input)));
        self.outcomes.lock().unwrap_or_else(PoisonError::into_inner).get(&key).cloned()
    }

    /// The kept outcome of `candidate`, or a new test with `prober`.
    pub fn outcome(&self, prober: &dyn Prober, candidate: &'static Candidate, input: Input, cancel: &Cancel) -> Outcome {
        if let Some(o) = self.get(prober, candidate, input) {
            return o;
        }
        // The lock is not held during the test, so another thread can read other outcomes.
        let o = prober.probe(candidate, input, cancel);
        self.keep(prober, candidate, input, o)
    }

    /// The kept outcome of `candidate`, or a new test with `prober` that stops after `limit` at
    /// the latest.
    pub fn outcome_within(&self, prober: &dyn Prober, candidate: &'static Candidate, input: Input, cancel: &Cancel, limit: Duration) -> Outcome {
        if let Some(o) = self.get(prober, candidate, input) {
            return o;
        }
        let o = prober.probe_within(candidate, input, cancel, limit);
        self.keep(prober, candidate, input, o)
    }

    fn keep(&self, prober: &dyn Prober, candidate: &'static Candidate, input: Input, o: Outcome) -> Outcome {
        if kept(prober, &o) {
            let key = (prober.runs_in(candidate).to_path_buf(), candidate.id, input, key_args(candidate, prober.args(candidate, input)));
            self.outcomes.lock().unwrap_or_else(PoisonError::into_inner).insert(key, o.clone());
        }
        o
    }

    /// Record that `candidate` failed while it encoded in `program`, so the automatic choice
    /// skips it for the rest of the session, whatever its arguments.
    pub fn mark_failed(&self, program: &Path, candidate: &'static Candidate, why: impl Into<String>) {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner).insert((program.to_path_buf(), candidate.id), why.into());
    }

    /// A cache with this one's mid-stream failures and no test outcomes, for a start while
    /// another output holds an encoder: a new test then shows whether a second session opens.
    pub fn without_outcomes(&self) -> ProbeCache {
        let failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner).clone();
        ProbeCache { outcomes: Mutex::default(), failed: Mutex::new(failed) }
    }

    /// Forget every outcome (after a driver or ffmpeg change).
    pub fn clear(&self) {
        self.outcomes.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.failed.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

/// What an encoder is chosen for: the graphics adapter's vendor, the system, the codec and the
/// input format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub vendor: Vendor,
    pub os: Os,
    pub codec: Codec,
    pub input: Input,
    /// Whether the encode is lossless.
    pub lossless: bool,
}

/// The time [`resolve`] gives the tests of one choice.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(30);

/// The time the tests of one choice may take together, measured on `clock`. Once it has passed,
/// the candidates that have no kept outcome are reported as [`Outcome::NotTested`], and a test
/// that is running is stopped by [`Prober::probe_within`]. A prober that implements only
/// [`Prober::probe`] stops a running test at its own limit instead.
#[derive(Clone, Copy)]
pub struct Budget<'a> {
    pub total: Duration,
    pub clock: &'a dyn Clock,
}

impl Budget<'static> {
    /// `total` on the system's clock.
    pub fn new(total: Duration) -> Self {
        Budget { total, clock: &SystemClock }
    }
}

/// The encoder `preference` gives for `target`, testing candidates with `prober` within
/// [`DEFAULT_BUDGET`] and keeping the outcomes in `cache`.
pub fn resolve(preference: &Preference, target: Target, prober: &dyn Prober, cache: &ProbeCache, cancel: &Cancel) -> Result<Choice, Error> {
    resolve_within(preference, target, prober, cache, cancel, Budget::new(DEFAULT_BUDGET))
}

/// [`resolve`] with the tests together taking at most `budget`.
pub fn resolve_within(
    preference: &Preference,
    target: Target,
    prober: &dyn Prober,
    cache: &ProbeCache,
    cancel: &Cancel,
    budget: Budget<'_>,
) -> Result<Choice, Error> {
    let started = budget.clock.now();
    let left = || budget.total.saturating_sub(budget.clock.now().saturating_duration_since(started));
    let outcome = |c: &'static Candidate| match cache.get(prober, c, target.input) {
        Some(o) => o,
        None if left().is_zero() => Outcome::NotTested(format!("the tests had used their {}", seconds(budget.total))),
        None => cache.outcome_within(prober, c, target.input, cancel, left()),
    };
    match preference {
        Preference::Software => Ok(Choice::Software { tried: Vec::new() }),
        Preference::Named(id) => {
            let c = candidate(id).ok_or_else(|| Error::Unknown(id.clone()))?;
            if !c.runs_on(target.os) {
                return Err(Error::Failed { candidate: c, outcome: Outcome::NoDevice("its API is not on this system".into()) });
            }
            if c.codec != target.codec {
                return Err(Error::Mismatch { candidate: c, why: "writes another codec than this output takes" });
            }
            if target.lossless && !c.lossless() {
                return Err(Error::Mismatch { candidate: c, why: "cannot record losslessly" });
            }
            match outcome(c) {
                Outcome::Works => Ok(Choice::Hardware(c)),
                Outcome::Canceled => Err(Error::Canceled),
                outcome => Err(Error::Failed { candidate: c, outcome }),
            }
        }
        Preference::Auto => {
            let mut tried = Vec::new();
            for c in auto_order(target.vendor, target.os, target.codec).into_iter().filter(|c| !target.lossless || c.lossless()) {
                match outcome(c) {
                    Outcome::Works => return Ok(Choice::Hardware(c)),
                    Outcome::Canceled => return Err(Error::Canceled),
                    // Without ffmpeg no other candidate can run either.
                    o @ Outcome::FfmpegMissing(_) => {
                        tried.push((c, o));
                        break;
                    }
                    o => tried.push((c, o)),
                }
            }
            Ok(Choice::Software { tried })
        }
    }
}
