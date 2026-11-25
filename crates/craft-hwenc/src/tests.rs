use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::*;

/// A prober that answers from a table and records each test it was asked for.
struct Fake {
    answers: HashMap<&'static str, Outcome>,
    asked: Mutex<Vec<&'static str>>,
    /// Set the cancel flag during the first test, as a user canceling mid-way.
    cancel_on_first: bool,
}

impl Fake {
    fn new(answers: &[(&'static str, Outcome)]) -> Self {
        Fake { answers: answers.iter().cloned().collect(), asked: Mutex::new(Vec::new()), cancel_on_first: false }
    }
    fn asked(&self) -> Vec<&'static str> {
        self.asked.lock().unwrap().clone()
    }
}

impl Prober for Fake {
    fn program(&self) -> &Path {
        Path::new("fake-ffmpeg")
    }
    fn probe(&self, c: &'static Candidate, _: Input, cancel: &Cancel) -> Outcome {
        let mut asked = self.asked.lock().unwrap();
        asked.push(c.id);
        if self.cancel_on_first && asked.len() == 1 {
            cancel.cancel();
            return Outcome::Canceled;
        }
        self.answers.get(c.id).cloned().unwrap_or_else(|| Outcome::NoDevice("fake".into()))
    }
}

fn target(vendor: Vendor, os: Os) -> Target {
    Target { vendor, os, codec: Codec::H264, input: Input::Nv12, lossless: false }
}

#[test]
fn a_busy_device_is_tested_again_and_named_encoders_must_fit_the_target() {
    let fake = Fake::new(&[("nvenc_h264", Outcome::Busy("incompatible client key".into()))]);
    let cache = ProbeCache::new();
    let nvenc = candidate("nvenc_h264").unwrap();
    let cancel = Cancel::new();
    assert!(matches!(cache.outcome(&fake, nvenc, Input::Nv12, &cancel), Outcome::Busy(_)));
    assert_eq!(cache.get(&fake, nvenc, Input::Nv12), None, "a busy device is not kept");
    let pref = Preference::Named("nvenc_hevc".into());
    let err = resolve(&pref, target(Vendor::Nvidia, Os::Windows), &fake, &cache, &cancel).unwrap_err();
    assert!(matches!(err, Error::Mismatch { .. }), "{err}");
    let lossless = Target { lossless: true, ..target(Vendor::Amd, Os::Windows) };
    let err = resolve(&Preference::Named("amf_h264".into()), lossless, &fake, &cache, &cancel).unwrap_err();
    assert_eq!(err.to_string(), "AMD AMF H.264 cannot record losslessly");
    let order: Vec<_> = match resolve(&Preference::Auto, lossless, &Fake::new(&[]), &cache, &cancel).unwrap() {
        Choice::Software { tried } => tried.iter().map(|(c, _)| c.id).collect(),
        Choice::Hardware(c) => vec![c.id],
    };
    assert_eq!(order, ["nvenc_h264"], "only NVENC H.264 runs losslessly on Windows");
}

fn ids(list: &[&'static Candidate]) -> Vec<&'static str> {
    list.iter().map(|c| c.id).collect()
}

#[test]
fn the_automatic_order_puts_the_adapter_vendors_api_first_then_the_systems() {
    let nvidia = auto_order(Vendor::from_pci_id(0x10de), Os::Windows, Codec::H264);
    assert_eq!(ids(&nvidia), ["nvenc_h264", "mf_h264", "amf_h264", "qsv_h264"]);
    let intel = auto_order(Vendor::from_pci_id(0x8086), Os::Windows, Codec::Hevc);
    assert_eq!(ids(&intel), ["qsv_hevc", "mf_hevc", "nvenc_hevc", "amf_hevc"]);
    let nvidia_av1 = auto_order(Vendor::from_pci_id(0x10de), Os::Windows, Codec::Av1);
    assert_eq!(ids(&nvidia_av1), ["nvenc_av1", "mf_av1", "amf_av1", "qsv_av1"]);
    let amd_linux = auto_order(Vendor::from_pci_id(0x1002), Os::Linux, Codec::Av1);
    assert_eq!(ids(&amd_linux), ["vaapi_av1", "nvenc_av1", "qsv_av1"], "AMF is not offered on Linux; VA-API runs AMD there");
    let apple = auto_order(Vendor::from_pci_id(0x106b), Os::MacOs, Codec::H264);
    assert_eq!(ids(&apple), ["vt_h264"]);
    let unknown = auto_order(Vendor::from_pci_id(0), Os::Linux, Codec::H264);
    assert_eq!(ids(&unknown), ["vaapi_h264", "nvenc_h264", "qsv_h264"]);
    assert!(auto_order(Vendor::Nvidia, Os::Other, Codec::H264).is_empty());
}

#[test]
fn every_candidate_has_a_unique_id_and_names_its_codec() {
    let mut seen = std::collections::HashSet::new();
    for c in &CANDIDATES {
        assert!(seen.insert(c.id), "{} twice", c.id);
        let prefix = match c.codec {
            Codec::H264 => "h264_",
            Codec::Hevc => "hevc_",
            Codec::Av1 => "av1_",
        };
        assert!(c.ffmpeg_codec.starts_with(prefix), "{}", c.ffmpeg_codec);
        assert_eq!(candidate(c.id), Some(c));
    }
    assert_eq!(candidate("x264"), None);
}

#[test]
fn ten_bit_input_is_for_hevc_and_av1_and_vaapi_uploads_in_the_input_format() {
    let h264 = candidate("nvenc_h264").unwrap();
    let hevc = candidate("nvenc_hevc").unwrap();
    assert!(h264.accepts(Input::Nv12) && !h264.accepts(Input::P010));
    assert!(hevc.accepts(Input::P010));
    assert!(!candidate("mf_hevc").unwrap().accepts(Input::P010));
    assert!(h264.prelude(Input::Nv12).is_empty());
    let vaapi = candidate("vaapi_hevc").unwrap().prelude(Input::P010);
    let vaapi_hevc = candidate("vaapi_hevc").unwrap();
    assert_eq!(vaapi, ["-vaapi_device".to_string(), vaapi_device(vaapi_hevc), "-vf".into(), "format=p010le,hwupload".into()]);
    use crate::candidate::{order_render_nodes, use_vaapi_device, with_vaapi_device};
    // Each candidate keeps the node its own test passed on.
    let vaapi_av1 = candidate("vaapi_av1").unwrap();
    use_vaapi_device(vaapi_av1, "/dev/dri/renderD131");
    assert_eq!(vaapi_device(vaapi_av1), "/dev/dri/renderD131");
    assert_eq!(vaapi_av1.prelude(Input::Nv12)[1], "/dev/dri/renderD131");
    assert_eq!(vaapi_device(vaapi_hevc), vaapi_devices()[0], "another candidate is not moved");
    assert_eq!(order_render_nodes(Vec::new()), ["/dev/dri/renderD128"], "no nodes listed");
    let busy = Outcome::Busy("no free session".into());
    let none = Outcome::NoDevice("no encoder".into());
    assert_eq!(crate::probe::later_node(busy.clone(), none.clone()), busy, "a busy node is tried again later");
    assert_eq!(crate::probe::later_node(none, busy.clone()), busy);
    assert_eq!(
        order_render_nodes(vec![(130, Some(0x1002)), (128, Some(0x10de)), (129, None)]),
        ["/dev/dri/renderD129", "/dev/dri/renderD130", "/dev/dri/renderD128"],
        "number order, NVIDIA last: its driver has no VA-API encoder"
    );
    let moved = with_vaapi_device(&vaapi, "/dev/dri/renderD129");
    assert_eq!(moved[..2], ["-vaapi_device", "/dev/dri/renderD129"]);
    assert_eq!(moved[2..], vaapi[2..]);
    assert_eq!(with_vaapi_device(&["-c:v".to_string(), "h264_nvenc".to_string()], "x"), ["-c:v", "h264_nvenc"], "no device: unchanged");
    assert_eq!(Input::Nv12.frame_bytes(4, 2), Some(12));
    assert_eq!(Input::P010.frame_bytes(4, 2), Some(24));
    assert_eq!(Input::Nv12.frame_bytes(3, 2), None, "4:2:0 needs even sides");
    assert_eq!(Input::P010.frame_bytes(u32::MAX - 1, u32::MAX - 1), None, "a size past u64 is no size");
}

#[test]
fn outcomes_are_kept_per_encode_arguments() {
    let p = FfmpegProber::new("no-such-dir/ffmpeg", Vec::new())
        .with_encode_args(Box::new(|c, _| (c.id == "nvenc_h264").then(|| vec!["-c:v".into(), "h264_nvenc".into(), "-cq".into(), "23".into()])));
    let nvenc = candidate("nvenc_h264").unwrap();
    assert_eq!(p.args(nvenc, Input::Nv12), ["-c:v", "h264_nvenc", "-cq", "23"]);
    assert_eq!(p.args(candidate("amf_h264").unwrap(), Input::Nv12), ["-c:v", "h264_amf"], "no arguments given: the defaults");
    let cache = ProbeCache::new();
    let fake = Fake::new(&[("nvenc_h264", Outcome::Works)]);
    cache.outcome(&fake, nvenc, Input::Nv12, &Cancel::new());
    assert_eq!(cache.get(&fake, nvenc, Input::Nv12), Some(Outcome::Works));
    struct Other<'a>(&'a Fake);
    impl Prober for Other<'_> {
        fn program(&self) -> &Path {
            self.0.program()
        }
        fn probe(&self, c: &'static Candidate, i: Input, cancel: &Cancel) -> Outcome {
            self.0.probe(c, i, cancel)
        }
        fn args(&self, _: &'static Candidate, _: Input) -> Vec<String> {
            vec!["-c:v".into(), "h264_nvenc".into(), "-b:v".into(), "6000k".into()]
        }
    }
    assert_eq!(cache.get(&Other(&fake), nvenc, Input::Nv12), None, "other arguments are tested again");
    let amf = candidate("amf_h264").unwrap();
    cache.mark_failed(fake.program(), amf, "lost the device");
    let fresh = cache.without_outcomes();
    assert_eq!(fresh.get(&fake, nvenc, Input::Nv12), None, "a second output tests again");
    assert_eq!(fresh.get(&fake, amf, Input::Nv12), Some(Outcome::Failed("lost the device".into())));
    assert_eq!(cache.get(&fake, nvenc, Input::Nv12), Some(Outcome::Works));
}

#[test]
fn auto_takes_the_first_candidate_that_works_and_skips_busy_and_missing_devices() {
    let fake = Fake::new(&[
        ("nvenc_h264", Outcome::Busy("incompatible client key".into())),
        ("mf_h264", Outcome::NotListed),
        ("amf_h264", Outcome::Works),
        ("qsv_h264", Outcome::Works),
    ]);
    let cache = ProbeCache::new();
    let choice = resolve(&Preference::Auto, target(Vendor::Nvidia, Os::Windows), &fake, &cache, &Cancel::new()).unwrap();
    assert_eq!(choice, Choice::Hardware(candidate("amf_h264").unwrap()));
    assert_eq!(fake.asked(), ["nvenc_h264", "mf_h264", "amf_h264"], "the tests stop at the first that works");
}

#[test]
fn auto_falls_back_to_software_and_reports_what_each_test_found() {
    let fake = Fake::new(&[("vaapi_h264", Outcome::Failed("Failed to initialise VAAPI connection".into()))]);
    let cache = ProbeCache::new();
    let choice = resolve(&Preference::Auto, target(Vendor::Amd, Os::Linux), &fake, &cache, &Cancel::new()).unwrap();
    let Choice::Software { tried } = choice else { panic!("software expected") };
    assert_eq!(tried.iter().map(|(c, _)| c.id).collect::<Vec<_>>(), ["vaapi_h264", "nvenc_h264", "qsv_h264"]);
    assert!(matches!(tried[0].1, Outcome::Failed(_)));
    let none = resolve(&Preference::Software, target(Vendor::Nvidia, Os::Windows), &fake, &cache, &Cancel::new()).unwrap();
    assert_eq!(none, Choice::Software { tried: Vec::new() });
}

#[test]
fn without_ffmpeg_auto_stops_testing_and_falls_back() {
    let fake = Fake::new(&[("nvenc_h264", Outcome::FfmpegMissing("ffmpeg.exe was not found".into()))]);
    let cache = ProbeCache::new();
    let Choice::Software { tried } = resolve(&Preference::Auto, target(Vendor::Nvidia, Os::Windows), &fake, &cache, &Cancel::new()).unwrap() else {
        panic!("software expected")
    };
    assert_eq!(tried.len(), 1);
    assert_eq!(fake.asked(), ["nvenc_h264"]);
    assert_eq!(cache.get(&fake, candidate("nvenc_h264").unwrap(), Input::Nv12), None, "a missing ffmpeg is tried again");
}

#[test]
fn outcomes_are_tested_once_per_session_and_a_failure_while_running_is_remembered() {
    let fake = Fake::new(&[("nvenc_h264", Outcome::Works), ("mf_h264", Outcome::Works)]);
    let cache = ProbeCache::new();
    let t = target(Vendor::Nvidia, Os::Windows);
    for _ in 0..3 {
        assert_eq!(resolve(&Preference::Auto, t, &fake, &cache, &Cancel::new()).unwrap(), Choice::Hardware(candidate("nvenc_h264").unwrap()));
    }
    assert_eq!(fake.asked(), ["nvenc_h264"]);
    cache.mark_failed(fake.program(), candidate("nvenc_h264").unwrap(), "the encoder stopped");
    assert_eq!(resolve(&Preference::Auto, t, &fake, &cache, &Cancel::new()).unwrap(), Choice::Hardware(candidate("mf_h264").unwrap()));
    let named = resolve(&Preference::Named("nvenc_h264".into()), t, &fake, &cache, &Cancel::new()).unwrap_err();
    assert!(named.to_string().contains("the encoder stopped"), "{named}");
    cache.clear();
    assert_eq!(resolve(&Preference::Auto, t, &fake, &cache, &Cancel::new()).unwrap(), Choice::Hardware(candidate("nvenc_h264").unwrap()));
}

#[test]
fn a_canceled_test_ends_the_choice_and_is_not_kept() {
    let mut fake = Fake::new(&[("nvenc_h264", Outcome::Works)]);
    fake.cancel_on_first = true;
    let cache = ProbeCache::new();
    let cancel = Cancel::new();
    let t = target(Vendor::Nvidia, Os::Windows);
    assert_eq!(resolve(&Preference::Auto, t, &fake, &cache, &cancel), Err(Error::Canceled));
    assert_eq!(fake.asked(), ["nvenc_h264"], "no test starts after the cancel");
    assert!(cancel.is_canceled());
    fake.cancel_on_first = false;
    assert_eq!(resolve(&Preference::Auto, t, &fake, &cache, &Cancel::new()).unwrap(), Choice::Hardware(candidate("nvenc_h264").unwrap()));
    assert_eq!(fake.asked(), ["nvenc_h264", "nvenc_h264"], "the canceled test runs again");
}

#[test]
fn a_named_encoder_that_fails_its_test_is_an_error() {
    let fake = Fake::new(&[("qsv_h264", Outcome::NoDevice("Error creating a MFX session".into()))]);
    let cache = ProbeCache::new();
    let t = target(Vendor::Nvidia, Os::Windows);
    let e = resolve(&Preference::Named("qsv_h264".into()), t, &fake, &cache, &Cancel::new()).unwrap_err();
    assert!(matches!(&e, Error::Failed { candidate, outcome: Outcome::NoDevice(_) } if candidate.id == "qsv_h264"), "{e:?}");
    assert!(e.to_string().starts_with("Intel Quick Sync H.264 did not pass its test encode"), "{e}");
    assert_eq!(resolve(&Preference::Named("nope".into()), t, &fake, &cache, &Cancel::new()), Err(Error::Unknown("nope".into())));
    let vt = resolve(&Preference::Named("vt_h264".into()), t, &fake, &cache, &Cancel::new()).unwrap_err();
    assert!(vt.to_string().contains("not on this system"), "{vt}");
    assert!(!fake.asked().contains(&"vt_h264"));
}

#[test]
fn preferences_round_trip_through_their_settings_value() {
    for p in [Preference::Auto, Preference::Software, Preference::Named("amf_hevc".into())] {
        assert_eq!(Preference::parse(p.id()), p);
    }
    assert_eq!(Preference::parse(" "), Preference::Auto);
}

#[test]
fn ffmpeg_errors_are_sorted_into_busy_no_device_and_failed() {
    let busy = "[h264_nvenc @ 0x1] OpenEncodeSessionEx failed: incompatible client key (21): (no details)\n";
    assert!(matches!(classify(busy), Outcome::Busy(l) if l.starts_with("[h264_nvenc")));
    assert!(matches!(classify("[h264_nvenc @ 0x1] Cannot load nvcuda.dll\n"), Outcome::NoDevice(_)));
    assert!(matches!(classify("[AVHWDeviceContext @ 0x2] Failed to initialise VAAPI connection: -1\n"), Outcome::NoDevice(_)));
    assert!(matches!(classify("[h264_amf @ 0x3] DLL amfrt64.dll failed to open\n"), Outcome::NoDevice(_)));
    let no_amd = "[AMF @ 0x4] AMFQueryVersion failed with error 1\n[h264_amf @ 0x5] Failed to create  hardware device context (AMF) : Unknown error occurred\n";
    assert!(matches!(classify(no_amd), Outcome::NoDevice(l) if l.starts_with("[AMF @")));
    assert_eq!(classify("Unknown encoder 'h264_foo'\n"), Outcome::Failed("Unknown encoder 'h264_foo'".into()));
    assert_eq!(classify(""), Outcome::Failed(String::new()));
}

#[test]
fn unsupported_input_and_unlisted_codecs_are_found_without_running_ffmpeg() {
    let p = FfmpegProber::new("no-such-dir/ffmpeg", vec!["h264_nvenc".into()]);
    let cancel = Cancel::new();
    assert_eq!(p.probe(candidate("nvenc_h264").unwrap(), Input::P010, &cancel), Outcome::Unsupported);
    assert_eq!(p.probe(candidate("amf_h264").unwrap(), Input::Nv12, &cancel), Outcome::NotListed);
    assert!(matches!(p.probe(candidate("nvenc_h264").unwrap(), Input::Nv12, &cancel), Outcome::FfmpegMissing(_)));
    let unknown_list = FfmpegProber::new("no-such-dir/ffmpeg", Vec::new());
    assert!(
        matches!(unknown_list.probe(candidate("amf_h264").unwrap(), Input::Nv12, &cancel), Outcome::FfmpegMissing(_)),
        "an unread list tries every codec"
    );
    cancel.cancel();
    assert_eq!(p.probe(candidate("nvenc_h264").unwrap(), Input::Nv12, &cancel), Outcome::Canceled);
}

/// The ffmpeg program: `CRAFT_HWENC_FFMPEG`, else `ffmpeg` on the `PATH`.
fn installed_ffmpeg() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CRAFT_HWENC_FFMPEG").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

#[test]
fn a_real_software_encode_passes_and_an_unknown_codec_fails() {
    let Some(ffmpeg) = installed_ffmpeg() else {
        eprintln!("skipped: no ffmpeg on the PATH (set CRAFT_HWENC_FFMPEG to run it)");
        return;
    };
    let cancel = Cancel::new();
    let timeout = Duration::from_secs(60);
    // mpeg4 is ffmpeg's own software encoder, built into every ffmpeg.
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(probe_codec(&ffmpeg, &args(&["-c:v", "mpeg4", "-q:v", "5"]), Input::Nv12, &cancel, timeout), Outcome::Works);
    let unknown = probe_codec(&ffmpeg, &args(&["-c:v", "h264_no_such_encoder"]), Input::Nv12, &cancel, timeout);
    assert!(matches!(&unknown, Outcome::Failed(why) if !why.is_empty()), "{unknown:?}");
    let bad_option = probe_codec(&ffmpeg, &args(&["-c:v", "mpeg4", "-no_such_option", "1"]), Input::Nv12, &cancel, timeout);
    assert!(matches!(&bad_option, Outcome::Failed(_)), "an option the encoder rejects fails the test: {bad_option:?}");
    cancel.cancel();
    assert_eq!(probe_codec(&ffmpeg, &args(&["-c:v", "mpeg4"]), Input::Nv12, &cancel, timeout), Outcome::Canceled);
    let version = ffmpeg_version(&ffmpeg, &Cancel::new(), timeout);
    assert!(version.starts_with("ffmpeg version") && version.contains(" bytes; modified "), "{version}");
}

/// A clock that moves only when a wait pauses on it or a test moves it.
struct TestClock {
    start: Instant,
    passed: Mutex<Duration>,
}

impl TestClock {
    fn new() -> Self {
        TestClock { start: Instant::now(), passed: Mutex::new(Duration::ZERO) }
    }
    fn passed(&self) -> Duration {
        *self.passed.lock().unwrap()
    }
    fn advance(&self, d: Duration) {
        *self.passed.lock().unwrap() += d;
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.start + self.passed()
    }
    fn pause(&self, d: Duration) {
        self.advance(d);
    }
}

/// A started program that ends after `ends_after` looks (never when `None`), sets `cancel` at
/// look `cancel_at`, or cannot be read.
#[derive(Default)]
struct Program {
    looks: usize,
    ends_after: Option<usize>,
    cancel_at: Option<(usize, Cancel)>,
    unreadable: bool,
    stops: usize,
}

impl crate::probe::Running for Program {
    type Status = &'static str;
    fn try_wait(&mut self) -> std::io::Result<Option<&'static str>> {
        self.looks += 1;
        if let Some((at, cancel)) = &self.cancel_at
            && *at == self.looks
        {
            cancel.cancel();
        }
        if self.unreadable {
            return Err(std::io::Error::other("no such process"));
        }
        Ok(self.ends_after.filter(|n| self.looks >= *n).map(|_| "ended"))
    }
    fn stop(&mut self) {
        self.stops += 1;
    }
}

#[test]
fn a_test_encode_that_never_ends_is_stopped_at_its_timeout_on_the_given_clock() {
    use crate::probe::{Ended, wait_for};
    let clock = TestClock::new();
    let mut hung = Program::default();
    assert_eq!(wait_for(&mut hung, &Cancel::new(), Duration::from_secs(5), &clock), Ended::TimedOut);
    assert_eq!(clock.passed(), Duration::from_secs(5), "the wait stops at the timeout, not past it");
    assert_eq!((hung.looks, hung.stops), (1001, 1), "one look every 5 ms, then one stop");
    let clock = TestClock::new();
    let mut ends = Program { ends_after: Some(3), ..Program::default() };
    assert_eq!(wait_for(&mut ends, &Cancel::new(), Duration::from_secs(5), &clock), Ended::Exited("ended"));
    assert_eq!((clock.passed(), ends.stops), (Duration::from_millis(10), 0));
    let cancel = Cancel::new();
    let mut canceled = Program { cancel_at: Some((2, cancel.clone())), ..Program::default() };
    assert_eq!(wait_for(&mut canceled, &cancel, Duration::from_secs(5), &TestClock::new()), Ended::Canceled);
    assert_eq!((canceled.looks, canceled.stops), (2, 1));
    let mut unreadable = Program { unreadable: true, ..Program::default() };
    assert!(matches!(wait_for(&mut unreadable, &Cancel::new(), Duration::from_secs(5), &TestClock::new()), Ended::Failed(_)));
    assert_eq!(unreadable.stops, 1);
    let mut no_time = Program::default();
    assert_eq!(wait_for(&mut no_time, &Cancel::new(), Duration::ZERO, &TestClock::new()), Ended::TimedOut);
    assert_eq!(no_time.looks, 1);
    assert_eq!(Outcome::TimedOut(Duration::from_millis(2500)).to_string(), "the test encode did not finish in 2.500 s");
}

/// A prober whose tests take a set time on a [`TestClock`]: a test given less time than it takes
/// times out after the time it was given.
struct Timed<'a> {
    clock: &'a TestClock,
    answers: HashMap<&'static str, (Outcome, Duration)>,
    asked: Mutex<Vec<&'static str>>,
}

impl Prober for Timed<'_> {
    fn program(&self) -> &Path {
        Path::new("timed-ffmpeg")
    }
    fn probe(&self, c: &'static Candidate, input: Input, cancel: &Cancel) -> Outcome {
        self.probe_within(c, input, cancel, Duration::from_secs(5))
    }
    fn probe_within(&self, c: &'static Candidate, _: Input, _: &Cancel, limit: Duration) -> Outcome {
        self.asked.lock().unwrap().push(c.id);
        let (answer, takes) = self.answers.get(c.id).cloned().unwrap_or((Outcome::NoDevice("timed".into()), Duration::from_secs(1)));
        let limit = limit.min(Duration::from_secs(5));
        if takes > limit {
            self.clock.advance(limit);
            return Outcome::TimedOut(limit);
        }
        self.clock.advance(takes);
        answer
    }
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(5))
    }
}

#[test]
fn the_tests_of_one_choice_stop_when_their_budget_is_used() {
    let clock = TestClock::new();
    let secs = Duration::from_secs;
    let prober = Timed {
        clock: &clock,
        answers: [
            ("nvenc_h264", (Outcome::NoDevice("no NVIDIA device".into()), secs(4))),
            ("mf_h264", (Outcome::Works, secs(60))),
            ("amf_h264", (Outcome::NoDevice("no AMD device".into()), secs(1))),
            ("qsv_h264", (Outcome::Works, secs(1))),
        ]
        .into_iter()
        .collect(),
        asked: Mutex::new(Vec::new()),
    };
    let cache = ProbeCache::new();
    let t = target(Vendor::Nvidia, Os::Windows);
    let budget = Budget { total: secs(8), clock: &clock };
    let Choice::Software { tried } = resolve_within(&Preference::Auto, t, &prober, &cache, &Cancel::new(), budget).unwrap() else {
        panic!("software expected")
    };
    assert_eq!(clock.passed(), secs(8), "the tests stop when the budget is used");
    let found: Vec<_> = tried.iter().map(|(c, o)| (c.id, o.clone())).collect();
    let not_tested = Outcome::NotTested("the tests had used their 8 s".into());
    assert_eq!(
        found,
        [
            ("nvenc_h264", Outcome::NoDevice("no NVIDIA device".into())),
            ("mf_h264", Outcome::TimedOut(secs(4))),
            ("amf_h264", not_tested.clone()),
            ("qsv_h264", not_tested),
        ]
    );
    let mf = candidate("mf_h264").unwrap();
    assert_eq!(cache.get(&prober, mf, Input::Nv12), None, "a test the budget cut short is tested again");
    assert_eq!(cache.get(&prober, candidate("qsv_h264").unwrap(), Input::Nv12), None, "a candidate not tested is tested later");
    // The next start: NVENC is kept, Media Foundation hangs for its whole timeout and is kept.
    let choice = resolve_within(&Preference::Auto, t, &prober, &cache, &Cancel::new(), budget).unwrap();
    assert_eq!(choice, Choice::Hardware(candidate("qsv_h264").unwrap()));
    assert_eq!(cache.get(&prober, mf, Input::Nv12), Some(Outcome::TimedOut(secs(5))));
    assert_eq!(clock.passed(), secs(8 + 5 + 1 + 1));
    let asked = prober.asked.lock().unwrap().clone();
    assert_eq!(asked, ["nvenc_h264", "mf_h264", "mf_h264", "amf_h264", "qsv_h264"]);
    // A third start reads every outcome from the cache and takes no time.
    assert_eq!(resolve_within(&Preference::Auto, t, &prober, &cache, &Cancel::new(), budget).unwrap(), choice);
    assert_eq!((clock.passed(), prober.asked.lock().unwrap().len()), (secs(15), 5));
    let named = resolve_within(
        &Preference::Named("mf_h264".into()),
        t,
        &prober,
        &ProbeCache::new(),
        &Cancel::new(),
        Budget { total: secs(2), clock: &clock },
    );
    assert_eq!(named, Err(Error::Failed { candidate: mf, outcome: Outcome::TimedOut(secs(2)) }), "a named encoder's test has the budget too");
}

/// A file in a folder of its own under the system's temporary folder.
fn temp_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("craft-hwenc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("hardware-encoder-tests.txt")
}

fn fingerprint() -> Fingerprint {
    Fingerprint {
        program: PathBuf::from("fake-ffmpeg"),
        ffmpeg_version: "ffmpeg version 8.0; 1000 bytes; modified 1.000000000".into(),
        driver: "10de:2684 GPU 591.44".into(),
        also: vec![PathBuf::from("<in process>")],
    }
}

#[test]
fn saved_outcomes_are_read_back_for_the_same_ffmpeg_and_driver() {
    let path = temp_file("hit");
    assert_eq!(ProbeCache::new().load(&path, &fingerprint()), Loaded::Missing);
    let fake = Fake::new(&[
        ("nvenc_h264", Outcome::NoDevice("line one\tcolumn\nline two \\ end".into())),
        ("mf_h264", Outcome::Works),
        ("amf_h264", Outcome::Busy("no free session".into())),
        ("qsv_h264", Outcome::Works),
    ]);
    let cache = ProbeCache::new();
    let t = target(Vendor::Nvidia, Os::Windows);
    assert_eq!(resolve(&Preference::Auto, t, &fake, &cache, &Cancel::new()).unwrap(), Choice::Hardware(candidate("mf_h264").unwrap()));
    for id in ["amf_h264", "qsv_h264"] {
        cache.outcome(&fake, candidate(id).unwrap(), Input::Nv12, &Cancel::new());
    }
    // A failure while encoding leaves the candidate out of the file, so the next session tests it.
    cache.mark_failed(fake.program(), candidate("qsv_h264").unwrap(), "the device was lost");
    assert_eq!(cache.save(&path, &fingerprint()).unwrap(), 2, "NVENC and Media Foundation; a busy device is not saved");
    let next = ProbeCache::new();
    assert_eq!(next.load(&path, &fingerprint()), Loaded::Read(2));
    let quiet = Fake::new(&[("nvenc_h264", Outcome::Works)]);
    assert_eq!(resolve(&Preference::Auto, t, &quiet, &next, &Cancel::new()).unwrap(), Choice::Hardware(candidate("mf_h264").unwrap()));
    assert!(quiet.asked().is_empty(), "every outcome came from the file: {:?}", quiet.asked());
    assert_eq!(next.get(&quiet, candidate("nvenc_h264").unwrap(), Input::Nv12), Some(Outcome::NoDevice("line one\tcolumn\nline two \\ end".into())));
    assert_eq!(next.get(&quiet, candidate("qsv_h264").unwrap(), Input::Nv12), None);
    // Encode arguments are part of what is kept.
    struct WithArgs(Fake);
    impl Prober for WithArgs {
        fn program(&self) -> &Path {
            self.0.program()
        }
        fn probe(&self, c: &'static Candidate, i: Input, cancel: &Cancel) -> Outcome {
            self.0.probe(c, i, cancel)
        }
        fn args(&self, _: &'static Candidate, _: Input) -> Vec<String> {
            vec!["-c:v".into(), "h264_nvenc".into(), "-metadata".into(), "title=a\tb".into()]
        }
    }
    let with_args = WithArgs(Fake::new(&[("nvenc_h264", Outcome::Works)]));
    let nvenc = candidate("nvenc_h264").unwrap();
    next.outcome(&with_args, nvenc, Input::Nv12, &Cancel::new());
    next.save(&path, &fingerprint()).unwrap();
    let again = ProbeCache::new();
    assert_eq!(again.load(&path, &fingerprint()), Loaded::Read(3));
    assert_eq!(again.get(&with_args, nvenc, Input::Nv12), Some(Outcome::Works));
    // An outcome this session already has is kept over the file's.
    let mine = ProbeCache::new();
    mine.outcome(&Fake::new(&[("mf_h264", Outcome::Failed("mine".into()))]), candidate("mf_h264").unwrap(), Input::Nv12, &Cancel::new());
    mine.load(&path, &fingerprint());
    assert_eq!(mine.get(&quiet, candidate("mf_h264").unwrap(), Input::Nv12), Some(Outcome::Failed("mine".into())));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_file_saved_for_another_ffmpeg_or_driver_is_not_read() {
    let path = temp_file("stale");
    let cache = ProbeCache::new();
    let fake = Fake::new(&[("nvenc_h264", Outcome::Works)]);
    let nvenc = candidate("nvenc_h264").unwrap();
    cache.outcome(&fake, nvenc, Input::Nv12, &Cancel::new());
    cache.save(&path, &fingerprint()).unwrap();
    let changes = [
        Fingerprint { ffmpeg_version: "ffmpeg version 8.1; 1000 bytes; modified 1.000000000".into(), ..fingerprint() },
        Fingerprint { ffmpeg_version: "ffmpeg version 8.0; 1001 bytes; modified 2.000000000".into(), ..fingerprint() },
        Fingerprint { driver: "10de:2684 GPU 595.02".into(), ..fingerprint() },
        Fingerprint { program: PathBuf::from("other-ffmpeg"), ..fingerprint() },
        Fingerprint { also: Vec::new(), ..fingerprint() },
    ];
    for f in &changes {
        let fresh = ProbeCache::new();
        assert_eq!(fresh.load(&path, f), Loaded::Stale, "{f:?}");
        assert_eq!(fresh.get(&fake, nvenc, Input::Nv12), None);
    }
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replacen(&format!("outcomes {CACHE_VERSION}\n"), &format!("outcomes {}\n", CACHE_VERSION + 1), 1)).unwrap();
    assert_eq!(ProbeCache::new().load(&path, &fingerprint()), Loaded::Stale, "another file version");
    // Saving for the new fingerprint replaces the file.
    cache.save(&path, &changes[2]).unwrap();
    assert_eq!(ProbeCache::new().load(&path, &changes[2]), Loaded::Read(1));
    assert_eq!(ProbeCache::new().load(&path, &fingerprint()), Loaded::Stale);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_damaged_or_oversized_file_is_not_read() {
    let path = temp_file("corrupt");
    let head = format!(
        "craft-hwenc test outcomes {CACHE_VERSION}\nprogram\tfake-ffmpeg\nffmpeg\tffmpeg version 8.0; 1000 bytes; modified 1.000000000\ndriver\t10de:2684 GPU 591.44\nalso\t<in process>\n"
    );
    let good = "outcome\tfake-ffmpeg\tnvenc_h264\tnv12\tworks\t\t-c:v\th264_nvenc\n";
    let damaged: Vec<(String, Vec<u8>)> = vec![
        ("not a cache file".into(), b"hello\n".to_vec()),
        ("an empty file".into(), Vec::new()),
        ("no driver line".into(), format!("craft-hwenc test outcomes {CACHE_VERSION}\nprogram\tfake-ffmpeg\nffmpeg\tx\n").into_bytes()),
        ("a version that is not a number".into(), b"craft-hwenc test outcomes one\n".to_vec()),
        ("an unknown escape".into(), format!("{head}outcome\tfake-ffmpeg\tnvenc_h264\tnv12\tfailed\tbad \\q\n").into_bytes()),
        ("an unknown encoder".into(), format!("{head}{good}outcome\tfake-ffmpeg\tnvenc_h999\tnv12\tworks\t\n").into_bytes()),
        ("an unknown result".into(), format!("{head}outcome\tfake-ffmpeg\tnvenc_h264\tnv12\tmaybe\t\n").into_bytes()),
        ("an unknown input".into(), format!("{head}outcome\tfake-ffmpeg\tnvenc_h264\tyuv444\tworks\t\n").into_bytes()),
        ("a cut-off outcome".into(), format!("{head}outcome\tfake-ffmpeg\tnvenc_h264\n").into_bytes()),
        ("a program the file does not name".into(), format!("{head}outcome\telsewhere\tnvenc_h264\tnv12\tworks\t\n").into_bytes()),
        ("an unknown line".into(), format!("{head}{good}rm -rf /\n").into_bytes()),
        ("not UTF-8".into(), [head.as_bytes(), b"outcome\tfake-ffmpeg\tnvenc_h264\tnv12\tfailed\t\xff\xfe\n"].concat()),
        ("too many outcomes".into(), format!("{head}{}", good.repeat(4097)).into_bytes()),
    ];
    let fake = Fake::new(&[]);
    let nvenc = candidate("nvenc_h264").unwrap();
    for (what, bytes) in damaged {
        std::fs::write(&path, &bytes).unwrap();
        let cache = ProbeCache::new();
        assert!(matches!(cache.load(&path, &fingerprint()), Loaded::Corrupt(_)), "{what}");
        assert_eq!(cache.get(&fake, nvenc, Input::Nv12), None, "{what}: nothing is read");
    }
    std::fs::write(&path, format!("{head}{good}")).unwrap();
    assert_eq!(ProbeCache::new().load(&path, &fingerprint()), Loaded::Read(1), "the same file undamaged");
    // A file past the limit is refused before it is read.
    let big = std::fs::File::create(&path).unwrap();
    big.set_len(MAX_CACHE_BYTES + 1).unwrap();
    assert_eq!(ProbeCache::new().load(&path, &fingerprint()), Loaded::Corrupt("it is larger than 1024 KiB".into()));
    // A folder in the file's place is an error, not a crash.
    let _ = std::fs::remove_file(&path);
    std::fs::create_dir_all(&path).unwrap();
    assert!(matches!(ProbeCache::new().load(&path, &fingerprint()), Loaded::Corrupt(_)));
    assert!(ProbeCache::new().save(&path, &fingerprint()).is_err());
    assert_eq!(ffmpeg_version(Path::new("no-such-dir/ffmpeg"), &Cancel::new(), Duration::from_secs(1)), "unknown");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_saved_file_stays_within_what_is_read_back() {
    let path = temp_file("big");
    let cache = ProbeCache::new();
    {
        let mut outcomes = cache.outcomes.lock().unwrap();
        for i in 0..400 {
            let args = vec![format!("-preset {i:04} {}", "x".repeat(4000))];
            outcomes.insert((PathBuf::from("fake-ffmpeg"), "nvenc_h264", Input::Nv12, args), Outcome::Works);
        }
    }
    let saved = cache.save(&path, &fingerprint()).unwrap();
    assert!((1..400).contains(&saved), "{saved} of 400 outcomes fit");
    assert!(std::fs::metadata(&path).unwrap().len() <= MAX_CACHE_BYTES);
    assert_eq!(ProbeCache::new().load(&path, &fingerprint()), Loaded::Read(saved));
    let huge = Fingerprint { driver: "d".repeat(2 << 20), ..fingerprint() };
    assert!(cache.save(&path, &huge).is_err(), "a fingerprint past the limit is not written");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_saved_va_api_test_names_the_render_node_it_passed_on() {
    use crate::disk::saved_vaapi_node;
    let devices = ["/dev/dri/renderD128".to_string(), "/dev/dri/renderD129".to_string()];
    let vaapi = candidate("vaapi_hevc").unwrap();
    let key = |id: &'static str, node: &str| {
        (PathBuf::from("ffmpeg"), id, Input::Nv12, vec!["-vaapi_device".to_string(), node.to_string(), "-c:v".into(), "hevc_vaapi".into()])
    };
    let later = key("vaapi_hevc", "/dev/dri/renderD129");
    assert_eq!(saved_vaapi_node(&later, &devices), Some((vaapi, "/dev/dri/renderD129")));
    assert_eq!(saved_vaapi_node(&key("vaapi_hevc", "/dev/dri/renderD140"), &devices), None, "a node this system does not have");
    assert_eq!(saved_vaapi_node(&key("nvenc_hevc", "/dev/dri/renderD129"), &devices), None, "not a VA-API encoder");
    let mut cut = later.clone();
    cut.3.truncate(1);
    assert_eq!(saved_vaapi_node(&cut, &devices), None, "no node after -vaapi_device");
}

#[test]
fn a_va_api_test_is_saved_with_the_node_it_passed_on() {
    use crate::candidate::use_vaapi_device;
    let path = temp_file("vaapi");
    let vaapi = candidate("vaapi_h264").unwrap();
    let fake = Fake::new(&[("vaapi_h264", Outcome::Works)]);
    let cache = ProbeCache::new();
    // The test passed on the node this system lists last, whatever node its arguments named.
    let node = vaapi_devices().pop().unwrap();
    assert_eq!(cache.outcome(&fake, vaapi, Input::Nv12, &Cancel::new()), Outcome::Works);
    use_vaapi_device(vaapi, &node);
    assert_eq!(cache.save(&path, &fingerprint()).unwrap(), 1);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(&format!("-vaapi_device\t{node}\t")), "{text}");
    // Kept apart from the node, the outcome is found whatever node the arguments name.
    let next = ProbeCache::new();
    assert_eq!(next.load(&path, &fingerprint()), Loaded::Read(1));
    assert_eq!(next.get(&fake, vaapi, Input::Nv12), Some(Outcome::Works));
    // A pass on a node this system no longer has is tested again.
    std::fs::write(&path, text.replace(&node, "/dev/dri/renderD999")).unwrap();
    let gone = ProbeCache::new();
    assert_eq!(gone.load(&path, &fingerprint()), Loaded::Read(0));
    assert_eq!(gone.get(&fake, vaapi, Input::Nv12), None);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
