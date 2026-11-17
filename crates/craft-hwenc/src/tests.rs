use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

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
}
