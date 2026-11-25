//! Every call into AMD's Advanced Media Framework: `amfrt64.dll`, which AMD's graphics driver
//! installs in the system directory, loaded at run time once per process, and one hardware encoder
//! component on a Direct3D 11 device that AMF opens itself ([`session`]). The declarations are in
//! [`abi`]. The items here are safe and run on every system: the properties an encoder is set up
//! with, the reading of AMF's results, the frame copy into a surface's planes, and the order of
//! submitting frames and collecting packets. On other systems opening an encoder is an error that
//! says AMF runs on Windows only.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::time::{Duration, Instant};

pub(crate) mod abi;
#[cfg(test)]
mod abi_tests;
#[cfg(target_os = "windows")]
mod session;

pub(crate) use crate::mf_ffi::Output;
use crate::{Codec, Rate};
use abi::AMF_RESULT;

#[cfg(not(target_os = "windows"))]
pub(crate) use none::{Session, loaded};
#[cfg(target_os = "windows")]
pub(crate) use session::{Session, loaded};

/// What an encoder component is set up for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Params {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: (u32, u32),
    pub rate: Rate,
    /// Frames from one keyframe to the next.
    pub gop: u32,
}

/// The oldest AMF runtime this crate asks for: 1.4.0.
pub(crate) const MIN_RUNTIME: u64 = abi::make_version(1, 4, 0, 0);

/// The version this crate asks `AMFInit` for: the version of the headers it was written from, or
/// the runtime's own when that is older. A runtime older than [`MIN_RUNTIME`] is an error.
pub(crate) fn init_version(runtime: u64) -> Result<u64, String> {
    if runtime < MIN_RUNTIME {
        let v = |shift: u32| (runtime >> shift) & 0xffff;
        return Err(format!("the AMF runtime {}.{}.{} is older than 1.4", v(48), v(32), v(16)));
    }
    Ok(runtime.min(abi::AMF_FULL_VERSION))
}

/// The AMF component id of the hardware encoder of `codec`.
pub(crate) fn component_id(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "AMFVideoEncoderVCE_AVC",
        Codec::Hevc => "AMFVideoEncoderHW_HEVC",
        Codec::Av1 => "AMFVideoEncoderHW_AV1",
    }
}

/// The read-only property that holds the parameter sets (AV1: the sequence header) once the
/// encoder is initialized.
pub(crate) fn extra_data(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "ExtraData",
        Codec::Hevc => "HevcExtraData",
        Codec::Av1 => "Av1ExtraData",
    }
}

/// A property value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    Int(i64),
    Bool(bool),
    Size(u32, u32),
    Rate(u32, u32),
}

/// Whether the encoder must take a property. A runtime that does not know an optional one (an
/// older driver) leaves its default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    Required,
    Optional,
}

/// The H.273 code point of BT.709 primaries, transfer and matrix, which AMF's color enums use.
const BT709: i64 = 1;

/// The base quantizer index of an AV1 stream at quality level `q`: `5 × q`, at most 255.
pub(crate) fn av1_q_index(q: u32) -> i64 {
    i64::from(q.saturating_mul(5).min(255))
}

/// The bitrate in bit/s of `kbps` kbit/s.
fn bits(kbps: u32) -> i64 {
    i64::from(kbps) * 1000
}

/// The properties an encoder for `p` is set up with before `Init`, in the order they are set.
/// The usage comes first: it sets every other property to its default for that usage.
///
/// - Usage is transcoding, with no B-frames, H.264 High, HEVC Main and AV1 Main.
/// - Quality mode is constant QP at the level (AV1: base quantizer index `5 × level`). Bitrate
///   mode is constant bitrate with the peak at the target and a one-second buffer.
/// - The output color is BT.709 at limited range. Older drivers that do not know the color
///   properties leave their defaults.
pub(crate) fn properties(p: &Params) -> Vec<(&'static str, Value, Need)> {
    use Need::{Optional, Required};
    use Value::{Bool, Int};
    let size = Value::Size(p.width, p.height);
    let rate = Value::Rate(p.fps.0, p.fps.1);
    let gop = Int(i64::from(p.gop.max(1)));
    let mut v = match p.codec {
        Codec::H264 => vec![
            ("Usage", Int(0), Required),
            ("Profile", Int(100), Required),
            ("FrameSize", size, Required),
            ("FrameRate", rate, Required),
            ("BPicturesPattern", Int(0), Required),
            ("IDRPeriod", gop, Required),
            ("FullRangeColor", Bool(false), Optional),
            ("OutColorPrimaries", Int(BT709), Optional),
            ("OutColorTransferChar", Int(BT709), Optional),
            ("OutMatrixCoeff", Int(BT709), Optional),
        ],
        Codec::Hevc => vec![
            ("HevcUsage", Int(0), Required),
            ("HevcProfile", Int(1), Required),
            ("HevcTier", Int(0), Required),
            ("HevcFrameSize", size, Required),
            ("HevcFrameRate", rate, Required),
            ("HevcGOPSize", gop, Required),
            ("HevcGOPSPerIDR", Int(1), Required),
            ("HevcHeaderInsertionMode", Int(2), Optional),
            ("HevcNominalRange", Bool(false), Optional),
            ("HevcOutColorPrimaries", Int(BT709), Optional),
            ("HevcOutColorTransferChar", Int(BT709), Optional),
            ("HevcOutMatrixCoeff", Int(BT709), Optional),
        ],
        Codec::Av1 => vec![
            ("Av1Usage", Int(0), Required),
            ("Av1Profile", Int(1), Required),
            ("Av1FrameSize", size, Required),
            ("Av1FrameRate", rate, Required),
            ("Av1AlignmentMode", Int(3), Optional),
            ("Av1BPicturesPattern", Int(0), Optional),
            ("Av1GOPSize", gop, Required),
            ("Av1HeaderInsertionMode", Int(2), Optional),
            ("Av1OutputColorPrimaries", Int(BT709), Optional),
            ("Av1OutputColorTransferChar", Int(BT709), Optional),
            ("Av1OutMatrixCoeff", Int(BT709), Optional),
        ],
    };
    let (method, qi, qp, target, peak, vbv) = match p.codec {
        Codec::H264 => ("RateControlMethod", "QPI", "QPP", "TargetBitrate", "PeakBitrate", "VBVBufferSize"),
        Codec::Hevc => ("HevcRateControlMethod", "HevcQP_I", "HevcQP_P", "HevcTargetBitrate", "HevcPeakBitrate", "HevcVBVBufferSize"),
        Codec::Av1 => ("Av1RateControlMethod", "Av1QIndex_Intra", "Av1QIndex_Inter", "Av1TargetBitrate", "Av1PeakBitrate", "Av1VBVBufferSize"),
    };
    match p.rate {
        Rate::Quality(q) => {
            let level = if p.codec == Codec::Av1 { av1_q_index(q) } else { i64::from(q) };
            v.extend([(method, Int(0), Required), (qi, Int(level), Required), (qp, Int(level), Required)]);
        }
        Rate::Cbr(kbps) => {
            // CBR is 1 in the H.264 enum and 3 in the HEVC and AV1 enums.
            let cbr = if p.codec == Codec::H264 { 1 } else { 3 };
            v.extend([
                (method, Int(cbr), Required),
                (target, Int(bits(kbps)), Required),
                (peak, Int(bits(kbps)), Required),
                (vbv, Int(bits(kbps)), Required),
            ]);
        }
    }
    v
}

/// Whether an AV1 encoder that keeps its default alignment (frames in 64 × 16 blocks) takes a
/// `width` × `height` stream without padding it.
pub(crate) fn av1_default_alignment_fits(width: u32, height: u32) -> bool {
    width.is_multiple_of(64) && height.is_multiple_of(16)
}

/// The name of an AMF result, for messages.
pub(crate) fn result_name(r: AMF_RESULT) -> String {
    use abi::*;
    let name = match r {
        AMF_OK => "AMF_OK",
        AMF_FAIL => "AMF_FAIL",
        AMF_INVALID_ARG => "AMF_INVALID_ARG",
        AMF_OUT_OF_RANGE => "AMF_OUT_OF_RANGE",
        AMF_OUT_OF_MEMORY => "AMF_OUT_OF_MEMORY",
        AMF_NOT_SUPPORTED => "AMF_NOT_SUPPORTED",
        AMF_NOT_FOUND => "AMF_NOT_FOUND",
        AMF_NO_DEVICE => "AMF_NO_DEVICE",
        AMF_DIRECTX_FAILED => "AMF_DIRECTX_FAILED",
        AMF_EOF => "AMF_EOF",
        AMF_REPEAT => "AMF_REPEAT",
        AMF_INPUT_FULL => "AMF_INPUT_FULL",
        AMF_INVALID_RESOLUTION => "AMF_INVALID_RESOLUTION",
        AMF_CODEC_NOT_SUPPORTED => "AMF_CODEC_NOT_SUPPORTED",
        AMF_SURFACE_FORMAT_NOT_SUPPORTED => "AMF_SURFACE_FORMAT_NOT_SUPPORTED",
        AMF_ENCODER_NOT_PRESENT => "AMF_ENCODER_NOT_PRESENT",
        AMF_NEED_MORE_INPUT => "AMF_NEED_MORE_INPUT",
        _ => return format!("AMF result {r}"),
    };
    name.into()
}

/// Why no encoder opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenError {
    /// AMF, an AMD GPU, or its encoder for the codec, is missing.
    NoDevice(String),
    /// The encoder does not take the configuration.
    Unsupported(String),
    /// The driver failed.
    Failed(String),
}

/// The error of the call `what` that failed with `r` while an encoder opened.
pub(crate) fn refusal(what: &str, r: AMF_RESULT) -> OpenError {
    use abi::*;
    let why = format!("{what} failed ({})", result_name(r));
    match r {
        AMF_NO_DEVICE | AMF_ENCODER_NOT_PRESENT | AMF_DIRECTX_FAILED | AMF_NOT_FOUND => OpenError::NoDevice(why),
        AMF_NOT_SUPPORTED
        | AMF_CODEC_NOT_SUPPORTED
        | AMF_INVALID_RESOLUTION
        | AMF_SURFACE_FORMAT_NOT_SUPPORTED
        | AMF_OUT_OF_RANGE
        | AMF_INVALID_ARG => OpenError::Unsupported(why),
        _ => OpenError::Failed(why),
    }
}

/// The largest coded frame read from the encoder: two raw frames of the stream or 64 MiB,
/// whichever is larger. A larger one is an error.
pub(crate) fn max_packet(width: u32, height: u32) -> usize {
    let frame = (width as usize).saturating_mul(height as usize).saturating_mul(3) / 2;
    frame.saturating_mul(2).max(64 << 20)
}

/// The most a surface row may be longer than the frame's row: drivers pad rows to an alignment
/// far below this.
pub(crate) const MAX_PITCH_PADDING: usize = 4096;

/// The pitch and the number of bytes of a surface plane that `rows` frame rows of `width` bytes
/// fill, from the `pitch` and the number of rows `plane_rows` the surface reports for the plane.
/// A pitch below the width or more than [`MAX_PITCH_PADDING`] above it, or a plane with fewer
/// rows than the frame, is an error. Only the rows the frame fills are counted, so a plane
/// reported taller than the frame is never addressed past the frame's last row.
pub(crate) fn plane_extent(width: usize, rows: usize, pitch: i32, plane_rows: i32) -> Result<(usize, usize), String> {
    let (Ok(pitch), Ok(plane_rows)) = (usize::try_from(pitch), usize::try_from(plane_rows)) else {
        return Err("the surface reports a negative plane size".into());
    };
    if pitch < width || pitch - width > MAX_PITCH_PADDING {
        return Err(format!("the surface reports a row of {pitch} bytes for a frame row of {width}"));
    }
    if plane_rows < rows {
        return Err("the surface plane is smaller than the frame".into());
    }
    if rows == 0 {
        return Ok((pitch, 0));
    }
    let len = pitch.checked_mul(rows - 1).and_then(|b| b.checked_add(width));
    len.map(|len| (pitch, len)).ok_or_else(|| "the surface plane is too large to address".into())
}

/// Copy `rows` rows of `width` bytes from the tightly packed `src` into `dst`, whose rows start
/// every `pitch` bytes.
pub(crate) fn copy_plane(src: &[u8], width: usize, rows: usize, dst: &mut [u8], pitch: usize) -> Result<(), String> {
    if pitch < width {
        return Err(format!("a surface row of {pitch} bytes is shorter than the frame's {width}"));
    }
    let need = if rows == 0 { 0 } else { pitch.saturating_mul(rows - 1).saturating_add(width) };
    if src.len() < width.saturating_mul(rows) || dst.len() < need {
        return Err("the surface plane is smaller than the frame".into());
    }
    for (s, d) in src.chunks_exact(width.max(1)).take(rows).zip(dst.chunks_mut(pitch.max(1))) {
        d[..width].copy_from_slice(s);
    }
    Ok(())
}

/// What `SubmitInput` or `Drain` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Submitted {
    /// The encoder took the frame, or started draining.
    Taken,
    /// The encoder's input queue is full: an output must be collected before it takes more.
    Full,
}

/// What one `QueryOutput` returned.
pub(crate) enum Polled {
    Output(Output),
    /// No output is ready yet.
    Empty,
    /// The encoder has returned every frame after a drain.
    Ended,
}

/// An encoder component as the submit and collect order sees it.
pub(crate) trait Engine {
    fn submit(&mut self, frame: &[u8], time: i64, duration: i64) -> Result<Submitted, String>;
    fn poll(&mut self) -> Result<Polled, String>;
    fn drain(&mut self) -> Result<Submitted, String>;
}

/// How long the encoder may go without taking a frame, or without finishing its last frames
/// after a drain, before that is an error.
pub(crate) const WAIT_LIMIT: Duration = Duration::from_secs(10);

/// How long one pause of [`waiter`] lasts.
const PAUSE: Duration = Duration::from_millis(1);

/// The step between two polls of the encoder: it returns `false` once [`WAIT_LIMIT`] has passed
/// since the waiter was made, and otherwise sleeps [`PAUSE`] when its argument asks for a pause
/// and returns `true`.
pub(crate) fn waiter() -> impl FnMut(bool) -> bool {
    let start = Instant::now();
    move |pause| {
        if start.elapsed() >= WAIT_LIMIT {
            return false;
        }
        if pause {
            std::thread::sleep(PAUSE);
        }
        true
    }
}

/// The most outputs one [`collect_ready`] call takes. An encoder that still has an output ready
/// after that many is an error.
pub(crate) const MAX_READY: usize = 64;

/// Every output that is ready, appended to `out`, without waiting; whether there was one. More
/// than [`MAX_READY`] outputs in one call is an error; every output polled stays in `out`.
pub(crate) fn collect_ready(e: &mut impl Engine, out: &mut Vec<Output>) -> Result<bool, String> {
    let mut got = 0;
    loop {
        match e.poll()? {
            Polled::Output(o) => {
                out.push(o);
                got += 1;
                if got > MAX_READY {
                    return Err(format!("the encoder returned more than {MAX_READY} outputs in a row"));
                }
            }
            Polled::Empty => return Ok(got > 0),
            Polled::Ended => return Err("the encoder ended its stream before it was drained".into()),
        }
    }
}

/// Submit one frame, collecting the outputs that come out while the encoder's input queue is full.
/// The call pauses (through `wait`) only while the queue is full and no output is ready, and every
/// retry counts toward the wait limit. It returns once the encoder has taken the frame; an error
/// means the encoder did not take it.
pub(crate) fn submit(
    e: &mut impl Engine,
    frame: &[u8],
    time: i64,
    duration: i64,
    out: &mut Vec<Output>,
    wait: &mut impl FnMut(bool) -> bool,
) -> Result<(), String> {
    while e.submit(frame, time, duration)? == Submitted::Full {
        let got = collect_ready(e, out)?;
        if !wait(!got) {
            return Err(format!("the encoder took no frame for {} seconds", WAIT_LIMIT.as_secs()));
        }
    }
    Ok(())
}

/// Submit one frame ([`submit`]) and collect the outputs that are ready after the encoder took it.
/// An output that is late comes out with a later call.
pub(crate) fn encode(
    e: &mut impl Engine,
    frame: &[u8],
    time: i64,
    duration: i64,
    out: &mut Vec<Output>,
    wait: &mut impl FnMut(bool) -> bool,
) -> Result<(), String> {
    submit(e, frame, time, duration, out, wait)?;
    collect_ready(e, out).map(drop)
}

/// End the stream: drain the encoder and collect every output until it has returned them all,
/// pausing (through `wait`) while none is ready. Every retry and every output counts toward the
/// wait limit.
pub(crate) fn drain(e: &mut impl Engine, out: &mut Vec<Output>, wait: &mut impl FnMut(bool) -> bool) -> Result<(), String> {
    let late = || format!("the encoder did not return its last frames within {} seconds", WAIT_LIMIT.as_secs());
    while e.drain()? == Submitted::Full {
        let got = collect_ready(e, out)?;
        if !wait(!got) {
            return Err(late());
        }
    }
    loop {
        let pause = match e.poll()? {
            Polled::Output(o) => {
                out.push(o);
                false
            }
            Polled::Empty => true,
            Polled::Ended => return Ok(()),
        };
        if !wait(pause) {
            return Err(late());
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod none {
    use super::{Engine, OpenError, Params, Polled, Submitted};

    const WHY: &str = "AMF runs on Windows only";

    /// Whether `amfrt64.dll` loaded: never, off Windows.
    pub fn loaded() -> Result<(), String> {
        Err(WHY.into())
    }

    /// No encoder: AMF runs on Windows only.
    pub struct Session;

    impl Session {
        pub fn open(_: &Params) -> Result<Session, OpenError> {
            Err(OpenError::NoDevice(WHY.into()))
        }

        pub fn extra_data(&self) -> Option<Vec<u8>> {
            None
        }
    }

    impl Engine for Session {
        fn submit(&mut self, _: &[u8], _: i64, _: i64) -> Result<Submitted, String> {
            Err(WHY.into())
        }

        fn poll(&mut self) -> Result<Polled, String> {
            Ok(Polled::Ended)
        }

        fn drain(&mut self) -> Result<Submitted, String> {
            Ok(Submitted::Taken)
        }
    }
}
