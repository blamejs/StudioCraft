//! The RTMP chunk stream (specification §5.3): splitting messages into chunks with compressed
//! headers, and putting them back together.

use std::io::{self, IoSlice, Write};

use crate::Error;

/// The chunk size each direction starts with (§5.4.1).
pub const DEFAULT_CHUNK_SIZE: usize = 128;
/// The chunk size [`crate::Publisher`] sets for what it sends.
pub const PUBLISH_CHUNK_SIZE: u32 = 4096;
/// The longest message: the message length field has 24 bits.
pub const MAX_MESSAGE: usize = 0xFF_FFFF;
/// The largest chunk stream id (§5.3.1.1).
const MAX_CSID: u32 = 65_599;
/// The most chunk streams a [`ChunkReader`] keeps state for.
pub const MAX_CHUNK_STREAMS: usize = 64;
/// A 24-bit timestamp field with this value means a 32-bit extended timestamp follows.
const EXTENDED: u32 = 0xFF_FFFF;
/// Slices per vectored write: a 4096-byte chunk takes two, so one write carries up to 128 kB.
const BATCH: usize = 64;

/// Message type ids (§5.4, §6.2, §7.1).
pub mod msg_type {
    /// Set Chunk Size.
    pub const SET_CHUNK_SIZE: u8 = 1;
    /// Abort Message.
    pub const ABORT: u8 = 2;
    /// Acknowledgement.
    pub const ACK: u8 = 3;
    /// User Control.
    pub const USER_CONTROL: u8 = 4;
    /// Window Acknowledgement Size.
    pub const WINDOW_ACK_SIZE: u8 = 5;
    /// Set Peer Bandwidth.
    pub const SET_PEER_BANDWIDTH: u8 = 6;
    /// Audio.
    pub const AUDIO: u8 = 8;
    /// Video.
    pub const VIDEO: u8 = 9;
    /// A data message in AMF3.
    pub const DATA_AMF3: u8 = 15;
    /// A command message in AMF3.
    pub const COMMAND_AMF3: u8 = 17;
    /// A data message in AMF0.
    pub const DATA: u8 = 18;
    /// A command message in AMF0.
    pub const COMMAND: u8 = 20;
}

/// The chunk stream ids this crate sends on. Protocol control messages and user control messages
/// go on 2, as §5.4 and §6.2 ask; the others each get their own so their headers compress.
pub mod csid {
    /// Protocol and user control messages.
    pub const CONTROL: u32 = 2;
    /// Command messages.
    pub const COMMAND: u32 = 3;
    /// Audio.
    pub const AUDIO: u32 = 4;
    /// Data messages, such as `@setDataFrame`.
    pub const DATA: u32 = 5;
    /// Video.
    pub const VIDEO: u32 = 6;
}

/// User control event types (§7.1.7).
pub mod user_control {
    /// Stream Begin.
    pub const STREAM_BEGIN: u16 = 0;
    /// Stream EOF.
    pub const STREAM_EOF: u16 = 1;
    /// StreamDry.
    pub const STREAM_DRY: u16 = 2;
    /// SetBuffer Length.
    pub const SET_BUFFER_LENGTH: u16 = 3;
    /// StreamIsRecorded.
    pub const STREAM_IS_RECORDED: u16 = 4;
    /// PingRequest.
    pub const PING_REQUEST: u16 = 6;
    /// PingResponse.
    pub const PING_RESPONSE: u16 = 7;
}

/// Where a message goes and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    /// The chunk stream id, 2 to 65 599.
    pub csid: u32,
    /// The message type id.
    pub type_id: u8,
    /// The message stream id: 0 for control and connection commands, the id `createStream`
    /// returned for a stream's commands and media.
    pub stream_id: u32,
    /// The timestamp in milliseconds.
    pub timestamp: u32,
}

/// What the last message on one chunk stream looked like.
#[derive(Debug, Clone, Copy)]
struct OutState {
    csid: u32,
    stream_id: u32,
    timestamp: u32,
    len: u32,
    type_id: u8,
    /// The delta the last header carried, if it carried one. After a type 0 header there is none,
    /// so a type 3 header never starts the next message (see [`ChunkWriter::write_message`]).
    delta: Option<u32>,
}

/// Splits messages into chunks (§5.3.1). It keeps the last header of each chunk stream so each
/// message gets the smallest header the specification allows.
#[derive(Debug, Clone)]
pub struct ChunkWriter {
    chunk_size: usize,
    streams: Vec<OutState>,
}

impl Default for ChunkWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkWriter {
    /// A writer with the default chunk size, 128 bytes.
    pub fn new() -> ChunkWriter {
        ChunkWriter { chunk_size: DEFAULT_CHUNK_SIZE, streams: Vec::new() }
    }

    /// The largest chunk payload.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Sets the largest chunk payload, from 1 to 16 777 215 bytes. Send the Set Chunk Size
    /// message with the old size first, as [`crate::Publisher`] does.
    pub fn set_chunk_size(&mut self, size: u32) -> Result<(), Error> {
        if size == 0 || size as usize > MAX_MESSAGE {
            return Err(Error::BadConfig("the chunk size must be from 1 to 16777215 bytes"));
        }
        self.chunk_size = size as usize;
        Ok(())
    }

    /// Writes one message, whose payload is the concatenation of `parts`, as chunks, and returns
    /// the bytes written.
    ///
    /// The first chunk's header is the smallest that carries what changed on its chunk stream:
    /// type 0 for the first message, a new message stream or a timestamp that goes back; type 1
    /// when the length or type changes; type 2 when only the timestamp delta changes; type 3 when
    /// nothing does. A type 3 header never starts the message right after a type 0 header, and
    /// never carries an extended timestamp for a new message, so a reader does not depend on how
    /// it reads those cases. Timestamps and deltas of 16 777 215 or more use the extended
    /// timestamp, which the continuation chunks repeat (§5.3.1.3).
    ///
    /// The headers are built on the stack and the parts are written with vectored writes of up
    /// to 64 slices, so nothing is allocated and the payload is not copied. A message longer than
    /// [`MAX_MESSAGE`] or a chunk stream id outside 2 to 65 599 is refused with
    /// `InvalidInput` before anything is written.
    pub fn write_message<W: Write + ?Sized>(&mut self, out: &mut W, header: &MessageHeader, parts: &[&[u8]]) -> io::Result<usize> {
        if !(2..=MAX_CSID).contains(&header.csid) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the chunk stream id is outside 2 to 65599"));
        }
        let len = parts.iter().fold(0usize, |n, p| n.saturating_add(p.len()));
        if len > MAX_MESSAGE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the message is longer than 16777215 bytes"));
        }
        let len = len as u32;
        let prev = self.streams.iter().position(|s| s.csid == header.csid);
        let (fmt, field) = match prev.map(|i| self.streams[i]) {
            None => (0, header.timestamp),
            Some(s) if s.stream_id != header.stream_id || header.timestamp < s.timestamp => (0, header.timestamp),
            Some(s) => {
                let delta = header.timestamp - s.timestamp;
                if s.len != len || s.type_id != header.type_id {
                    (1, delta)
                } else if s.delta != Some(delta) || delta >= EXTENDED {
                    (2, delta)
                } else {
                    (3, delta)
                }
            }
        };
        let state = OutState {
            csid: header.csid,
            stream_id: header.stream_id,
            timestamp: header.timestamp,
            len,
            type_id: header.type_id,
            delta: if fmt == 0 { None } else { Some(field) },
        };
        let ext = fmt != 3 && field >= EXTENDED;
        let field24 = if ext { EXTENDED } else { field };
        let mut first = [0u8; 18];
        let mut n = basic_header(&mut first, fmt, header.csid);
        if fmt <= 2 {
            first[n..n + 3].copy_from_slice(&field24.to_be_bytes()[1..]);
            n += 3;
        }
        if fmt <= 1 {
            first[n..n + 3].copy_from_slice(&len.to_be_bytes()[1..]);
            first[n + 3] = header.type_id;
            n += 4;
        }
        if fmt == 0 {
            first[n..n + 4].copy_from_slice(&header.stream_id.to_le_bytes());
            n += 4;
        }
        if ext {
            first[n..n + 4].copy_from_slice(&field.to_be_bytes());
            n += 4;
        }
        let mut cont = [0u8; 7];
        let mut m = basic_header(&mut cont, 3, header.csid);
        if ext {
            cont[m..m + 4].copy_from_slice(&field.to_be_bytes());
            m += 4;
        }
        let result = write_chunked(out, &first[..n], &cont[..m], self.chunk_size, parts);
        // The next message's header depends on this one only if this one was written. After a
        // failed write the chunk stream starts again with a type 0 header.
        match (prev, result.is_ok()) {
            (Some(i), true) => self.streams[i] = state,
            (None, true) => self.streams.push(state),
            (Some(i), false) => {
                self.streams.swap_remove(i);
            }
            (None, false) => {}
        }
        result
    }
}

/// Writes a chunk basic header (§5.3.1.1) in its smallest form and returns its length.
fn basic_header(out: &mut [u8], fmt: u8, csid: u32) -> usize {
    let f = fmt << 6;
    if csid < 64 {
        out[0] = f | csid as u8;
        1
    } else if csid < 320 {
        out[0] = f;
        out[1] = (csid - 64) as u8;
        2
    } else {
        let v = csid - 64;
        out[0] = f | 1;
        out[1] = (v & 0xFF) as u8;
        out[2] = (v >> 8) as u8;
        3
    }
}

/// Writes `first`, then the payload in pieces of at most `chunk` bytes with `cont` before every
/// piece after the first, and returns the bytes written.
fn write_chunked<W: Write + ?Sized>(out: &mut W, first: &[u8], cont: &[u8], chunk: usize, parts: &[&[u8]]) -> io::Result<usize> {
    let mut slices = [IoSlice::new(&[]); BATCH];
    slices[0] = IoSlice::new(first);
    let mut n = 1;
    let mut total = first.len();
    let mut room = chunk;
    for part in parts {
        let mut p: &[u8] = part;
        while !p.is_empty() {
            if room == 0 {
                if n == BATCH {
                    write_all_vectored(out, &mut slices[..n])?;
                    n = 0;
                }
                slices[n] = IoSlice::new(cont);
                n += 1;
                total += cont.len();
                room = chunk;
            }
            if n == BATCH {
                write_all_vectored(out, &mut slices[..n])?;
                n = 0;
            }
            let take = room.min(p.len());
            slices[n] = IoSlice::new(&p[..take]);
            n += 1;
            total += take;
            room -= take;
            p = &p[take..];
        }
    }
    if n > 0 {
        write_all_vectored(out, &mut slices[..n])?;
    }
    Ok(total)
}

/// Writes every slice, as `Write::write_all` does for one.
fn write_all_vectored<W: Write + ?Sized>(out: &mut W, mut bufs: &mut [IoSlice<'_>]) -> io::Result<()> {
    IoSlice::advance_slices(&mut bufs, 0);
    while !bufs.is_empty() {
        match out.write_vectored(bufs) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// One whole message read from the chunk stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Its header. `csid` is the chunk stream it arrived on.
    pub header: MessageHeader,
    /// Its payload.
    pub payload: Vec<u8>,
}

/// What the last header on one incoming chunk stream said, and the message being put together.
#[derive(Debug)]
struct InState {
    csid: u32,
    stream_id: u32,
    timestamp: u32,
    delta: u32,
    len: usize,
    type_id: u8,
    /// The last type 0, 1 or 2 header had an extended timestamp, so type 3 chunks carry one too.
    ext: bool,
    /// The payload so far of a message that has not ended.
    partial: Vec<u8>,
    in_progress: bool,
}

/// Puts incoming chunks back together into messages (§5.3).
///
/// Bytes go in with [`ChunkReader::push`] as they arrive, and [`ChunkReader::next_message`] returns
/// each message once its last chunk is in. A Set Chunk Size message changes the chunk size for the
/// chunks after it, and an Abort Message drops the message being put together on the chunk
/// stream it names, before either is returned.
///
/// Refused with [`Error::Protocol`]: a type 1, 2 or 3 chunk on a chunk stream with no type 0
/// header before it, a new message header before the last message on its chunk stream ended, a
/// message longer than the limit passed to [`ChunkReader::new`], messages in progress on several
/// chunk streams whose lengths add up to more than that limit, more than [`MAX_CHUNK_STREAMS`]
/// chunk streams, and a Set Chunk Size of 0. So the reader holds at most the limit in partial
/// messages, plus the bytes pushed in that it has not read yet. After an error the reader is out
/// of step; start again with a new one.
#[derive(Debug)]
pub struct ChunkReader {
    chunk_size: usize,
    max_message: usize,
    buf: Vec<u8>,
    pos: usize,
    streams: Vec<InState>,
    received: u64,
}

impl ChunkReader {
    /// A reader that accepts messages of up to `max_message` bytes, with the default chunk size.
    pub fn new(max_message: usize) -> ChunkReader {
        ChunkReader {
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_message: max_message.min(MAX_MESSAGE),
            buf: Vec::new(),
            pos: 0,
            streams: Vec::new(),
            received: 0,
        }
    }

    /// The chunk size the sender set.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// The bytes pushed in so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Bytes pushed in that no returned message has used yet.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Adds bytes that arrived.
    pub fn push(&mut self, data: &[u8]) {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos >= 64 * 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(data);
        self.received += data.len() as u64;
    }

    /// The next whole message, or `None` until more bytes arrive.
    pub fn next_message(&mut self) -> Result<Option<Message>, Error> {
        loop {
            match self.next_chunk()? {
                Step::NeedMore => return Ok(None),
                Step::Partial => {}
                Step::Done(m) => return Ok(Some(m)),
            }
        }
    }

    fn next_chunk(&mut self) -> Result<Step, Error> {
        let b = &self.buf[self.pos..];
        let Some(&b0) = b.first() else { return Ok(Step::NeedMore) };
        let fmt = b0 >> 6;
        let (csid, mut i) = match b0 & 0x3F {
            0 => match b.get(1) {
                Some(&v) => (64 + u32::from(v), 2),
                None => return Ok(Step::NeedMore),
            },
            1 => match (b.get(1), b.get(2)) {
                (Some(&lo), Some(&hi)) => (64 + u32::from(lo) + 256 * u32::from(hi), 3),
                _ => return Ok(Step::NeedMore),
            },
            c => (u32::from(c), 1),
        };
        let header_len = [11, 7, 3, 0][usize::from(fmt)];
        let Some(h) = b.get(i..i + header_len) else { return Ok(Step::NeedMore) };
        i += header_len;
        let idx = self.streams.iter().position(|s| s.csid == csid);
        let prev = idx.map(|n| &self.streams[n]);
        let in_progress = prev.is_some_and(|s| s.in_progress);
        if fmt != 3 && in_progress {
            return Err(Error::Protocol("a message header arrives before the last message on its chunk stream ended"));
        }
        let be24 = |s: &[u8]| u32::from(s[0]) << 16 | u32::from(s[1]) << 8 | u32::from(s[2]);
        let field = if fmt <= 2 { be24(&h[0..3]) } else { 0 };
        let ext = if fmt <= 2 { field == EXTENDED } else { prev.is_some_and(|s| s.ext) };
        let ext_value = if ext {
            let Some(e) = b.get(i..i + 4) else { return Ok(Step::NeedMore) };
            i += 4;
            u32::from_be_bytes([e[0], e[1], e[2], e[3]])
        } else {
            0
        };
        let full = if ext { ext_value } else { field };

        // The header this chunk gives its message, worked out before anything changes.
        let (timestamp, delta, len, type_id, stream_id, ext_flag) = match (fmt, prev) {
            (0, _) => (full, full, be24(&h[3..6]) as usize, h[6], u32::from_le_bytes([h[7], h[8], h[9], h[10]]), ext),
            (1, Some(s)) => (s.timestamp.wrapping_add(full), full, be24(&h[3..6]) as usize, h[6], s.stream_id, ext),
            (2, Some(s)) => (s.timestamp.wrapping_add(full), full, s.len, s.type_id, s.stream_id, ext),
            (_, Some(s)) if in_progress => (s.timestamp, s.delta, s.len, s.type_id, s.stream_id, s.ext),
            (_, Some(s)) => {
                let delta = if s.ext { ext_value } else { s.delta };
                (s.timestamp.wrapping_add(delta), delta, s.len, s.type_id, s.stream_id, s.ext)
            }
            (_, None) => return Err(Error::Protocol("a chunk continues a chunk stream that has no earlier header")),
        };
        if len > self.max_message {
            return Err(Error::Protocol("a message is longer than the reader accepts"));
        }
        if !in_progress {
            let others: usize = self.streams.iter().filter(|s| s.in_progress).map(|s| s.len).sum();
            if others + len > self.max_message {
                return Err(Error::Protocol("the messages in progress are longer than the reader accepts"));
            }
            if idx.is_none() && self.streams.len() >= MAX_CHUNK_STREAMS {
                return Err(Error::Protocol("the sender uses too many chunk streams"));
            }
        }
        let have = if in_progress { prev.map_or(0, |s| s.partial.len()) } else { 0 };
        let data_len = self.chunk_size.min(len - have);
        let Some(data) = b.get(i..i + data_len) else { return Ok(Step::NeedMore) };

        let n = match idx {
            Some(n) => n,
            None => {
                self.streams.push(InState {
                    csid,
                    stream_id,
                    timestamp,
                    delta,
                    len,
                    type_id,
                    ext: ext_flag,
                    partial: Vec::new(),
                    in_progress: false,
                });
                self.streams.len() - 1
            }
        };
        let s = &mut self.streams[n];
        if !in_progress {
            s.stream_id = stream_id;
            s.timestamp = timestamp;
            s.delta = delta;
            s.len = len;
            s.type_id = type_id;
            s.ext = ext_flag;
            s.partial.clear();
            s.partial.reserve(len);
            s.in_progress = true;
        }
        s.partial.extend_from_slice(data);
        self.pos += i + data_len;
        if s.partial.len() < s.len {
            return Ok(Step::Partial);
        }
        s.in_progress = false;
        let message = Message {
            header: MessageHeader { csid, type_id: s.type_id, stream_id: s.stream_id, timestamp: s.timestamp },
            payload: std::mem::take(&mut s.partial),
        };
        self.apply_control(&message)?;
        Ok(Step::Done(message))
    }

    /// Applies Set Chunk Size and Abort Message, which change how the next chunks are read.
    fn apply_control(&mut self, m: &Message) -> Result<(), Error> {
        let Some(v) = m.payload.get(..4).map(|p| u32::from_be_bytes([p[0], p[1], p[2], p[3]])) else { return Ok(()) };
        match m.header.type_id {
            msg_type::SET_CHUNK_SIZE => {
                let size = v & 0x7FFF_FFFF;
                if size == 0 {
                    return Err(Error::Protocol("the server set a chunk size of 0"));
                }
                self.chunk_size = (size as usize).min(MAX_MESSAGE);
            }
            msg_type::ABORT => {
                if let Some(s) = self.streams.iter_mut().find(|s| s.csid == v) {
                    s.partial.clear();
                    s.in_progress = false;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

enum Step {
    NeedMore,
    Partial,
    Done(Message),
}
