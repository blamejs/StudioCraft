//! The hardware encoders ffmpeg can run, and the order the automatic choice tries them in.

/// The compressed format a candidate writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

/// The hardware encoding API behind a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Api {
    /// NVIDIA's encoder.
    Nvenc,
    /// AMD's Advanced Media Framework.
    Amf,
    /// Intel Quick Sync Video.
    Qsv,
    /// Windows Media Foundation, which runs the encoder of whichever GPU the system picks.
    MediaFoundation,
    /// Apple's VideoToolbox.
    VideoToolbox,
    /// The Video Acceleration API on Linux.
    Vaapi,
}

/// An operating system, for the APIs it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl Os {
    /// The system this program was built for.
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

/// The maker of the graphics adapter, from its PCI vendor id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
    /// Another id, or none (0).
    Other(u32),
}

impl Vendor {
    pub fn from_pci_id(id: u32) -> Vendor {
        match id {
            0x10de => Vendor::Nvidia,
            0x1002 | 0x1022 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            0x106b => Vendor::Apple,
            other => Vendor::Other(other),
        }
    }

    /// The vendor's own encoding API on `os`.
    fn api(self, os: Os) -> Option<Api> {
        match (self, os) {
            (Vendor::Nvidia, Os::Windows | Os::Linux) => Some(Api::Nvenc),
            (Vendor::Amd, Os::Windows) => Some(Api::Amf),
            (Vendor::Amd, Os::Linux) => Some(Api::Vaapi),
            (Vendor::Intel, Os::Windows | Os::Linux) => Some(Api::Qsv),
            (Vendor::Apple, Os::MacOs) => Some(Api::VideoToolbox),
            _ => None,
        }
    }
}

/// The frames a candidate is given: what the GPU conversion writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Input {
    /// 8 bits: the luma plane, then interleaved Cb and Cr.
    Nv12,
    /// 10 bits in the high bits of little-endian 16-bit words, in NV12's planes.
    P010,
}

impl Input {
    /// ffmpeg's name for the pixel format.
    pub fn ffmpeg_pix_fmt(self) -> &'static str {
        match self {
            Input::Nv12 => "nv12",
            Input::P010 => "p010le",
        }
    }

    /// Bytes of one tightly packed frame; `None` for an odd side or a size past `usize`.
    pub fn frame_bytes(self, width: u32, height: u32) -> Option<usize> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return None;
        }
        let samples = usize::try_from(u64::from(width).checked_mul(u64::from(height))?.checked_mul(3)? / 2).ok()?;
        samples.checked_mul(if self == Input::P010 { 2 } else { 1 })
    }
}

/// One hardware encoder ffmpeg can run.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// Stable id, for settings files.
    pub id: &'static str,
    /// The name shown to the user.
    pub name: &'static str,
    /// ffmpeg's codec name (`-c:v`).
    pub ffmpeg_codec: &'static str,
    pub codec: Codec,
    pub api: Api,
}

impl Candidate {
    /// Whether the API exists on `os`.
    pub fn runs_on(&self, os: Os) -> bool {
        match self.api {
            Api::Nvenc | Api::Qsv => matches!(os, Os::Windows | Os::Linux),
            Api::Amf => os == Os::Windows,
            Api::MediaFoundation => os == Os::Windows,
            Api::VideoToolbox => os == Os::MacOs,
            Api::Vaapi => os == Os::Linux,
        }
    }

    /// Whether the candidate takes `input`. 10-bit input is for HEVC and AV1 only, and Media
    /// Foundation takes 8-bit input only.
    pub fn accepts(&self, input: Input) -> bool {
        match input {
            Input::Nv12 => true,
            Input::P010 => self.codec != Codec::H264 && self.api != Api::MediaFoundation,
        }
    }

    /// Whether ffmpeg can run it losslessly: NVENC for H.264 and HEVC only.
    pub fn lossless(&self) -> bool {
        self.api == Api::Nvenc && self.codec != Codec::Av1
    }

    /// The arguments that go before the codec arguments of an encode with `input`: VA-API opens
    /// [`vaapi_device`] and uploads each frame to it.
    pub fn prelude(&self, input: Input) -> Vec<String> {
        match self.api {
            Api::Vaapi => vec!["-vaapi_device".into(), vaapi_device(self), "-vf".into(), format!("format={},hwupload", input.ffmpeg_pix_fmt())],
            _ => Vec::new(),
        }
    }
}

/// The render node each VA-API candidate's test encode last passed on, by candidate id.
static VAAPI_DEVICES: std::sync::RwLock<Option<std::collections::HashMap<&'static str, String>>> = std::sync::RwLock::new(None);

/// The render node `candidate` encodes on with VA-API: the one its test encode last passed on,
/// else the first of [`vaapi_devices`].
pub fn vaapi_device(candidate: &Candidate) -> String {
    let passed = VAAPI_DEVICES.read().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().and_then(|m| m.get(candidate.id).cloned());
    passed.or_else(|| vaapi_devices().into_iter().next()).unwrap_or_else(|| DEFAULT_RENDER_NODE.to_string())
}

/// Use `node` for `candidate`'s VA-API encodes that follow.
pub(crate) fn use_vaapi_device(candidate: &'static Candidate, node: &str) {
    VAAPI_DEVICES.write().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert_default().insert(candidate.id, node.to_string());
}

const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

/// The `/dev/dri/renderD*` nodes in the order VA-API tries them: in number order, NVIDIA's last
/// (its driver has no VA-API encoder); `/dev/dri/renderD128` when none is listed.
pub fn vaapi_devices() -> Vec<String> {
    let nodes = std::fs::read_dir("/dev/dri").into_iter().flatten().flatten().filter_map(|e| {
        let name = e.file_name().into_string().ok()?;
        let number: u32 = name.strip_prefix("renderD")?.parse().ok()?;
        let vendor = std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor"))
            .ok()
            .and_then(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok());
        Some((number, vendor))
    });
    order_render_nodes(nodes.collect())
}

/// [`vaapi_devices`]'s order for `(number, PCI vendor id)` pairs.
pub(crate) fn order_render_nodes(mut nodes: Vec<(u32, Option<u32>)>) -> Vec<String> {
    nodes.sort_unstable_by_key(|&(n, v)| (v == Some(0x10de), n));
    if nodes.is_empty() {
        return vec![DEFAULT_RENDER_NODE.to_string()];
    }
    nodes.into_iter().map(|(n, _)| format!("/dev/dri/renderD{n}")).collect()
}

/// `args` with the value after `-vaapi_device` set to `node`.
pub(crate) fn with_vaapi_device(args: &[String], node: &str) -> Vec<String> {
    let mut a = args.to_vec();
    if let Some(i) = a.iter().position(|s| s == "-vaapi_device")
        && let Some(v) = a.get_mut(i + 1)
    {
        *v = node.to_string();
    }
    a
}

macro_rules! c {
    ($id:literal, $name:literal, $ffmpeg:literal, $codec:ident, $api:ident) => {
        Candidate { id: $id, name: $name, ffmpeg_codec: $ffmpeg, codec: Codec::$codec, api: Api::$api }
    };
}

/// Every hardware encoder, in each API's order of H.264, HEVC, AV1.
pub static CANDIDATES: [Candidate; 16] = [
    c!("nvenc_h264", "NVIDIA NVENC H.264", "h264_nvenc", H264, Nvenc),
    c!("nvenc_hevc", "NVIDIA NVENC HEVC", "hevc_nvenc", Hevc, Nvenc),
    c!("nvenc_av1", "NVIDIA NVENC AV1", "av1_nvenc", Av1, Nvenc),
    c!("amf_h264", "AMD AMF H.264", "h264_amf", H264, Amf),
    c!("amf_hevc", "AMD AMF HEVC", "hevc_amf", Hevc, Amf),
    c!("amf_av1", "AMD AMF AV1", "av1_amf", Av1, Amf),
    c!("qsv_h264", "Intel Quick Sync H.264", "h264_qsv", H264, Qsv),
    c!("qsv_hevc", "Intel Quick Sync HEVC", "hevc_qsv", Hevc, Qsv),
    c!("qsv_av1", "Intel Quick Sync AV1", "av1_qsv", Av1, Qsv),
    c!("mf_h264", "Windows Media Foundation H.264", "h264_mf", H264, MediaFoundation),
    c!("mf_hevc", "Windows Media Foundation HEVC", "hevc_mf", Hevc, MediaFoundation),
    c!("vt_h264", "VideoToolbox H.264", "h264_videotoolbox", H264, VideoToolbox),
    c!("vt_hevc", "VideoToolbox HEVC", "hevc_videotoolbox", Hevc, VideoToolbox),
    c!("vaapi_h264", "VA-API H.264", "h264_vaapi", H264, Vaapi),
    c!("vaapi_hevc", "VA-API HEVC", "hevc_vaapi", Hevc, Vaapi),
    c!("vaapi_av1", "VA-API AV1", "av1_vaapi", Av1, Vaapi),
];

/// The candidate with this id.
pub fn candidate(id: &str) -> Option<&'static Candidate> {
    CANDIDATES.iter().find(|c| c.id == id)
}

/// The system's own API, which runs whatever GPU is installed.
fn system_api(os: Os) -> Option<Api> {
    match os {
        Os::Windows => Some(Api::MediaFoundation),
        Os::MacOs => Some(Api::VideoToolbox),
        Os::Linux => Some(Api::Vaapi),
        Os::Other => None,
    }
}

/// The candidates for `codec` on `os` in the order the automatic choice tries them: the API of
/// the adapter's `vendor`, then the system's own API, then the other vendors' APIs (a second GPU
/// can have one).
pub fn auto_order(vendor: Vendor, os: Os, codec: Codec) -> Vec<&'static Candidate> {
    let rank = |c: &Candidate| {
        if Some(c.api) == vendor.api(os) {
            0
        } else if Some(c.api) == system_api(os) {
            1
        } else {
            2
        }
    };
    let mut list: Vec<&'static Candidate> = CANDIDATES.iter().filter(|c| c.codec == codec && c.runs_on(os)).collect();
    // A stable sort keeps the table's order inside each rank.
    list.sort_by_key(|c| rank(c));
    list
}
