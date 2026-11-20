//! Hardware video encoders run through the ffmpeg program.
//!
//! - [`CANDIDATES`] lists the hardware encoders ffmpeg can run: NVENC, AMF, Quick Sync, Media
//!   Foundation, VideoToolbox and VA-API, for H.264, HEVC and AV1. [`Candidate::prelude`] gives the
//!   arguments a candidate needs before its codec arguments (VA-API uploads the frames to the
//!   device).
//! - [`FfmpegProber`] encodes one frame with a candidate to find out whether it works on this
//!   machine. A listed encoder can still fail: no device of its vendor, a missing driver, or no free
//!   encoding session. [`FfmpegProber::with_encode_args`] tests each candidate with the options
//!   the output uses. [`classify`] turns ffmpeg's error output into an [`Outcome`].
//! - [`ProbeCache`] keeps each outcome for the session, per ffmpeg program, candidate, input
//!   format and encode arguments. [`ProbeCache::mark_failed`] records a candidate that failed
//!   while it ran. [`ProbeCache::save`] and [`ProbeCache::load`] keep the outcomes in a file
//!   between sessions, for one [`Fingerprint`]: the ffmpeg program, its version and the driver.
//! - [`resolve`] turns a [`Preference`] into a [`Choice`]: in [`Preference::Auto`] the candidates
//!   are tried in [`auto_order`], which puts the adapter vendor's own API first, and the software
//!   encoder is the fallback. [`resolve_within`] bounds the time all the tests take together; the
//!   timeouts are measured on a [`Clock`].
//!
//! The crate runs ffmpeg as a separate process and has no dependencies.

mod candidate;
mod disk;
mod probe;
mod select;

pub use candidate::{Api, CANDIDATES, Candidate, Codec, Input, Os, Vendor, auto_order, candidate, vaapi_device, vaapi_devices};
pub use disk::{CACHE_VERSION, Fingerprint, Loaded, MAX_CACHE_BYTES, ffmpeg_version, program_stamp};
pub use probe::{Cancel, Clock, EncodeArgs, FfmpegProber, Outcome, PROBE_HEIGHT, PROBE_WIDTH, Prober, SystemClock, classify, probe_codec};
pub use select::{Budget, Choice, DEFAULT_BUDGET, Error, Preference, ProbeCache, Target, resolve, resolve_within};

#[cfg(test)]
mod tests;
