//! The parts of Linux screen capture that need no desktop: copying a stream buffer into a frame, and
//! handing the newest frame from the stream thread to the source thread. Camera capture hands its
//! frames over the same way on every system.

use std::io::{PipeWriter, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use craft_gpu_interop::SharedFrame;
use crossbeam_queue::ArrayQueue;
use studiocraft_media::video::{PixelFormat, VideoFrame};

/// The byte order of a 4-byte pixel in a stream buffer. The `x` layouts carry no alpha.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Rgba,
    Rgbx,
    Bgra,
    Bgrx,
}

/// Copy a `width` x `height` picture with rows `stride` bytes apart, starting `offset` bytes into
/// `bytes` (modulo its length) and `size` bytes long, into `frame`. A layout without alpha is made opaque. Returns
/// `false` and leaves `frame` alone when the picture does not fit in the buffer.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn copy_frame(bytes: &[u8], offset: u32, size: u32, stride: i32, width: u32, height: u32, layout: Layout, frame: &mut VideoFrame) -> bool {
    let (Ok(w), Ok(h), Ok(stride)) = (usize::try_from(width), usize::try_from(height), usize::try_from(stride)) else { return false };
    let Some(row) = w.checked_mul(4) else { return false };
    if w == 0 || h == 0 || stride < row {
        return false;
    }
    if bytes.is_empty() {
        return false;
    }
    // PipeWire chunk offsets are taken modulo the buffer's length.
    let start = usize::try_from(offset).map_or(0, |o| o % bytes.len());
    let end = start.saturating_add(usize::try_from(size).unwrap_or(usize::MAX)).min(bytes.len());
    let Some(region) = bytes.get(start..end) else { return false };
    // The last row needs only its pixels, not a whole stride.
    let Some(needed) = (h - 1).checked_mul(stride).and_then(|n| n.checked_add(row)) else { return false };
    if region.len() < needed {
        return false;
    }
    let format = match layout {
        Layout::Rgba | Layout::Rgbx => PixelFormat::Rgba8,
        Layout::Bgra | Layout::Bgrx => PixelFormat::Bgra8,
    };
    if !frame.reshape(width, height, format) {
        return false;
    }
    let opaque = matches!(layout, Layout::Rgbx | Layout::Bgrx);
    for (dst, src) in frame.data.chunks_exact_mut(row).zip(region.chunks(stride)) {
        let Some(src) = src.get(..row) else { return false };
        dst.copy_from_slice(src);
        if opaque {
            dst.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
        }
    }
    true
}

/// Where a screen capture stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The system picker is open, or the stream has not delivered a picture yet.
    Waiting,
    Streaming,
    /// The person canceled the system picker.
    Canceled,
    /// The stream ended: sharing was stopped from the desktop, or the shared window closed.
    Ended,
    Failed,
    /// The desktop has no screen-cast portal, or its session bus could not be reached.
    NoPortal,
}

impl State {
    fn from_u8(v: u8) -> State {
        match v {
            1 => State::Streaming,
            2 => State::Canceled,
            3 => State::Ended,
            4 => State::Failed,
            5 => State::NoPortal,
            _ => State::Waiting,
        }
    }
}

/// Hands the newest picture from the stream thread to the source thread without locks. The stream
/// thread fills a spare frame and publishes it; a picture the source thread has not taken yet is
/// replaced by the newer one and goes back to the spares. A stream whose buffers stay on the GPU
/// publishes shared frames the same way, the newest replacing one not taken yet.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct FrameSlot {
    newest: ArrayQueue<VideoFrame>,
    spare: ArrayQueue<VideoFrame>,
    state: AtomicU8,
    shared: ArrayQueue<SharedFrame>,
    /// Whether the source asked for pictures on the GPU, and whether the stream thread has read
    /// the request ([`FrameSlot::take_gpu_request`]): one of the `SHARE_` values.
    share: AtomicU8,
    /// Whether the stream's buffers are on the GPU now.
    on_gpu: AtomicBool,
    /// The pipe that wakes the stream thread's loop to leave the GPU ([`FrameSlot::wake_stream`]).
    wake: OnceLock<PipeWriter>,
}

/// The states of [`FrameSlot`]'s GPU request: not asked for, asked for, and read by the stream
/// thread with or without the request.
const SHARE_NONE: u8 = 0;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SHARE_ASKED: u8 = 1;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SHARE_READ: u8 = 2;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SHARE_READ_ASKED: u8 = 3;

/// Frames a capture keeps for reuse besides the newest one and the one on screen.
pub const SPARE_FRAMES: usize = 2;

impl Default for FrameSlot {
    fn default() -> Self {
        FrameSlot {
            newest: ArrayQueue::new(1),
            spare: ArrayQueue::new(SPARE_FRAMES),
            state: AtomicU8::new(0),
            shared: ArrayQueue::new(1),
            share: AtomicU8::new(SHARE_NONE),
            on_gpu: AtomicBool::new(false),
            wake: OnceLock::new(),
        }
    }
}

impl FrameSlot {
    pub fn state(&self) -> State {
        State::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Set the state; a capture that was canceled, ended or failed stays that way.
    pub fn set_state(&self, s: State) {
        let v = match s {
            State::Waiting => 0,
            State::Streaming => 1,
            State::Canceled => 2,
            State::Ended => 3,
            State::Failed => 4,
            State::NoPortal => 5,
        };
        let mut old = self.state.load(Ordering::Acquire);
        while old <= 1 {
            match self.state.compare_exchange_weak(old, v, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => old = now,
            }
        }
    }

    /// A frame for the stream thread to fill: a spare one, or a new one when there is none.
    pub fn spare(&self) -> VideoFrame {
        self.spare.pop().unwrap_or_default()
    }

    /// Give back a frame from [`FrameSlot::spare`] that was not filled.
    pub fn recycle(&self, frame: VideoFrame) {
        let _ = self.spare.push(frame);
    }

    /// Publish a filled frame.
    pub fn publish(&self, frame: VideoFrame) {
        if let Some(old) = self.newest.force_push(frame) {
            let _ = self.spare.push(old);
        }
        self.set_state(State::Streaming);
    }

    /// Swap the newest picture into `frame`, keeping `frame`'s old buffer as a spare. Returns
    /// `false` when no new picture has arrived since the last call.
    pub fn take(&self, frame: &mut VideoFrame) -> bool {
        let Some(mut newest) = self.newest.pop() else { return false };
        std::mem::swap(frame, &mut newest);
        let _ = self.spare.push(newest);
        true
    }
}

/// The pictures on the GPU, which only a Linux screen capture publishes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl FrameSlot {
    /// Ask for the stream's pictures on the GPU. Returns `false` when the stream thread has already
    /// read the request without it, so the stream offers memory formats only.
    pub fn request_gpu(&self) -> bool {
        match self.share.compare_exchange(SHARE_NONE, SHARE_ASKED, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => true,
            Err(now) => now == SHARE_ASKED || now == SHARE_READ_ASKED,
        }
    }

    #[cfg(test)]
    pub fn gpu_requested(&self) -> bool {
        matches!(self.share.load(Ordering::Acquire), SHARE_ASKED | SHARE_READ_ASKED)
    }

    /// Read the request for the stream thread, once, before the stream offers its formats. A
    /// request made after this is refused ([`FrameSlot::request_gpu`]).
    pub fn take_gpu_request(&self) -> bool {
        // Once the request is asked for, only this call changes it again.
        match self.share.compare_exchange(SHARE_NONE, SHARE_READ, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => false,
            Err(SHARE_ASKED) => {
                self.share.store(SHARE_READ_ASKED, Ordering::Release);
                true
            }
            Err(now) => now == SHARE_READ_ASKED,
        }
    }

    /// Record whether the stream's buffers are on the GPU. Turning it off drops a shared frame not
    /// taken yet.
    pub fn set_on_gpu(&self, on: bool) {
        self.on_gpu.store(on, Ordering::Release);
        if !on {
            let _ = self.shared.pop();
        }
    }

    pub fn on_gpu(&self) -> bool {
        self.on_gpu.load(Ordering::Acquire)
    }

    /// Publish a picture on the GPU, replacing one not taken yet.
    pub fn publish_shared(&self, frame: SharedFrame) {
        let _ = self.shared.force_push(frame);
        self.set_state(State::Streaming);
    }

    /// The newest picture on the GPU since the last call.
    pub fn take_shared(&self) -> Option<SharedFrame> {
        self.shared.pop()
    }

    /// Give the stream thread the pipe through which [`FrameSlot::wake_stream`] reaches it. Only
    /// the first call keeps its pipe.
    pub fn set_wake(&self, wake: PipeWriter) {
        let _ = self.wake.set(wake);
    }

    /// Ask the stream thread to leave the GPU without waiting for its next picture, which a still
    /// screen does not send. It writes one byte, which the stream thread reads when it wakes.
    pub fn wake_stream(&self) {
        if let Some(mut wake) = self.wake.get() {
            let _ = wake.write(&[1]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2 x 2 picture with 12-byte rows (4 bytes of padding), pixels numbered from 1.
    fn padded() -> Vec<u8> {
        let mut b = Vec::new();
        for y in 0..2u8 {
            for x in 0..2u8 {
                let p = 1 + y * 2 + x;
                b.extend([p, p + 10, p + 20, 0]);
            }
            b.extend([99; 4]);
        }
        b
    }

    #[test]
    fn rows_are_copied_without_their_padding_and_x_layouts_are_opaque() {
        let b = padded();
        let mut f = VideoFrame::default();
        assert!(copy_frame(&b, 0, 24, 12, 2, 2, Layout::Bgrx, &mut f));
        assert_eq!((f.width, f.height, f.format), (2, 2, PixelFormat::Bgra8));
        assert_eq!(f.data, [1, 11, 21, 255, 2, 12, 22, 255, 3, 13, 23, 255, 4, 14, 24, 255]);
        assert!(copy_frame(&b, 0, 24, 12, 2, 2, Layout::Rgba, &mut f));
        assert_eq!(f.format, PixelFormat::Rgba8);
        assert_eq!(f.data, [1, 11, 21, 0, 2, 12, 22, 0, 3, 13, 23, 0, 4, 14, 24, 0]);
        assert!(copy_frame(&b, 0, 24, 12, 2, 2, Layout::Bgra, &mut f));
        assert_eq!(f.format, PixelFormat::Bgra8);
        assert_eq!(f.data[3], 0, "a layout with alpha keeps it");
    }

    #[test]
    fn the_last_row_needs_no_padding() {
        let b = padded();
        let mut f = VideoFrame::default();
        assert!(copy_frame(&b, 0, 20, 12, 2, 2, Layout::Rgbx, &mut f));
        assert!(!copy_frame(&b, 0, 19, 12, 2, 2, Layout::Rgbx, &mut f));
    }

    #[test]
    fn a_picture_that_does_not_fit_is_refused() {
        let b = padded();
        let mut f = VideoFrame::default();
        assert!(copy_frame(&b, 0, 24, 12, 2, 2, Layout::Rgba, &mut f));
        let before = f.data.clone();
        // Rows shorter than the pixels, negative strides, empty and oversized pictures, offsets
        // past the end, and a size that the buffer cuts short.
        assert!(!copy_frame(&b, 0, 24, 4, 2, 2, Layout::Rgba, &mut f));
        assert!(!copy_frame(&b, 0, 24, -12, 2, 2, Layout::Rgba, &mut f));
        assert!(!copy_frame(&b, 0, 24, 12, 0, 2, Layout::Rgba, &mut f));
        assert!(!copy_frame(&b, 0, 24, 12, 2, 0, Layout::Rgba, &mut f));
        assert!(!copy_frame(&b, 0, 24, 12, 2, 3, Layout::Rgba, &mut f));
        assert!(!copy_frame(&b, 8, 24, 12, 2, 2, Layout::Rgba, &mut f));
        assert!(!copy_frame(&b, 8, u32::MAX, 12, 2, 2, Layout::Rgba, &mut f));
        assert!(!copy_frame(&[], 0, 24, 12, 2, 2, Layout::Rgba, &mut f));
        assert!(!copy_frame(&b, 0, u32::MAX, i32::MAX, u32::MAX, u32::MAX, Layout::Rgba, &mut f));
        assert_eq!(f.data, before, "a refused picture leaves the frame alone");
    }

    #[test]
    fn a_chunk_size_past_the_buffer_is_cut_to_the_buffer() {
        let b = padded();
        let mut f = VideoFrame::default();
        assert!(copy_frame(&b, 0, u32::MAX, 12, 2, 2, Layout::Rgba, &mut f));
        assert_eq!(f.data[12], 4);
    }

    #[test]
    fn an_offset_is_taken_modulo_the_buffer_length() {
        let b = padded();
        let mut f = VideoFrame::default();
        assert!(copy_frame(&b, 24, 24, 12, 2, 2, Layout::Rgba, &mut f));
        assert_eq!(f.data[0], 1);
        assert!(copy_frame(&b, 36, 12, 12, 2, 1, Layout::Rgba, &mut f));
        assert_eq!(f.data[0], 3);
    }

    #[test]
    fn a_resized_stream_reshapes_the_frame() {
        let mut f = VideoFrame::default();
        assert!(copy_frame(&[7; 16], 0, 16, 8, 2, 2, Layout::Rgba, &mut f));
        assert_eq!((f.width, f.height), (2, 2));
        assert!(copy_frame(&[7; 12], 0, 12, 12, 3, 1, Layout::Rgba, &mut f));
        assert_eq!((f.width, f.height, f.data.len()), (3, 1, 12));
    }

    fn frame(w: u32) -> VideoFrame {
        let mut f = VideoFrame::default();
        assert!(f.reshape(w, 1, PixelFormat::Rgba8));
        f
    }

    #[test]
    fn the_newest_picture_replaces_one_not_taken_yet() {
        let slot = FrameSlot::default();
        let mut out = VideoFrame::default();
        assert!(!slot.take(&mut out), "nothing published yet");
        assert_eq!(slot.state(), State::Waiting);
        slot.publish(frame(1));
        slot.publish(frame(2));
        assert_eq!(slot.state(), State::Streaming);
        assert!(slot.take(&mut out));
        assert_eq!(out.width, 2);
        assert!(!slot.take(&mut out), "each picture is taken once");
        // The replaced picture and the taken frame's old buffer are reused.
        assert_eq!(slot.spare().width, 1);
        assert_eq!(slot.spare().width, 0);
    }

    #[test]
    fn a_capture_that_stopped_stays_stopped() {
        for end in [State::Canceled, State::Ended, State::Failed, State::NoPortal] {
            let slot = FrameSlot::default();
            slot.set_state(end);
            slot.publish(frame(1));
            slot.set_state(State::Waiting);
            assert_eq!(slot.state(), end);
        }
    }

    #[test]
    fn a_gpu_request_counts_only_before_the_stream_thread_reads_it() {
        let early = FrameSlot::default();
        assert!(early.request_gpu());
        assert!(early.take_gpu_request());
        assert!(early.request_gpu(), "asking again changes nothing");
        assert!(early.gpu_requested());
        let late = FrameSlot::default();
        assert!(!late.take_gpu_request());
        assert!(!late.request_gpu(), "the stream already offers memory formats only");
        assert!(!late.gpu_requested());
        assert!(!late.take_gpu_request());
    }

    #[test]
    fn the_newest_gpu_picture_replaces_one_not_taken_and_leaving_the_gpu_drops_it() {
        let slot = FrameSlot::default();
        assert!(!slot.gpu_requested() && !slot.on_gpu());
        slot.request_gpu();
        assert!(slot.gpu_requested());
        let ring = craft_gpu_interop::SharedRing::detached(4, 2);
        let shared = |ready| SharedFrame::new(std::sync::Arc::clone(&ring), 0, ready).unwrap();
        assert!(slot.take_shared().is_none());
        slot.set_on_gpu(true);
        slot.publish_shared(shared(1));
        slot.publish_shared(shared(2));
        assert_eq!(slot.state(), State::Streaming);
        assert_eq!(slot.take_shared().map(|f| f.ready()), Some(2));
        assert!(slot.take_shared().is_none(), "each picture is taken once");
        slot.publish_shared(shared(3));
        slot.set_on_gpu(false);
        assert!(!slot.on_gpu());
        assert!(slot.take_shared().is_none(), "a picture of a stream that left the GPU is not shown");
    }

    #[test]
    fn spares_are_bounded() {
        let slot = FrameSlot::default();
        let mut out = VideoFrame::default();
        for w in 1..10 {
            slot.publish(frame(w));
            assert!(slot.take(&mut out));
        }
        let mut count = 0;
        while slot.spare.pop().is_some() {
            count += 1;
        }
        assert!(count <= SPARE_FRAMES);
    }
}
