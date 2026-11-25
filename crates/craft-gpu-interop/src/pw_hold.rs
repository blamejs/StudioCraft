//! A screen-cast stream's DMA-BUF buffers kept from the producer while the compositor reads them.
//! A buffer the consumer gives back can be written again at once, so each buffer whose frame was
//! handed to the compositor stays dequeued until the GPU has finished reading that frame
//! ([`SharedRing::release_dmabuf`](crate::SharedRing::release_dmabuf)), or until every copy of
//! that frame is gone without a draw.

use pipewire as pw;

use crate::dmabuf::HoldQueue;
use crate::pw_hold_ffi::{self as ffi, Raw};
use crate::ring::{FrameWatch, SharedFrame};

/// The first data plane of a buffer taken from a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldPlane {
    /// Whether the plane is a DMA-BUF.
    pub dmabuf: bool,
    /// The plane's descriptor; it stays the stream's.
    pub fd: i64,
    /// Where the buffer's data starts in the descriptor.
    pub mapoffset: u32,
    /// Where the picture starts, counted from `mapoffset`.
    pub offset: u32,
    /// How many bytes of the picture are valid. For a DMA-BUF the producer may set any value, 0
    /// included, so only 0 is read from it, as a picture that did not change.
    pub size: u32,
    /// Whether the producer marked the chunk's data as corrupted.
    pub corrupted: bool,
    /// How far apart the picture's rows are.
    pub stride: i32,
}

/// A buffer taken from a stream. It goes back to the stream when it is dropped, unless
/// [`HeldBuffers::keep`] keeps it.
pub struct Dequeued<'s> {
    buffer: Option<Raw>,
    stream: &'s pw::stream::Stream,
}

impl<'s> Dequeued<'s> {
    /// The newest buffer the stream has for the consumer, if any.
    pub fn take(stream: &'s pw::stream::Stream) -> Option<Dequeued<'s>> {
        ffi::dequeue(stream).map(|b| Dequeued { buffer: Some(b), stream })
    }

    /// The buffer's first data plane, or `None` for a buffer without one.
    pub fn plane(&self) -> Option<HeldPlane> {
        self.buffer.as_ref().and_then(ffi::plane)
    }
}

impl Drop for Dequeued<'_> {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            ffi::queue(self.stream, buffer);
        }
    }
}

/// The buffers one stream keeps from its producer until the GPU has read their frames. Each is
/// given back once the stream records that frame, or a later one, as read, or once every copy of
/// the frame is gone and no draw read it ([`FrameWatch::dropped_undrawn`]). Its owner calls
/// [`HeldBuffers::forget`] from the stream's `remove_buffer` callback, so that a buffer the stream
/// frees is never given back.
#[derive(Debug, Default)]
pub struct HeldBuffers {
    /// The stream the buffers came from, by address.
    stream: Option<usize>,
    held: HoldQueue<(Raw, FrameWatch)>,
}

impl HeldBuffers {
    pub fn new() -> HeldBuffers {
        HeldBuffers::default()
    }

    /// How many buffers are kept now.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    fn owns(&mut self, stream: &pw::stream::Stream) -> bool {
        let address = ffi::stream_address(stream);
        *self.stream.get_or_insert(address) == address
    }

    /// Keep `buffer` until the GPU has read `frame`, when fewer than `limit` buffers, and fewer
    /// than [`MAX_HELD`](crate::MAX_HELD), are kept. Returns whether it was kept; a buffer that was
    /// not, one of another stream than the earlier ones, or one whose frame was not made with
    /// [`SharedFrame::watched`], goes back unread.
    pub fn keep(&mut self, mut buffer: Dequeued<'_>, frame: &SharedFrame, limit: usize) -> bool {
        if !self.owns(buffer.stream) {
            return false;
        }
        let Some(watch) = frame.watch() else { return false };
        let Some(raw) = buffer.buffer.take() else { return false };
        match self.held.keep((raw, watch), frame.ready(), limit) {
            Ok(()) => true,
            Err((raw, _)) => {
                buffer.buffer = Some(raw);
                false
            }
        }
    }

    /// Give back the buffers whose frames have a ready value up to `released`, and those whose
    /// frames were dropped without a draw.
    pub fn release(&mut self, stream: &pw::stream::Stream, released: u64) {
        if self.owns(stream) {
            self.held.release(released, |(b, _)| ffi::queue(stream, b));
            self.held.release_unread(|(_, watch)| watch.dropped_undrawn(), |(b, _)| ffi::queue(stream, b));
        }
    }

    /// The stream removed the buffer at `address`: forget it without giving it back. Returns
    /// whether it was kept.
    pub fn forget(&mut self, address: usize) -> bool {
        self.held.forget(|(b, _)| b.address() == address)
    }
}
