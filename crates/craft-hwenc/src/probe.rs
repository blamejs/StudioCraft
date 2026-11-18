//! The one-frame test encode that tells whether a candidate works on this machine.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::candidate::{use_vaapi_device, with_vaapi_device};
use crate::{Api, Candidate, Input, vaapi_devices};

/// The size of the test frame. Every listed encoder takes it; the smallest sizes they document
/// are below it.
pub const PROBE_WIDTH: u32 = 256;
pub const PROBE_HEIGHT: u32 = 144;

/// The most of ffmpeg's error output that is kept.
const MAX_LOG: u64 = 64 * 1024;

/// What a test encode found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The frame was encoded.
    Works,
    /// This ffmpeg was built without the candidate's codec.
    NotListed,
    /// The candidate does not take the input format.
    Unsupported,
    /// No device or driver for the API was found.
    NoDevice(String),
    /// The device has no free encoding session.
    Busy(String),
    /// The encode failed for another reason.
    Failed(String),
    /// The ffmpeg program was not found or could not be started.
    FfmpegMissing(String),
    /// The test was canceled before it finished.
    Canceled,
}

impl Outcome {
    pub fn works(&self) -> bool {
        *self == Outcome::Works
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Works => f.write_str("it works"),
            Outcome::NotListed => f.write_str("this ffmpeg does not have it"),
            Outcome::Unsupported => f.write_str("it does not take this pixel format"),
            Outcome::NoDevice(why) => write!(f, "no device for it was found ({why})"),
            Outcome::Busy(why) => write!(f, "the device has no free encoding session ({why})"),
            Outcome::Failed(why) => f.write_str(why),
            Outcome::FfmpegMissing(why) => write!(f, "ffmpeg could not be run ({why})"),
            Outcome::Canceled => f.write_str("the test was canceled"),
        }
    }
}

/// A flag that stops test encodes. Clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Runs test encodes. [`FfmpegProber`] runs them in ffmpeg; tests give their own.
pub trait Prober: Sync {
    /// The ffmpeg program the encodes run in; outcomes are cached per program.
    fn program(&self) -> &Path;
    /// The program `candidate`'s encodes run in: by default [`Prober::program`]. Its outcomes and
    /// its failures while encoding are kept under this path.
    fn runs_in(&self, candidate: &'static Candidate) -> &Path {
        let _ = candidate;
        self.program()
    }
    /// Encode one frame of `input` with `candidate`.
    fn probe(&self, candidate: &'static Candidate, input: Input, cancel: &Cancel) -> Outcome;
    /// The arguments after the input that the test encode of `candidate` runs with: by default
    /// [`Candidate::prelude`] and `-c:v <codec>`. Outcomes are cached per argument list.
    fn args(&self, candidate: &'static Candidate, input: Input) -> Vec<String> {
        default_args(candidate, input)
    }
}

fn default_args(candidate: &'static Candidate, input: Input) -> Vec<String> {
    let mut a = candidate.prelude(input);
    a.extend(["-c:v".to_string(), candidate.ffmpeg_codec.to_string()]);
    a
}

/// The encode arguments of a candidate, as an output would run it; `None` uses the defaults.
pub type EncodeArgs = Box<dyn Fn(&'static Candidate, Input) -> Option<Vec<String>> + Send + Sync>;

/// Test encodes in an ffmpeg program.
pub struct FfmpegProber {
    program: PathBuf,
    listed: Option<Vec<String>>,
    timeout: Duration,
    encode_args: Option<EncodeArgs>,
}

impl FfmpegProber {
    /// Test encodes in `program`, whose `ffmpeg -encoders` list is `listed` (empty when it could
    /// not be read: every candidate is then tried).
    pub fn new(program: impl Into<PathBuf>, listed: Vec<String>) -> Self {
        Self { program: program.into(), listed: (!listed.is_empty()).then_some(listed), timeout: Duration::from_secs(15), encode_args: None }
    }

    /// The longest one test encode may take before it is stopped and counted as failed.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Test each candidate with the arguments `f` gives (prelude, `-c:v` and options), so a
    /// candidate passes only with the options the output uses.
    pub fn with_encode_args(mut self, f: EncodeArgs) -> Self {
        self.encode_args = Some(f);
        self
    }
}

impl Prober for FfmpegProber {
    fn program(&self) -> &Path {
        &self.program
    }
    fn probe(&self, candidate: &'static Candidate, input: Input, cancel: &Cancel) -> Outcome {
        if cancel.is_canceled() {
            return Outcome::Canceled;
        }
        if !candidate.accepts(input) {
            return Outcome::Unsupported;
        }
        if self.listed.as_ref().is_some_and(|l| !l.iter().any(|c| c == candidate.ffmpeg_codec)) {
            return Outcome::NotListed;
        }
        let args = self.args(candidate, input);
        if candidate.api != Api::Vaapi {
            return probe_codec(&self.program, &args, input, cancel, self.timeout);
        }
        // Each render node is a GPU; the first one that encodes is used from then on.
        let mut last = Outcome::NoDevice("no render node".into());
        for node in vaapi_devices() {
            let o = probe_codec(&self.program, &with_vaapi_device(&args, &node), input, cancel, self.timeout);
            match o {
                Outcome::Works => {
                    use_vaapi_device(candidate, &node);
                    return o;
                }
                Outcome::Canceled | Outcome::FfmpegMissing(_) => return o,
                o => last = later_node(last, o),
            }
        }
        last
    }
    fn args(&self, candidate: &'static Candidate, input: Input) -> Vec<String> {
        self.encode_args.as_ref().and_then(|f| f(candidate, input)).unwrap_or_else(|| default_args(candidate, input))
    }
}

fn command(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// The result of the render nodes tested so far, `last` before them and `o` for the next one,
/// when neither encoded: a busy node is kept, since it encodes once its session is free.
pub(crate) fn later_node(last: Outcome, o: Outcome) -> Outcome {
    if matches!(last, Outcome::Busy(_)) { last } else { o }
}

/// Encode one gray [`PROBE_WIDTH`] × [`PROBE_HEIGHT`] frame of `input` with the encode
/// arguments `encode` (at least `-c:v <codec>`), and throw the result away. The encode is stopped
/// when `cancel` is set or after `timeout`.
pub fn probe_codec(program: &Path, encode: &[String], input: Input, cancel: &Cancel, timeout: Duration) -> Outcome {
    if cancel.is_canceled() {
        return Outcome::Canceled;
    }
    if !program.is_file() {
        return Outcome::FfmpegMissing(format!("{} was not found", program.display()));
    }
    let Some(bytes) = input.frame_bytes(PROBE_WIDTH, PROBE_HEIGHT) else {
        return Outcome::Failed("the test frame has no size".into());
    };
    let size = format!("{PROBE_WIDTH}x{PROBE_HEIGHT}");
    let mut cmd = command(program);
    cmd.args(["-hide_banner", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", input.ffmpeg_pix_fmt(), "-s", &size, "-r", "30", "-i", "pipe:0"]);
    cmd.args(["-frames:v", "1"]);
    cmd.args(encode);
    cmd.args(["-f", "null", "-"]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::FfmpegMissing(format!("{} could not be started: {e}", program.display())),
    };
    // Gray: luma and both chroma at mid scale (P010 keeps the value in the high bits).
    let frame: Vec<u8> = match input {
        Input::Nv12 => vec![0x80; bytes],
        Input::P010 => [0x00, 0x80].repeat(bytes / 2),
    };
    let writer = child.stdin.take().map(|mut stdin| {
        std::thread::spawn(move || {
            // A write error means ffmpeg stopped reading; its exit status tells why.
            let _ = stdin.write_all(&frame);
        })
    });
    let reader = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut log = String::new();
            let _ = stderr.take(MAX_LOG).read_to_string(&mut log);
            log
        })
    });
    let started = Instant::now();
    let status = loop {
        if cancel.is_canceled() {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::Canceled;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("the test encode did not finish in {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                break Err(format!("ffmpeg's state could not be read: {e}"));
            }
        }
    };
    if let Some(w) = writer {
        let _ = w.join();
    }
    let log = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    match status {
        Ok(s) if s.success() => Outcome::Works,
        Ok(s) => match classify(&log) {
            Outcome::Failed(why) if why.is_empty() => Outcome::Failed(format!("ffmpeg exited with {s} without an error message")),
            other => other,
        },
        Err(why) => Outcome::Failed(why),
    }
}

/// What ffmpeg's error output says about a failed encode.
pub fn classify(log: &str) -> Outcome {
    let line = log.lines().map(str::trim).filter(|l| !l.is_empty()).find(|l| !l.starts_with("[in#") && !l.starts_with("Input #"));
    let first: String = line.unwrap_or_default().chars().take(200).collect();
    let lower = log.to_ascii_lowercase();
    const BUSY: [&str; 3] = ["incompatible client key", "openencodesessionex failed: out of memory", "no free encoding session"];
    const NO_DEVICE: [&str; 14] = [
        // ffmpeg writes this one with two spaces after "create".
        "hardware device context",
        "amfqueryversion failed",
        "no capable devices found",
        "no nvenc capable devices",
        "cannot load nvcuda",
        "cannot load libcuda",
        "cannot load libnvidia-encode",
        "amfrt64.dll failed to open",
        "amfrt32.dll failed to open",
        "failed to initialise vaapi",
        "no va display found",
        "error creating a mfx session",
        "error initializing an internal mfx session",
        "could not find any mft",
    ];
    if BUSY.iter().any(|p| lower.contains(p)) {
        Outcome::Busy(first)
    } else if NO_DEVICE.iter().any(|p| lower.contains(p)) {
        Outcome::NoDevice(first)
    } else {
        Outcome::Failed(first)
    }
}
