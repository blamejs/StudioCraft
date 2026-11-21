//! Test outcomes kept between sessions, in a file tied to the ffmpeg program and the driver.

use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::time::{Duration, UNIX_EPOCH};

use crate::candidate::{passed_vaapi_device, use_vaapi_device, with_vaapi_device};
use crate::probe::{Run, command, run_bounded};
use crate::select::{Key, key_args};
use crate::{Api, Cancel, Candidate, Clock, Input, Outcome, ProbeCache, SystemClock, candidate, vaapi_devices};

/// The first line of a saved file, followed by its format version.
const HEADER: &str = "craft-hwenc test outcomes";

/// The format of the saved file. A file of another version is not read.
pub const CACHE_VERSION: u32 = 1;

/// The largest saved file that is read.
pub const MAX_CACHE_BYTES: u64 = 1024 * 1024;

/// The most outcomes a saved file holds.
const MAX_ENTRIES: usize = 4096;

/// What the kept outcomes are valid for: the ffmpeg program, its version and the graphics driver.
/// A file saved for another fingerprint is not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// The ffmpeg program.
    pub program: PathBuf,
    /// The program's version: see [`ffmpeg_version`].
    pub ffmpeg_version: String,
    /// The graphics adapter and its driver version, as the caller describes them.
    pub driver: String,
    /// Other program paths whose outcomes are kept with the ffmpeg program's, such as encoders
    /// that run in the calling process and depend on the driver alone.
    pub also: Vec<PathBuf>,
}

impl Fingerprint {
    /// The fingerprint of `program` with [`ffmpeg_version`], and `driver`.
    pub fn of(program: &Path, driver: impl Into<String>, cancel: &Cancel, timeout: Duration) -> Fingerprint {
        Fingerprint {
            program: program.to_path_buf(),
            ffmpeg_version: ffmpeg_version(program, cancel, timeout),
            driver: driver.into(),
            also: Vec::new(),
        }
    }

    fn holds(&self, program: &Path) -> bool {
        self.program == program || self.also.iter().any(|p| p == program)
    }
}

/// The version of the ffmpeg program at `program`: the first line of `ffmpeg -version`, the
/// program file's size and its modification time. A rebuilt or replaced program with the same
/// version line gets another value. `-version` is stopped after `timeout`, and its line is then
/// `unknown`.
pub fn ffmpeg_version(program: &Path, cancel: &Cancel, timeout: Duration) -> String {
    ffmpeg_version_on(program, cancel, timeout, &SystemClock)
}

fn ffmpeg_version_on(program: &Path, cancel: &Cancel, timeout: Duration, clock: &dyn Clock) -> String {
    let mut cmd = command(program);
    cmd.args(["-hide_banner", "-version"]);
    let line = match run_bounded(cmd, None, true, cancel, timeout, clock) {
        Run::Exited(s, out) if s.success() => out.lines().next().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string),
        _ => None,
    };
    let mut v = line.unwrap_or_else(|| "unknown".into());
    v.push_str(&program_stamp(program).unwrap_or_default());
    v
}

/// The size and modification time of the program file at `program`, as the end of
/// [`ffmpeg_version`] gives them, without running it; `None` when the file cannot be read, as for
/// a program found on the `PATH`.
pub fn program_stamp(program: &Path) -> Option<String> {
    let meta = std::fs::metadata(program).ok()?;
    let mut v = format!("; {} bytes", meta.len());
    if let Some(t) = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        v.push_str(&format!("; modified {}.{:09}", t.as_secs(), t.subsec_nanos()));
    }
    Some(v)
}

/// What [`ProbeCache::load`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    /// There is no saved file.
    Missing,
    /// This many outcomes were read.
    Read(usize),
    /// The file was saved for another ffmpeg program, ffmpeg version, driver or file version;
    /// nothing was read.
    Stale,
    /// The file could not be read or is damaged; nothing was read.
    Corrupt(String),
}

impl fmt::Display for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Loaded::Missing => f.write_str("no saved test outcomes"),
            Loaded::Read(n) => write!(f, "{n} saved test outcomes read"),
            Loaded::Stale => f.write_str("the saved test outcomes are for another ffmpeg or driver"),
            Loaded::Corrupt(why) => write!(f, "the saved test outcomes could not be read: {why}"),
        }
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            other => return Err(format!("an unknown escape \\{}", other.map(String::from).unwrap_or_default())),
        }
    }
    Ok(out)
}

fn input_name(input: Input) -> &'static str {
    match input {
        Input::Nv12 => "nv12",
        Input::P010 => "p010",
    }
}

fn input_of(name: &str) -> Option<Input> {
    match name {
        "nv12" => Some(Input::Nv12),
        "p010" => Some(Input::P010),
        _ => None,
    }
}

/// The saved form of an outcome that is kept between sessions. Timeouts, busy devices and the
/// outcomes a session does not keep are tested again in the next session.
fn saved(o: &Outcome) -> Option<(&'static str, &str)> {
    match o {
        Outcome::Works => Some(("works", "")),
        Outcome::NotListed => Some(("not-listed", "")),
        Outcome::Unsupported => Some(("unsupported", "")),
        Outcome::NoDevice(why) => Some(("no-device", why)),
        Outcome::Failed(why) => Some(("failed", why)),
        _ => None,
    }
}

fn outcome_of(kind: &str, detail: String) -> Option<Outcome> {
    match kind {
        "works" => Some(Outcome::Works),
        "not-listed" => Some(Outcome::NotListed),
        "unsupported" => Some(Outcome::Unsupported),
        "no-device" => Some(Outcome::NoDevice(detail)),
        "failed" => Some(Outcome::Failed(detail)),
        _ => None,
    }
}

/// The header line `name` with its value.
fn field<'a>(lines: &mut impl Iterator<Item = &'a str>, name: &str) -> Result<String, String> {
    let line = lines.next().ok_or_else(|| format!("the `{name}` line is missing"))?;
    match line.split_once('\t') {
        Some((n, v)) if n == name => unescape(v),
        _ => Err(format!("the `{name}` line is missing")),
    }
}

/// The outcomes in `text`, or `None` when it was saved for another fingerprint or version.
fn parse(text: &str, fingerprint: &Fingerprint) -> Result<Option<Vec<(Key, Outcome)>>, String> {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let version = first.strip_prefix(HEADER).and_then(|v| v.strip_prefix(' ')).ok_or("the first line is not a test outcome file")?;
    if version.parse::<u32>().map_err(|_| format!("the version `{version}` is not a number"))? != CACHE_VERSION {
        return Ok(None);
    }
    let program = PathBuf::from(field(&mut lines, "program")?);
    let ffmpeg = field(&mut lines, "ffmpeg")?;
    let driver = field(&mut lines, "driver")?;
    let mut also = Vec::new();
    let mut entries = Vec::new();
    for line in lines {
        let mut parts = line.split('\t');
        match parts.next() {
            Some("also") if entries.is_empty() => also.push(PathBuf::from(unescape(parts.next().unwrap_or_default())?)),
            Some("outcome") => {
                if entries.len() >= MAX_ENTRIES {
                    return Err(format!("it holds more than {MAX_ENTRIES} outcomes"));
                }
                let mut next = |what: &str| parts.next().ok_or_else(|| format!("an outcome has no {what}"));
                let at = PathBuf::from(unescape(next("program")?)?);
                let id = next("encoder")?;
                let c = candidate(id).ok_or_else(|| format!("there is no encoder `{id}`"))?;
                let input = next("input format")?;
                let input = input_of(input).ok_or_else(|| format!("there is no input format `{input}`"))?;
                let kind = next("result")?;
                let detail = unescape(next("detail")?)?;
                let o = outcome_of(kind, detail).ok_or_else(|| format!("there is no result `{kind}`"))?;
                let args = parts.map(unescape).collect::<Result<Vec<_>, _>>()?;
                entries.push(((at, c.id, input, args), o));
            }
            Some("") if line.is_empty() => {}
            _ => return Err(format!("the line `{}` is not understood", line.chars().take(40).collect::<String>())),
        }
    }
    let saved_for = Fingerprint { program, ffmpeg_version: ffmpeg, driver, also };
    if saved_for != *fingerprint {
        return Ok(None);
    }
    if let Some(((at, ..), _)) = entries.iter().find(|((at, ..), _)| !fingerprint.holds(at)) {
        return Err(format!("an outcome is for {}, which the file does not name", at.display()));
    }
    Ok(Some(entries))
}

/// The VA-API candidate of a saved test and the render node it ran on, when `devices` (this
/// system's render nodes) still has that node.
pub(crate) fn saved_vaapi_node<'a>(key: &'a Key, devices: &[String]) -> Option<(&'static Candidate, &'a str)> {
    let (_, id, _, args) = key;
    let c = candidate(id).filter(|c| c.api == Api::Vaapi)?;
    let node = args.iter().position(|a| a == "-vaapi_device").and_then(|i| args.get(i + 1))?;
    devices.contains(node).then_some((c, node.as_str()))
}

impl ProbeCache {
    /// Read the outcomes saved at `path` for `fingerprint` into this cache. An outcome this cache
    /// already has is kept. A file for another fingerprint, a damaged file and a file larger than
    /// [`MAX_CACHE_BYTES`] add nothing.
    pub fn load(&self, path: &Path, fingerprint: &Fingerprint) -> Loaded {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Loaded::Missing,
            Err(e) => return Loaded::Corrupt(e.to_string()),
        };
        let too_large = || Loaded::Corrupt(format!("it is larger than {} KiB", MAX_CACHE_BYTES / 1024));
        match file.metadata() {
            Ok(m) if m.len() > MAX_CACHE_BYTES => return too_large(),
            Ok(_) => {}
            Err(e) => return Loaded::Corrupt(e.to_string()),
        }
        let mut bytes = Vec::new();
        // The file can grow after the size check; at most one byte past the limit is read.
        if let Err(e) = file.take(MAX_CACHE_BYTES.saturating_add(1)).read_to_end(&mut bytes) {
            return Loaded::Corrupt(e.to_string());
        }
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CACHE_BYTES {
            return too_large();
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return Loaded::Corrupt("it is not UTF-8 text".into());
        };
        match parse(&text, fingerprint) {
            Err(why) => Loaded::Corrupt(why),
            Ok(None) => Loaded::Stale,
            Ok(Some(entries)) => {
                let mut n = 0;
                let mut outcomes = self.outcomes.lock().unwrap_or_else(PoisonError::into_inner);
                let devices = vaapi_devices();
                for (key, o) in entries {
                    // A VA-API test that passed is used only on the render node it passed on, while
                    // this system has that node and no test of this session chose another one.
                    if o == Outcome::Works && key.3.iter().any(|a| a == "-vaapi_device") {
                        let Some((c, node)) = saved_vaapi_node(&key, &devices) else { continue };
                        match passed_vaapi_device(c) {
                            None => use_vaapi_device(c, node),
                            Some(p) if p == node => {}
                            Some(_) => continue,
                        }
                    }
                    n += 1;
                    let (at, id, input, args) = key;
                    let args = match candidate(id) {
                        Some(c) => key_args(c, args),
                        None => args,
                    };
                    outcomes.entry((at, id, input, args)).or_insert(o);
                }
                Loaded::Read(n)
            }
        }
    }

    /// Save this cache's outcomes for `fingerprint`'s programs to `path`, replacing the file, and
    /// give how many were saved. Outcomes of a candidate that failed while it encoded are left out,
    /// so the next session tests it again. The file is written next to `path` first and then
    /// renamed over it.
    pub fn save(&self, path: &Path, fingerprint: &Fingerprint) -> io::Result<usize> {
        let failed: HashSet<(PathBuf, &'static str)> = self.failed.lock().unwrap_or_else(PoisonError::into_inner).keys().cloned().collect();
        let mut rows: Vec<String> = self
            .outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((at, id, ..), _)| fingerprint.holds(at) && !failed.contains(&(at.clone(), *id)))
            .filter_map(|((at, id, input, args), o)| {
                let (kind, detail) = saved(o)?;
                // A passing VA-API test is saved with the render node it passed on.
                let node = candidate(id).filter(|c| c.api == Api::Vaapi && *o == Outcome::Works).and_then(passed_vaapi_device);
                let args = node.map_or_else(|| args.clone(), |n| with_vaapi_device(args, &n));
                let mut row = format!("outcome\t{}\t{id}\t{}\t{kind}\t{}", escape(&at.to_string_lossy()), input_name(*input), escape(detail));
                for a in &args {
                    row.push('\t');
                    row.push_str(&escape(a));
                }
                Some(row)
            })
            .collect();
        rows.sort();
        let mut text = format!(
            "{HEADER} {CACHE_VERSION}\nprogram\t{}\nffmpeg\t{}\ndriver\t{}\n",
            escape(&fingerprint.program.to_string_lossy()),
            escape(&fingerprint.ffmpeg_version),
            escape(&fingerprint.driver)
        );
        for p in &fingerprint.also {
            text.push_str(&format!("also\t{}\n", escape(&p.to_string_lossy())));
        }
        // The file stays within what `load` reads: the outcomes past either limit are left out.
        let limit = usize::try_from(MAX_CACHE_BYTES).unwrap_or(usize::MAX);
        if text.len() > limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("the fingerprint is larger than {} KiB", MAX_CACHE_BYTES / 1024)));
        }
        let mut count = 0;
        for r in rows.iter().take(MAX_ENTRIES) {
            if text.len() + r.len() + 1 > limit {
                break;
            }
            text.push_str(r);
            text.push('\n');
            count += 1;
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut temp = path.as_os_str().to_owned();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, text)?;
        if let Err(e) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        Ok(count)
    }
}
