//! One hardware encoder MFT, driven through its asynchronous events. Every hardware MFT uses the
//! asynchronous model: it asks for each input with a `METransformNeedInput` event and announces
//! each output with `METransformHaveOutput`.
//!
//! - The encoders Media Foundation lists for NV12 input and the codec's output are tried in its
//!   order, and the first one that takes the configuration is used.
//! - The rate control, the keyframe interval and the B-frame count are set through `ICodecAPI`
//!   before the media types. An encoder whose B-frame count cannot be set to 0, and does not
//!   report 0, is skipped.
//! - Frames go in as samples in system memory, and output samples are read back to Annex B bytes.
//! - A wait for the encoder's next event ends with an error after [`EVENT_TIMEOUT`].

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQuality, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFShutdown, IMFTransform, METransformDrainComplete, METransformHaveOutput, METransformNeedInput, MF_E_HW_MFT_FAILED_START_STREAMING,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFMediaType_Video, MFNominalRange_16_235, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_DATA_BUFFER_INCOMPLETE, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_Quality, eAVEncH264VProfile_High,
    eAVEncH265VProfile_Main_420_8,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface};

use super::api::{self, Api};
use super::{MAX_HEADER, MAX_SAMPLES_PER_OUTPUT, MAX_STREAM_CHANGES, OpenError, Output, Params, nominal_bitrate, output_size, quality};
use crate::{Codec, Rate};

/// How long a wait for the encoder's next event lasts before it is an error.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// What one event of the encoder asked for.
enum Event {
    NeedInput,
    HaveOutput,
    DrainComplete,
    Other,
}

/// What one `ProcessOutput` call did.
enum Read {
    /// It returned a sample, or had none yet.
    Done,
    /// It returned a sample, and more output is ready.
    Incomplete,
    /// The encoder changed its output type, and the output is still to be read.
    StreamChanged,
}

/// Why one encoder MFT did not open, and whether it lacked the hardware resources to start
/// streaming, which is what an MFT reports when other programs hold its sessions.
struct Refused {
    why: String,
    busy: bool,
}

impl From<String> for Refused {
    fn from(why: String) -> Refused {
        Refused { why, busy: false }
    }
}

/// A failed call that creates or starts the encoder, as a [`Refused`].
fn refused(what: String, e: &windows::core::Error) -> Refused {
    Refused { why: format!("{what}: {e}"), busy: e.code() == MF_E_HW_MFT_FAILED_START_STREAMING }
}

/// A hardware encoder MFT, set up and streaming.
pub struct Transform {
    mft: IMFTransform,
    activate: IMFActivate,
    events: IMFMediaEventGenerator,
    api: &'static Api,
    name: String,
    input_id: u32,
    output_id: u32,
    /// The frame size.
    width: u32,
    height: u32,
    /// Whether the MFT allocates its output samples.
    provides_samples: bool,
    /// The buffer size of an output sample this side allocates.
    output_size: u32,
    /// Input requests not answered yet.
    wanted: u32,
    /// Set by [`Transform::drain`]: the stream has ended.
    ended: bool,
    header: Option<Vec<u8>>,
}

// SAFETY: hardware encoder MFTs are free-threaded COM objects, and a `Transform` is used from one
// thread at a time (every call takes `&mut self` or `&self` of an owned value). Each thread that
// calls it sets up COM first.
unsafe impl Send for Transform {}

impl Transform {
    /// The first hardware encoder MFT that takes `p`, set up and streaming. When none opens and
    /// one of them lacked the resources to start streaming, the error is [`OpenError::Busy`].
    pub fn open(p: &Params) -> Result<Transform, OpenError> {
        api::ensure_com().map_err(OpenError::NoDevice)?;
        let api = api::api().map_err(OpenError::NoDevice)?;
        let (subtype, label) = match p.codec {
            Codec::H264 => (MFVideoFormat_H264, "H.264"),
            Codec::Hevc => (MFVideoFormat_HEVC, "HEVC"),
        };
        let found = api.encoders(subtype).map_err(OpenError::NoDevice)?;
        if found.is_empty() {
            return Err(OpenError::NoDevice(format!("Media Foundation lists no hardware {label} encoder")));
        }
        let (mut reasons, mut busy) = (Vec::new(), false);
        for activate in found {
            match Transform::configure(api, activate, p, subtype) {
                Ok(t) => return Ok(t),
                Err(e) => {
                    busy |= e.busy;
                    reasons.push(e.why);
                }
            }
        }
        let why = reasons.join("; ");
        Err(if busy { OpenError::Busy(why) } else { OpenError::Unsupported(why) })
    }

    fn configure(api: &'static Api, activate: IMFActivate, p: &Params, subtype: GUID) -> Result<Transform, Refused> {
        let name = api::friendly_name(&activate).unwrap_or_else(|| "a hardware encoder".into());
        // SAFETY: plain COM calls on live interfaces.
        let mft: IMFTransform = unsafe { activate.ActivateObject() }.map_err(|e| refused(format!("{name} cannot be created"), &e))?;
        // SAFETY: plain COM calls on live interfaces.
        let events = unsafe {
            let unlocked = mft.GetAttributes().and_then(|attrs| {
                if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                    return Ok(false);
                }
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map(|()| true)
            });
            match unlocked {
                Ok(true) => mft.cast::<IMFMediaEventGenerator>().map_err(|e| format!("{name} sends no events: {e}")),
                Ok(false) => Err(format!("{name} is not an asynchronous MFT")),
                Err(e) => Err(format!("{name} cannot be unlocked: {e}")),
            }
        };
        let events = match events {
            Ok(e) => e,
            Err(e) => {
                // SAFETY: a plain COM call; the object was activated above and is not used again.
                let _ = unsafe { activate.ShutdownObject() };
                return Err(e.into());
            }
        };
        let mut t = Transform {
            mft,
            activate,
            events,
            api,
            name,
            input_id: 0,
            output_id: 0,
            width: p.width,
            height: p.height,
            provides_samples: false,
            output_size: 0,
            wanted: 0,
            ended: false,
            header: None,
        };
        t.set_up(p, subtype)?;
        Ok(t)
    }

    fn set_up(&mut self, p: &Params, subtype: GUID) -> Result<(), Refused> {
        let name = self.name.clone();
        let (mut inputs, mut outputs) = ([0u32], [0u32]);
        // SAFETY: plain COM calls on live interfaces with arrays of one stream id each.
        match unsafe { self.mft.GetStreamIDs(&mut inputs, &mut outputs) } {
            Ok(()) => (self.input_id, self.output_id) = (inputs[0], outputs[0]),
            // Streams numbered from 0.
            Err(e) if e.code() == E_NOTIMPL => {}
            Err(e) => return Err(format!("{name} has no stream ids: {e}").into()),
        }
        let codec_api: ICodecAPI = self.mft.cast().map_err(|e| format!("{name} has no encoder settings: {e}"))?;
        let set = |key: &GUID, value: u32, what: &str| {
            let v = VARIANT::from(value);
            // SAFETY: a plain COM call with a key and a VT_UI4 value that live through it.
            unsafe { codec_api.SetValue(key, &v) }.map_err(|e| format!("{name} does not take {what}: {e}"))
        };
        let bits_per_second = match p.rate {
            Rate::Quality(q) => {
                set(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_Quality.0 as u32, "quality rate control")?;
                set(&CODECAPI_AVEncCommonQuality, quality(q), "the quality level")?;
                nominal_bitrate(p)
            }
            Rate::Cbr(kbps) => {
                let bps = kbps.saturating_mul(1000);
                set(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32, "constant bitrate")?;
                set(&CODECAPI_AVEncCommonMeanBitRate, bps, "the bitrate")?;
                set(&CODECAPI_AVEncCommonBufferSize, bps, "a one-second buffer")?;
                bps
            }
        };
        set(&CODECAPI_AVEncMPVGOPSize, p.gop, "the keyframe interval")?;
        // Packets must come out in input order. An encoder that does not take the setting is used
        // only when it reports 0 B-frames already.
        if let Err(e) = set(&CODECAPI_AVEncMPVDefaultBPictureCount, 0, "a B-frame count of 0") {
            // SAFETY: a plain COM call with a key that lives through it.
            let current = unsafe { codec_api.GetValue(&CODECAPI_AVEncMPVDefaultBPictureCount) };
            if !current.ok().is_some_and(|v| u32::try_from(&v).is_ok_and(|n| n == 0)) {
                return Err(e.into());
            }
        }

        let profile = match p.codec {
            Codec::H264 => eAVEncH264VProfile_High.0 as u32,
            Codec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
        };
        let output = self.api.media_type()?;
        describe(&output, p, subtype).map_err(|e| format!("{name}: {e}"))?;
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            output.SetUINT32(&MF_MT_AVG_BITRATE, bits_per_second).map_err(|e| e.to_string())?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).map_err(|e| e.to_string())?;
            self.mft.SetOutputType(self.output_id, &output, 0).map_err(|e| format!("{name} does not write this stream: {e}"))?;
        }
        let input = self.api.media_type()?;
        describe(&input, p, MFVideoFormat_NV12).map_err(|e| format!("{name}: {e}"))?;
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, p.width).map_err(|e| e.to_string())?;
            self.mft.SetInputType(self.input_id, &input, 0).map_err(|e| format!("{name} does not take these frames: {e}"))?;
        }
        self.stream_info()?;
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| refused(format!("{name} does not start"), &e))?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| refused(format!("{name} does not start"), &e))?;
        }
        self.header = self.read_header();
        Ok(())
    }

    /// Read who allocates output samples and how large this side's must be.
    fn stream_info(&mut self) -> Result<(), String> {
        // SAFETY: a plain COM call on a live interface.
        let info = unsafe { self.mft.GetOutputStreamInfo(self.output_id) }.map_err(|e| format!("{} has no output stream info: {e}", self.name))?;
        let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        self.provides_samples = info.dwFlags & provides != 0;
        self.output_size = output_size(info.cbSize, self.width, self.height).map_err(|e| format!("{}: {e}", self.name))?;
        Ok(())
    }

    /// The parameter sets the output type carries, as Annex B, when it carries them and they are
    /// at most [`MAX_HEADER`] bytes.
    fn read_header(&self) -> Option<Vec<u8>> {
        // SAFETY: plain COM calls on live interfaces; `GetBlob` writes at most the length of the
        // buffer it is given, which is the size `GetBlobSize` reported.
        unsafe {
            let t = self.mft.GetOutputCurrentType(self.output_id).ok()?;
            let len = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
            if len > MAX_HEADER {
                return None;
            }
            let mut buf = vec![0u8; len as usize];
            let mut written = 0u32;
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, Some(&mut written)).ok()?;
            buf.truncate(written as usize);
            (!buf.is_empty()).then_some(buf)
        }
    }

    /// The parameter sets the encoder's output type carries, as Annex B, when it carries them.
    pub fn sequence_header(&self) -> Option<&[u8]> {
        self.header.as_deref()
    }

    /// Send one NV12 frame at `time` for `duration` (100 ns units), after the encoder asks for
    /// input. The samples that came out meanwhile are added to `out`.
    pub fn encode(&mut self, frame: &[u8], time: i64, duration: i64, out: &mut Vec<Output>) -> Result<(), String> {
        api::ensure_com()?;
        if self.ended {
            return Err(format!("{}: the stream has ended", self.name));
        }
        let sample = self.api.sample_of(frame, time, duration)?;
        while self.wanted == 0 {
            match self.next_event(true)? {
                Some(Event::NeedInput) => self.wanted += 1,
                Some(Event::HaveOutput) => self.read(out)?,
                Some(Event::DrainComplete | Event::Other) | None => {}
            }
        }
        // SAFETY: a plain COM call on live interfaces.
        unsafe { self.mft.ProcessInput(self.input_id, &sample, 0) }.map_err(|e| format!("{}: a frame was refused: {e}", self.name))?;
        self.wanted -= 1;
        while let Some(event) = self.next_event(false)? {
            match event {
                Event::NeedInput => self.wanted += 1,
                Event::HaveOutput => self.read(out)?,
                Event::DrainComplete | Event::Other => {}
            }
        }
        Ok(())
    }

    /// End the stream: every sample still inside is added to `out`.
    pub fn drain(&mut self, out: &mut Vec<Output>) -> Result<(), String> {
        api::ensure_com()?;
        if self.ended {
            return Ok(());
        }
        self.ended = true;
        // SAFETY: plain COM calls on a live interface.
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, self.input_id as usize).map_err(|e| format!("{}: {e}", self.name))?;
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, self.input_id as usize).map_err(|e| format!("{}: cannot drain: {e}", self.name))?;
        }
        loop {
            match self.next_event(true)? {
                Some(Event::HaveOutput) => self.read(out)?,
                Some(Event::DrainComplete) => return Ok(()),
                Some(Event::NeedInput | Event::Other) | None => {}
            }
        }
    }

    /// The next event. With `wait`, it polls for up to [`EVENT_TIMEOUT`] and then fails; without,
    /// it returns `None` when none is queued.
    fn next_event(&self, wait: bool) -> Result<Option<Event>, String> {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let event = loop {
            // SAFETY: a plain COM call on a live interface.
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => break e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if !wait {
                        return Ok(None);
                    }
                    if Instant::now() >= deadline {
                        return Err(format!("{} sent no event for {} seconds", self.name, EVENT_TIMEOUT.as_secs()));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(format!("{}: no event: {e}", self.name)),
            }
        };
        // SAFETY: plain COM calls on a live interface.
        unsafe {
            let status = event.GetStatus().map_err(|e| format!("{}: {e}", self.name))?;
            if status.is_err() {
                return Err(format!("{} failed ({status:?})", self.name));
            }
            let kind = event.GetType().map_err(|e| format!("{}: {e}", self.name))?;
            Ok(Some(match kind {
                k if k == METransformNeedInput.0 as u32 => Event::NeedInput,
                k if k == METransformHaveOutput.0 as u32 => Event::HaveOutput,
                k if k == METransformDrainComplete.0 as u32 => Event::DrainComplete,
                _ => Event::Other,
            }))
        }
    }

    /// Take the output the encoder announced and add it to `out`. After the encoder changes its
    /// output type, the new type is set and the output is read again; while the encoder marks a
    /// sample incomplete, the next one is read too, up to [`MAX_SAMPLES_PER_OUTPUT`] samples.
    fn read(&mut self, out: &mut Vec<Output>) -> Result<(), String> {
        let (mut changes, mut samples) = (0, 0);
        loop {
            match self.read_once(out)? {
                Read::Done => return Ok(()),
                Read::Incomplete => {
                    samples += 1;
                    if samples >= MAX_SAMPLES_PER_OUTPUT {
                        return Err(format!("{}: one output held more than {MAX_SAMPLES_PER_OUTPUT} samples", self.name));
                    }
                }
                Read::StreamChanged => {
                    changes += 1;
                    if changes > MAX_STREAM_CHANGES {
                        return Err(format!("{}: the output type changed {changes} times in a row", self.name));
                    }
                    self.renegotiate()?;
                }
            }
        }
    }

    fn read_once(&mut self, out: &mut Vec<Output>) -> Result<Read, String> {
        let ours = if self.provides_samples { None } else { Some(self.api.empty_sample(self.output_size)?.0) };
        let mut buffers =
            [MFT_OUTPUT_DATA_BUFFER { dwStreamID: self.output_id, pSample: ManuallyDrop::new(ours), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0u32;
        // SAFETY: one output buffer descriptor (with this side's sample, or none when the MFT
        // provides its own) and a live status out-pointer.
        let r = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };
        // SAFETY: the descriptor's fields were set above and are taken once; the MFT may have
        // replaced them even on failure, and dropping them releases the references it handed over.
        let (sample, _events) = unsafe { (ManuallyDrop::take(&mut buffers[0].pSample), ManuallyDrop::take(&mut buffers[0].pEvents)) };
        match r {
            Ok(()) => {
                let sample = sample.ok_or_else(|| format!("{} returned no sample", self.name))?;
                out.push(self.bytes_of(&sample)?);
                let incomplete = buffers[0].dwStatus & MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32 != 0;
                Ok(if incomplete { Read::Incomplete } else { Read::Done })
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Read::Done),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Read::StreamChanged),
            Err(e) => Err(format!("{}: reading a frame failed: {e}", self.name)),
        }
    }

    /// Set the output type the encoder offers first after it changed its output, and read the
    /// output stream's buffer needs again.
    fn renegotiate(&mut self) -> Result<(), String> {
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            let t = self.mft.GetOutputAvailableType(self.output_id, 0).map_err(|e| format!("{}: no output type: {e}", self.name))?;
            self.mft.SetOutputType(self.output_id, &t, 0).map_err(|e| format!("{}: refused its own output type: {e}", self.name))?;
        }
        self.stream_info()?;
        if let Some(h) = self.read_header() {
            self.header = Some(h);
        }
        Ok(())
    }

    /// The bytes and time of an output sample. A sample longer than this side's output buffers
    /// ([`output_size`]) is an error, also when the MFT allocated it.
    fn bytes_of(&self, sample: &IMFSample) -> Result<Output, String> {
        // SAFETY: `Lock` gives a readable block of `len` bytes valid until `Unlock`; it is copied
        // before the buffer is unlocked.
        unsafe {
            let time = sample.GetSampleTime().ok();
            let total = sample.GetTotalLength().map_err(|e| format!("{}: cannot size a frame: {e}", self.name))?;
            if total > self.output_size {
                return Err(format!("{}: a frame of {total} bytes is larger than {} bytes", self.name, self.output_size));
            }
            let buffer = sample.ConvertToContiguousBuffer().map_err(|e| format!("{}: cannot read a frame: {e}", self.name))?;
            let (mut ptr, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
            buffer.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| format!("{}: cannot lock a frame: {e}", self.name))?;
            let data = if ptr.is_null() { Vec::new() } else { std::slice::from_raw_parts(ptr, len.min(total) as usize).to_vec() };
            buffer.Unlock().map_err(|e| format!("{}: cannot unlock a frame: {e}", self.name))?;
            Ok(Output { data, time })
        }
    }
}

impl Drop for Transform {
    fn drop(&mut self) {
        if api::ensure_com().is_err() {
            return;
        }
        // SAFETY: plain COM calls on live interfaces; the MFT is not used after them.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// Fill the video attributes both media types share: `subtype`, size, frame rate, square pixels,
/// progressive frames and BT.709 at limited range.
fn describe(t: &IMFMediaType, p: &Params, subtype: GUID) -> Result<(), String> {
    // SAFETY: plain COM calls on a live interface.
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(|e| e.to_string())?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(p.width) << 32) | u64::from(p.height)).map_err(|e| e.to_string())?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(p.fps.0) << 32) | u64::from(p.fps.1)).map_err(|e| e.to_string())?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).map_err(|e| e.to_string())?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32).map_err(|e| e.to_string())?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32).map_err(|e| e.to_string())?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32).map_err(|e| e.to_string())?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32).map_err(|e| e.to_string())?;
    }
    Ok(())
}
