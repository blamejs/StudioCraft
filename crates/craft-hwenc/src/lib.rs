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
//!   while it ran.
//! - [`resolve`] turns a [`Preference`] into a [`Choice`]: in [`Preference::Auto`] the candidates
//!   are tried in [`auto_order`], which puts the adapter vendor's own API first, and the software
//!   encoder is the fallback.
//!
//! The crate runs ffmpeg as a separate process and has no dependencies.

mod candidate;
mod probe;
mod select;

pub use candidate::{Api, CANDIDATES, Candidate, Codec, Input, Os, Vendor, auto_order, candidate, vaapi_device, vaapi_devices};
pub use probe::{Cancel, EncodeArgs, FfmpegProber, Outcome, PROBE_HEIGHT, PROBE_WIDTH, Prober, classify, probe_codec};
pub use select::{Choice, Error, Preference, ProbeCache, Target, resolve};

#[cfg(test)]
mod tests;
