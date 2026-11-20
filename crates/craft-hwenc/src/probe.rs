//! The one-frame test encode that tells whether a candidate works on this machine.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
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
    /// The test encode did not finish in the time it was given, and was stopped.
    TimedOut(Duration),
    /// The candidate was not tested, because the tests of one choice had used their time.
    NotTested(String),
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
            Outcome::TimedOut(after) => write!(f, "the test encode did not finish in {}", seconds(*after)),
            Outcome::NotTested(why) => write!(f, "it was not tested ({why})"),
        }
    }
}

/// `d` as seconds for a message: `5 s`, or `2.500 s` when it has a fraction.
pub(crate) fn seconds(d: Duration) -> String {
    match d.subsec_millis() {
        0 => format!("{} s", d.as_secs()),
        ms => format!("{}.{ms:03} s", d.as_secs()),
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

/// The time the limits of test encodes are measured with. [`SystemClock`] is the real one; a test
/// gives a clock that it moves forward itself, so a limit is reached without waiting for it.
pub trait Clock: Send + Sync {
    /// The time now.
    fn now(&self) -> Instant;
    /// Wait for `d` before the next look at a running test encode. A test clock moves forward by
    /// `d` instead.
    fn pause(&self, d: Duration);
}

/// The system's clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// How often a running test encode is looked at.
const POLL: Duration = Duration::from_millis(5);

/// A started program, as [`wait_for`] sees it. Tests give one that never ends.
pub(crate) trait Running {
    type Status;
    /// The exit status, once the program has ended.
    fn try_wait(&mut self) -> io::Result<Option<Self::Status>>;
    /// Stop the program and wait for it to end.
    fn stop(&mut self);
}

impl Running for Child {
    type Status = ExitStatus;
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }
    fn stop(&mut self) {
        let _ = self.kill();
        let _ = self.wait();
    }
}

/// How a wait for a program ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ended<S> {
    Exited(S),
    /// The program ran for the whole timeout and was stopped.
    TimedOut,
    /// `cancel` was set, and the program was stopped.
    Canceled,
    /// The program's state could not be read, and it was stopped.
    Failed(String),
}

/// Wait for `running` to end, looking at it every 5 ms of `clock`. The program is stopped when
/// `cancel` is set, when its state cannot be read, and once `timeout` has passed.
pub(crate) fn wait_for<R: Running>(running: &mut R, cancel: &Cancel, timeout: Duration, clock: &dyn Clock) -> Ended<R::Status> {
    let started = clock.now();
    loop {
        if cancel.is_canceled() {
            running.stop();
            return Ended::Canceled;
        }
        match running.try_wait() {
            Ok(Some(status)) => return Ended::Exited(status),
            Ok(None) => {
                let elapsed = clock.now().saturating_duration_since(started);
                if elapsed >= timeout {
                    running.stop();
                    return Ended::TimedOut;
                }
                clock.pause(POLL.min(timeout.saturating_sub(elapsed)));
            }
            Err(e) => {
                running.stop();
                return Ended::Failed(format!("ffmpeg's state could not be read: {e}"));
            }
        }
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
    /// Encode one frame of `input` with `candidate`, stopping after `limit` at the latest. By
    /// default it is [`Prober::probe`], which keeps its own limit.
    fn probe_within(&self, candidate: &'static Candidate, input: Input, cancel: &Cancel, limit: Duration) -> Outcome {
        let _ = limit;
        self.probe(candidate, input, cancel)
    }
    /// The longest one test encode runs when no shorter limit is given; `None` when the prober
    /// has no limit of its own. A test stopped before this time is tested again.
    fn timeout(&self) -> Option<Duration> {
        None
    }
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
    clock: Arc<dyn Clock>,
}

impl FfmpegProber {
    /// Test encodes in `program`, whose `ffmpeg -encoders` list is `listed` (empty when it could
    /// not be read: every candidate is then tried).
    pub fn new(program: impl Into<PathBuf>, listed: Vec<String>) -> Self {
        Self {
            program: program.into(),
            listed: (!listed.is_empty()).then_some(listed),
            timeout: Duration::from_secs(15),
            encode_args: None,
            clock: Arc::new(SystemClock),
        }
    }

    /// The longest one test encode may take before it is stopped and counted as timed out.
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

    /// Measure the timeouts on `clock` instead of the system's clock.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

impl Prober for FfmpegProber {
    fn program(&self) -> &Path {
        &self.program
    }
    fn probe(&self, candidate: &'static Candidate, input: Input, cancel: &Cancel) -> Outcome {
        self.probe_within(candidate, input, cancel, self.timeout)
    }
    fn probe_within(&self, candidate: &'static Candidate, input: Input, cancel: &Cancel, limit: Duration) -> Outcome {
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
        let clock = self.clock.as_ref();
        if candidate.api != Api::Vaapi {
            return probe_codec_on(&self.program, &args, input, cancel, self.timeout.min(limit), clock);
        }
        // Each render node is a GPU; the first one that encodes is used from then on. Each node
        // has the whole timeout, and all of them together have `limit`.
        let started = clock.now();
        let mut last = Outcome::NoDevice("no render node".into());
        for node in vaapi_devices() {
            let left = limit.saturating_sub(clock.now().saturating_duration_since(started));
            if left.is_zero() {
                return later_node(last, Outcome::NotTested(format!("no time was left for {node}")));
            }
            let o = probe_codec_on(&self.program, &with_vaapi_device(&args, &node), input, cancel, self.timeout.min(left), clock);
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
    fn timeout(&self) -> Option<Duration> {
        Some(self.timeout)
    }
    fn args(&self, candidate: &'static Candidate, input: Input) -> Vec<String> {
        self.encode_args.as_ref().and_then(|f| f(candidate, input)).unwrap_or_else(|| default_args(candidate, input))
    }
}

pub(crate) fn command(program: &Path) -> Command {
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

/// How a bounded run of a program ended.
pub(crate) enum Run {
    /// It ended, with this status and at most 64 KiB of the output that was read.
    Exited(ExitStatus, String),
    TimedOut,
    Canceled,
    /// It could not be started.
    NotStarted(String),
    Failed(String),
}

/// Run `cmd` with `stdin` written to its standard input, reading its standard output when
/// `stdout` is set and its error output otherwise. It is stopped when `cancel` is set or after
/// `timeout` on `clock`.
pub(crate) fn run_bounded(mut cmd: Command, stdin: Option<Vec<u8>>, stdout: bool, cancel: &Cancel, timeout: Duration, clock: &dyn Clock) -> Run {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    if stdout {
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Run::NotStarted(e.to_string()),
    };
    let writer = stdin.and_then(|frame| {
        child.stdin.take().map(|mut pipe| {
            std::thread::spawn(move || {
                // A write error means the program stopped reading; its exit status tells why.
                let _ = pipe.write_all(&frame);
            })
        })
    });
    let out: Option<Box<dyn Read + Send>> = if stdout {
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>)
    } else {
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)
    };
    // A read on this thread would block the timeout check, so the output is read on its own.
    let reader = out.map(|pipe| {
        std::thread::spawn(move || {
            let mut log = String::new();
            let _ = pipe.take(MAX_LOG).read_to_string(&mut log);
            log
        })
    });
    let ended = wait_for(&mut child, cancel, timeout, clock);
    // The program has ended or was stopped, so both pipes are closed and the threads end.
    if let Some(w) = writer {
        let _ = w.join();
    }
    let log = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    match ended {
        Ended::Exited(status) => Run::Exited(status, log),
        Ended::TimedOut => Run::TimedOut,
        Ended::Canceled => Run::Canceled,
        Ended::Failed(why) => Run::Failed(why),
    }
}

/// Encode one gray [`PROBE_WIDTH`] × [`PROBE_HEIGHT`] frame of `input` with the encode
/// arguments `encode` (at least `-c:v <codec>`), and throw the result away. The encode is stopped
/// when `cancel` is set or after `timeout`.
pub fn probe_codec(program: &Path, encode: &[String], input: Input, cancel: &Cancel, timeout: Duration) -> Outcome {
    probe_codec_on(program, encode, input, cancel, timeout, &SystemClock)
}

/// [`probe_codec`] with its timeout measured on `clock`.
fn probe_codec_on(program: &Path, encode: &[String], input: Input, cancel: &Cancel, timeout: Duration, clock: &dyn Clock) -> Outcome {
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
    // Gray: luma and both chroma at mid scale (P010 keeps the value in the high bits).
    let frame: Vec<u8> = match input {
        Input::Nv12 => vec![0x80; bytes],
        Input::P010 => [0x00, 0x80].repeat(bytes / 2),
    };
    match run_bounded(cmd, Some(frame), false, cancel, timeout, clock) {
        Run::Exited(s, _) if s.success() => Outcome::Works,
        Run::Exited(s, log) => match classify(&log) {
            Outcome::Failed(why) if why.is_empty() => Outcome::Failed(format!("ffmpeg exited with {s} without an error message")),
            other => other,
        },
        Run::TimedOut => Outcome::TimedOut(timeout),
        Run::Canceled => Outcome::Canceled,
        Run::NotStarted(e) => Outcome::FfmpegMissing(format!("{} could not be started: {e}", program.display())),
        Run::Failed(why) => Outcome::Failed(why),
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
