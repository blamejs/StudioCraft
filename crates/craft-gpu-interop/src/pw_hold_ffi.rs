//! PipeWire's buffer queue by pointer: taking a stream's newest buffer, reading its first data
//! plane, and giving it back. Every PipeWire call of [`crate::pw_hold`] that needs `unsafe` is
//! here.

use std::ptr::NonNull;

use pipewire as pw;
use pw::spa;

use crate::pw_hold::HeldPlane;

/// A buffer dequeued from a stream, which belongs to the caller until [`queue`] gives it back.
#[derive(Debug)]
pub(crate) struct Raw(NonNull<pw::sys::pw_buffer>);

impl Raw {
    /// The buffer's address, which names it in the stream's `remove_buffer` callback.
    pub(crate) fn address(&self) -> usize {
        self.0.as_ptr() as usize
    }
}

/// The stream's address, which tells streams apart. It is only compared, never followed.
pub(crate) fn stream_address(stream: &pw::stream::Stream) -> usize {
    stream.as_raw_ptr() as usize
}

/// The newest buffer `stream` has for the consumer, if any.
pub(crate) fn dequeue(stream: &pw::stream::Stream) -> Option<Raw> {
    // SAFETY: the stream is a live stream for as long as `stream` borrows it. A NULL result means
    // it has no buffer, and any other pointer is a buffer that now belongs to the caller until it
    // is queued back on this stream.
    NonNull::new(unsafe { stream.dequeue_raw_buffer() }).map(Raw)
}

/// The first data plane of `buffer`, or `None` for a buffer without one.
pub(crate) fn plane(buffer: &Raw) -> Option<HeldPlane> {
    // SAFETY: the buffer is dequeued and stays valid until it is queued back, which takes it by
    // value. PipeWire fills `buffer`, `datas` and `chunk` before it hands a buffer over, and each
    // pointer is checked for NULL before it is read.
    unsafe {
        let spa_buffer = buffer.0.as_ref().buffer;
        if spa_buffer.is_null() || (*spa_buffer).n_datas == 0 || (*spa_buffer).datas.is_null() {
            return None;
        }
        let data = &*(*spa_buffer).datas;
        if data.chunk.is_null() {
            return None;
        }
        let chunk = &*data.chunk;
        Some(HeldPlane {
            dmabuf: data.type_ == spa::sys::SPA_DATA_DmaBuf,
            fd: data.fd,
            mapoffset: data.mapoffset,
            offset: chunk.offset,
            size: chunk.size,
            stride: chunk.stride,
        })
    }
}

/// Give `buffer` back to `stream`, the stream it was dequeued from.
pub(crate) fn queue(stream: &pw::stream::Stream, buffer: Raw) {
    // SAFETY: every caller passes the stream the buffer was dequeued from: `Dequeued` keeps that
    // stream, and `HeldBuffers` queues only on the stream whose address it recorded with the
    // buffer. A buffer the stream removed is forgotten in its `remove_buffer` callback before the
    // stream frees it, so the buffer is still the stream's.
    unsafe { stream.queue_raw_buffer(buffer.0.as_ptr()) };
}
