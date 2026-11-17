//! MPEG-2 transport stream writer (ITU-T H.222.0 | ISO/IEC 13818-1).
//!
//! [`Muxer`] writes one program to any [`Write`], 188 bytes at a time, as frames arrive:
//!
//! - a program association section and a program map section, each ending with its CRC-32, at the
//!   start, every [`Config::psi_interval`], before every video keyframe (optional) and after
//!   [`Muxer::replace_writer`];
//! - one PES packet per frame for H.264, HEVC and MPEG-2 video, AAC in ADTS frames (passed in, or
//!   built from raw AAC frames), Opus, AC-3 and MPEG audio (layers I to III), with PTS and, when
//!   it differs, DTS;
//! - the program clock reference on [`Config::pcr_pid`], in the adaptation field of the PCR
//!   stream's frames or in packets of its own, never more than [`Config::pcr_interval`] apart
//!   (at most 100 ms, §2.7.2);
//! - a continuity counter per PID, the random access indicator on keyframes, and adaptation field
//!   stuffing in each frame's last packet.
//!
//! Timestamps are 90 kHz ticks. They are written modulo 2^33, so a stream may run past the
//! wrap. The PCR follows the largest DTS so far minus [`Config::pcr_delay`].
//!
//! The writer keeps no frame data: each packet is built in a 188-byte buffer and written. After
//! an I/O error the output ends mid-stream and the muxer should be dropped.
//!
//! No dependencies beyond `std`, no `unsafe`, builds for `wasm32-unknown-unknown`.

#![warn(missing_docs)]

mod pes;
mod psi;

use std::fmt;
use std::io::{self, Write};

/// Bytes in a transport stream packet.
pub const PACKET_SIZE: usize = 188;
/// Timestamp ticks per second.
pub const TICKS_PER_SECOND: u32 = 90_000;
/// Largest PCR interval, 100 ms in 90 kHz ticks (H.222.0 §2.7.2).
pub const MAX_PCR_INTERVAL: u32 = 9_000;
/// Most elementary streams in one program.
pub const MAX_STREAMS: usize = 32;
/// Largest number of PCR-only packets, and of PAT and PMT repeats, written to fill one gap in the
/// timeline. A gap that needs more PCRs gets one PCR at its end and is counted in
/// [`Stats::pcr_gaps`].
pub const MAX_PCR_FILL: u32 = 1024;

/// Errors from writing a transport stream.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// Writing the output failed.
    Io(io::Error),
    /// The configuration or the stream list is not valid.
    Config(String),
    /// There is no stream with this index.
    NoSuchStream(usize),
    /// The frame has no bytes.
    EmptyFrame,
    /// The frame is not in the form its codec needs.
    BadFrame(&'static str),
    /// The frame does not fit in one PES packet.
    FrameTooLarge {
        /// Bytes in the frame.
        size: usize,
        /// Most bytes the frame may have.
        limit: usize,
    },
    /// The frame's timestamps are not valid.
    Timestamp(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O: {e}"),
            Error::Config(s) => write!(f, "transport stream settings: {s}"),
            Error::NoSuchStream(i) => write!(f, "no stream {i}"),
            Error::EmptyFrame => write!(f, "the frame is empty"),
            Error::BadFrame(s) => write!(f, "{s}"),
            Error::FrameTooLarge { size, limit } => write!(f, "the frame is {size} bytes; the limit is {limit}"),
            Error::Timestamp(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// What an elementary stream carries, and the form its frames take.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// H.264 access units in Annex B form. An access unit delimiter is added when a frame does
    /// not start with one.
    H264,
    /// HEVC access units in Annex B form. An access unit delimiter is added when a frame does not
    /// start with one.
    Hevc,
    /// MPEG-2 video pictures, from their first start code.
    Mpeg2Video,
    /// AAC frames that already carry their ADTS headers.
    AacAdts,
    /// Raw AAC frames; each one gets an ADTS header built from these fields.
    Aac {
        /// Audio object type, 1 to 4 (2 is AAC LC).
        object_type: u8,
        /// Sampling frequency index, 0 to 12 (3 is 48 kHz).
        sample_rate_index: u8,
        /// Channel configuration, 1 to 7.
        channels: u8,
    },
    /// Opus packets, one per frame; each gets an Opus control header.
    Opus {
        /// Channel count, 1 to 8 (mapping family 0 for 1 and 2, family 1 for 3 to 8).
        channels: u8,
    },
    /// AC-3 sync frames.
    Ac3,
    /// MPEG-1 or MPEG-2 audio frames, layers I to III (MP2, MP3).
    MpegAudio {
        /// The stream uses the MPEG-2 low sampling frequencies (16, 22.05 and 24 kHz) or the
        /// lower MPEG 2.5 rates.
        low_sample_rate: bool,
    },
}

impl Codec {
    /// The PMT stream_type.
    pub fn stream_type(self) -> u8 {
        match self {
            Codec::H264 => 0x1B,
            Codec::Hevc => 0x24,
            Codec::Mpeg2Video => 0x02,
            Codec::AacAdts | Codec::Aac { .. } => 0x0F,
            Codec::Opus { .. } => 0x06,
            Codec::Ac3 => 0x81,
            Codec::MpegAudio { low_sample_rate: false } => 0x03,
            Codec::MpegAudio { low_sample_rate: true } => 0x04,
        }
    }

    /// The PES stream_id.
    pub fn stream_id(self) -> u8 {
        match self {
            Codec::H264 | Codec::Hevc | Codec::Mpeg2Video => 0xE0,
            Codec::AacAdts | Codec::Aac { .. } | Codec::MpegAudio { .. } => 0xC0,
            Codec::Opus { .. } | Codec::Ac3 => 0xBD,
        }
    }

    /// The stream carries pictures.
    pub fn is_video(self) -> bool {
        matches!(self, Codec::H264 | Codec::Hevc | Codec::Mpeg2Video)
    }

    fn check(self) -> std::result::Result<(), String> {
        match self {
            Codec::Aac { object_type, sample_rate_index, channels } => {
                if !(1..=4).contains(&object_type) {
                    return Err(format!("AAC object type {object_type} is not 1 to 4"));
                }
                if sample_rate_index > 12 {
                    return Err(format!("AAC sampling frequency index {sample_rate_index} is not 0 to 12"));
                }
                if !(1..=7).contains(&channels) {
                    return Err(format!("AAC channel configuration {channels} is not 1 to 7"));
                }
                Ok(())
            }
            Codec::Opus { channels } if !(1..=8).contains(&channels) => Err(format!("{channels} Opus channels; 1 to 8 are supported")),
            _ => Ok(()),
        }
    }
}

/// One elementary stream of the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stream {
    /// The PID its packets carry, 0x0010 to 0x1FFE.
    pub pid: u16,
    /// What it carries.
    pub codec: Codec,
    /// ISO 639-2 language code (three lowercase letters), written as a language descriptor.
    pub language: Option<[u8; 3]>,
}

impl Stream {
    /// A stream on `pid` without a language.
    pub fn new(pid: u16, codec: Codec) -> Self {
        Self { pid, codec, language: None }
    }

    /// The same stream with a language code.
    pub fn with_language(mut self, language: [u8; 3]) -> Self {
        self.language = Some(language);
        self
    }
}

/// Program and timing settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// transport_stream_id in the program association section.
    pub transport_stream_id: u16,
    /// program_number, not 0.
    pub program_number: u16,
    /// The PID of the program map section, 0x0010 to 0x1FFE.
    pub pmt_pid: u16,
    /// The PID that carries the PCR: one of the streams' PIDs, or a PID of its own.
    /// `None` uses the first video stream, or the first stream when there is no video.
    pub pcr_pid: Option<u16>,
    /// Largest gap between PCRs, 90 kHz ticks, 1 to [`MAX_PCR_INTERVAL`].
    pub pcr_interval: u32,
    /// How far the PCR runs behind the largest DTS so far, 90 kHz ticks.
    pub pcr_delay: u32,
    /// How often the PAT and PMT are repeated, 90 kHz ticks, at least 1.
    pub psi_interval: u32,
    /// Repeat the PAT and PMT before every video keyframe, so each keyframe starts a segment that
    /// decodes on its own.
    pub psi_before_keyframes: bool,
}

impl Default for Config {
    /// Program 1 with its PMT on PID 0x1000; the PCR at most 40 ms apart and 700 ms behind the
    /// DTS; the PAT and PMT every 100 ms and before keyframes.
    fn default() -> Self {
        Self {
            transport_stream_id: 1,
            program_number: 1,
            pmt_pid: 0x1000,
            pcr_pid: None,
            pcr_interval: 3_600,
            pcr_delay: 63_000,
            psi_interval: 9_000,
            psi_before_keyframes: true,
        }
    }
}

/// One frame to write.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// The coded frame.
    pub data: &'a [u8],
    /// Presentation time, 90 kHz ticks.
    pub pts: i64,
    /// Decoding time, 90 kHz ticks; `None` when it equals `pts`. Never after `pts`, never before
    /// the stream's previous DTS, and never behind the clock (the largest DTS so far minus
    /// [`Config::pcr_delay`], or a time given to [`Muxer::advance`]).
    pub dts: Option<i64>,
    /// Decoding can start at this frame (an IDR or other random access picture; any audio frame).
    pub key: bool,
}

/// Counts of what has been written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Transport stream packets.
    pub packets: u64,
    /// Frames.
    pub frames: u64,
    /// Times the PAT and PMT were written.
    pub psi: u64,
    /// PCRs, in frames' packets and in packets of their own.
    pub pcrs: u64,
    /// Gaps in the timeline longer than [`MAX_PCR_FILL`] PCR intervals.
    pub pcr_gaps: u64,
}

impl Stats {
    /// Bytes written.
    pub fn bytes(&self) -> u64 {
        self.packets.saturating_mul(PACKET_SIZE as u64)
    }
}

struct State {
    pid: u16,
    codec: Codec,
    /// The counter the next packet with a payload carries.
    cc: u8,
    last_dts: Option<i64>,
}

impl State {
    /// The counter an adaptation-field-only packet carries: the one before the next payload
    /// packet's (§2.4.3.3), so the counter goes up by one at the next payload packet.
    fn held_cc(&self) -> u8 {
        self.cc.wrapping_sub(1) & 0x0F
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PcrOn {
    Stream(usize),
    Own(u16),
}

/// Writes one program as a transport stream.
pub struct Muxer<W: Write> {
    out: W,
    cfg: Config,
    streams: Vec<State>,
    pat: Vec<u8>,
    pmt: Vec<u8>,
    pat_cc: u8,
    pmt_cc: u8,
    pcr_on: PcrOn,
    /// The 27 MHz clock: the largest DTS so far minus the PCR delay.
    clock: Option<i64>,
    last_pcr: Option<i64>,
    last_psi: Option<i64>,
    psi_forced: bool,
    /// The next frame or advance writes a PCR whatever the interval (a new writer).
    pcr_forced: bool,
    buf: [u8; PACKET_SIZE],
    stats: Stats,
}

fn check_pid(what: &str, pid: u16) -> Result<()> {
    if (0x0010..=0x1FFE).contains(&pid) { Ok(()) } else { Err(Error::Config(format!("{what} PID 0x{pid:04X} is outside 0x0010 to 0x1FFE"))) }
}

impl<W: Write> Muxer<W> {
    /// A muxer for `streams` writing to `out`. Nothing is written until the first frame or
    /// [`Muxer::advance`].
    pub fn new(out: W, cfg: Config, streams: &[Stream]) -> Result<Self> {
        if streams.is_empty() || streams.len() > MAX_STREAMS {
            return Err(Error::Config(format!("{} streams; 1 to {MAX_STREAMS} are supported", streams.len())));
        }
        if cfg.program_number == 0 {
            return Err(Error::Config("program number 0 is reserved for the network PID".into()));
        }
        check_pid("the PMT", cfg.pmt_pid)?;
        if !(1..=MAX_PCR_INTERVAL).contains(&cfg.pcr_interval) {
            return Err(Error::Config(format!("a PCR interval of {} ticks is outside 1 to {MAX_PCR_INTERVAL}", cfg.pcr_interval)));
        }
        if cfg.psi_interval == 0 {
            return Err(Error::Config("the PAT and PMT interval must be at least 1 tick".into()));
        }
        for (i, s) in streams.iter().enumerate() {
            check_pid("a stream", s.pid)?;
            if s.pid == cfg.pmt_pid || streams[..i].iter().any(|t| t.pid == s.pid) {
                return Err(Error::Config(format!("PID 0x{:04X} is used twice", s.pid)));
            }
            s.codec.check().map_err(Error::Config)?;
            if let Some(l) = s.language
                && !l.iter().all(u8::is_ascii_lowercase)
            {
                return Err(Error::Config("a language code is three lowercase letters".into()));
            }
        }
        let pcr_on = match cfg.pcr_pid {
            Some(pid) => {
                check_pid("the PCR", pid)?;
                if pid == cfg.pmt_pid {
                    return Err(Error::Config("the PCR cannot share the PMT's PID".into()));
                }
                streams.iter().position(|s| s.pid == pid).map_or(PcrOn::Own(pid), PcrOn::Stream)
            }
            None => PcrOn::Stream(streams.iter().position(|s| s.codec.is_video()).unwrap_or(0)),
        };
        let pcr_pid = match pcr_on {
            PcrOn::Stream(i) => streams.get(i).map_or(0x1FFF, |s| s.pid),
            PcrOn::Own(pid) => pid,
        };
        Ok(Self {
            out,
            cfg,
            streams: streams.iter().map(|s| State { pid: s.pid, codec: s.codec, cc: 0, last_dts: None }).collect(),
            pat: psi::pat(cfg.transport_stream_id, cfg.program_number, cfg.pmt_pid)?,
            pmt: psi::pmt(cfg.program_number, pcr_pid, streams)?,
            pat_cc: 0,
            pmt_cc: 0,
            pcr_on,
            clock: None,
            last_pcr: None,
            last_psi: None,
            psi_forced: false,
            pcr_forced: false,
            buf: [0xFF; PACKET_SIZE],
            stats: Stats::default(),
        })
    }

    /// The PID that carries the PCR.
    pub fn pcr_pid(&self) -> u16 {
        match self.pcr_on {
            PcrOn::Stream(i) => self.streams.get(i).map_or(0x1FFF, |s| s.pid),
            PcrOn::Own(pid) => pid,
        }
    }

    /// What has been written so far.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The output.
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// The output, for flushing or for writing between packets.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Sends the following packets to `out` and returns the old output, for a new segment. The
    /// next packets start with the PAT and PMT, and the next frame (or [`Muxer::advance`]) writes a
    /// PCR; continuity counters and the clock carry on.
    pub fn replace_writer(&mut self, out: W) -> W {
        self.psi_forced = true;
        self.pcr_forced = true;
        std::mem::replace(&mut self.out, out)
    }

    /// Writes the PAT and PMT before the next frame.
    pub fn repeat_psi(&mut self) {
        self.psi_forced = true;
    }

    /// Flushes and returns the output. A transport stream has no trailer.
    pub fn finish(mut self) -> Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }

    /// The 27 MHz clock value for a DTS.
    fn time(&self, dts: i64) -> Result<i64> {
        dts.checked_sub(i64::from(self.cfg.pcr_delay)).and_then(|v| v.checked_mul(300)).ok_or(Error::Timestamp("the timestamp is out of range"))
    }

    /// Moves the clock to `time` (90 kHz) without a frame, writing the PCRs and the PAT and PMT
    /// that fall due. A live output calls this while no frames arrive, so the PCR stays within
    /// its interval. A time before the clock has no effect.
    pub fn advance(&mut self, time: i64) -> Result<()> {
        let t = self.time(time)?;
        let clock = self.clock.map_or(t, |c| c.max(t));
        self.clock = Some(clock);
        self.fill_pcr(clock)?;
        self.psi_if_due(clock, false)?;
        if self.pcr_due(clock) {
            self.pcr_packet(clock)?;
        }
        Ok(())
    }

    /// A PCR packet is due: none yet, a new writer, or the interval has passed.
    fn pcr_due(&self, clock: i64) -> bool {
        self.pcr_forced || self.last_pcr.is_none_or(|l| clock.saturating_sub(l) >= self.limit())
    }

    /// Writes one frame of stream `index` (an index into the list given to [`Muxer::new`]).
    /// The frame is checked before anything is written.
    pub fn write_frame(&mut self, index: usize, frame: &Frame<'_>) -> Result<()> {
        let st = self.streams.get(index).ok_or(Error::NoSuchStream(index))?;
        if frame.data.is_empty() {
            return Err(Error::EmptyFrame);
        }
        let dts = frame.dts.unwrap_or(frame.pts);
        if dts > frame.pts {
            return Err(Error::Timestamp("the decoding time is after the presentation time"));
        }
        if st.last_dts.is_some_and(|l| dts < l) {
            return Err(Error::Timestamp("the decoding time is before the stream's previous one"));
        }
        let codec = st.codec;
        let mut prefix = [0u8; pes::PREFIX_MAX];
        let prefix_len = pes::prefix(codec, frame.data, &mut prefix)?;
        let with_dts = dts != frame.pts;
        let header_len = if with_dts { 19 } else { 14 };
        // bytes after PES_packet_length
        let body = (header_len - 6) + prefix_len + frame.data.len();
        let length = match u16::try_from(body) {
            Ok(n) => n,
            Err(_) if codec.is_video() => 0,
            Err(_) => return Err(Error::FrameTooLarge { size: frame.data.len(), limit: 0xFFFF - (header_len - 6) - prefix_len }),
        };
        let t = self.time(dts)?;
        // the frame must arrive no later than it decodes: its DTS may not be behind the clock
        // that PCRs already sent (the largest DTS so far minus the PCR delay, or an advance)
        if dts.checked_mul(300).is_none_or(|d| self.clock.is_some_and(|c| d < c)) {
            return Err(Error::Timestamp("the decoding time is behind the transport stream clock"));
        }
        let mut header = [0u8; pes::HEADER_MAX];
        pes::header(&mut header, codec.stream_id(), length, frame.pts, with_dts.then_some(dts));

        let clock = self.clock.map_or(t, |c| c.max(t));
        self.clock = Some(clock);
        self.fill_pcr(clock)?;
        self.psi_if_due(clock, frame.key && codec.is_video())?;
        let mut pcr = None;
        if self.pcr_on == PcrOn::Stream(index) {
            if self.pcr_forced || self.last_pcr.is_none_or(|l| clock > l) {
                pcr = Some(clock);
                self.last_pcr = Some(clock);
                self.pcr_forced = false;
                self.stats.pcrs += 1;
            }
        } else if self.pcr_due(clock) {
            self.pcr_packet(clock)?;
        }
        self.pes_packets(index, [&header[..header_len], &prefix[..prefix_len], frame.data], pcr, frame.key)?;
        if let Some(st) = self.streams.get_mut(index) {
            st.last_dts = Some(dts);
        }
        self.stats.frames += 1;
        Ok(())
    }

    /// The PCR interval at 27 MHz.
    fn limit(&self) -> i64 {
        i64::from(self.cfg.pcr_interval) * 300
    }

    fn psi_if_due(&mut self, clock: i64, keyframe: bool) -> Result<()> {
        let due = self.psi_forced
            || self.last_psi.is_none_or(|l| clock.saturating_sub(l) >= i64::from(self.cfg.psi_interval) * 300)
            || (keyframe && self.cfg.psi_before_keyframes);
        if due {
            section_packets(&mut self.out, &mut self.buf, 0, &mut self.pat_cc, &self.pat, &mut self.stats)?;
            section_packets(&mut self.out, &mut self.buf, self.cfg.pmt_pid, &mut self.pmt_cc, &self.pmt, &mut self.stats)?;
            self.last_psi = Some(clock);
            self.psi_forced = false;
            self.stats.psi += 1;
        }
        Ok(())
    }

    /// Fills the gap up to `clock`: PCR-only packets until the gap to `clock` is within the PCR
    /// interval, and the PAT and PMT wherever they fall due before `clock`, in time order. A new
    /// writer gets the PAT and PMT before anything else.
    fn fill_pcr(&mut self, clock: i64) -> Result<()> {
        let Some(last_pcr) = self.last_pcr else { return Ok(()) };
        if self.psi_forced {
            self.psi_if_due(last_pcr, false)?;
        }
        let limit = self.limit();
        let psi = i64::from(self.cfg.psi_interval) * 300;
        // each kind of packet is capped on its own, so a short PAT and PMT interval cannot use
        // up the PCRs' share
        let (mut pcrs, mut tables) = (0, 0);
        loop {
            let mut pcr_at = self.last_pcr.map(|l| l.saturating_add(limit)).filter(|&t| t < clock);
            if pcrs >= MAX_PCR_FILL && pcr_at.is_some() {
                if pcrs == MAX_PCR_FILL {
                    // counted once: the PCR at `clock` that follows closes the gap
                    self.stats.pcr_gaps += 1;
                    pcrs += 1;
                }
                pcr_at = None;
            }
            let psi_at = self.last_psi.map(|l| l.saturating_add(psi)).filter(|&t| t < clock && tables < MAX_PCR_FILL);
            match (psi_at, pcr_at) {
                (Some(s), Some(p)) if s <= p => {
                    self.psi_if_due(s, false)?;
                    tables += 1;
                }
                (Some(s), None) => {
                    self.psi_if_due(s, false)?;
                    tables += 1;
                }
                (_, Some(p)) => {
                    self.pcr_packet(p)?;
                    pcrs += 1;
                }
                (None, None) => return Ok(()),
            }
        }
    }

    /// A packet on the PCR PID with only an adaptation field holding the PCR.
    fn pcr_packet(&mut self, pcr: i64) -> Result<()> {
        let (pid, cc) = match self.pcr_on {
            PcrOn::Stream(i) => self.streams.get(i).map_or((0x1FFF, 0), |s| (s.pid, s.held_cc())),
            PcrOn::Own(pid) => (pid, 0),
        };
        let b = &mut self.buf;
        b[..6].copy_from_slice(&[0x47, (pid >> 8) as u8 & 0x1F, pid as u8, 0x20 | cc, 183, 0x10]);
        b[6..12].copy_from_slice(&pes::pcr(pcr));
        b[12..].fill(0xFF);
        self.out.write_all(b)?;
        self.stats.packets += 1;
        self.stats.pcrs += 1;
        self.last_pcr = Some(pcr);
        self.pcr_forced = false;
        Ok(())
    }

    /// Splits one PES packet (`parts` in order) into transport packets.
    fn pes_packets(&mut self, index: usize, mut parts: [&[u8]; 3], pcr: Option<i64>, key: bool) -> Result<()> {
        let Some(st) = self.streams.get_mut(index) else { return Err(Error::NoSuchStream(index)) };
        let pid = st.pid;
        let mut left: usize = parts.iter().map(|p| p.len()).sum();
        let mut first = true;
        while left > 0 {
            let pcr_here = if first { pcr } else { None };
            let rai = first && key;
            let fixed = if pcr_here.is_some() || rai { 2 + if pcr_here.is_some() { 6 } else { 0 } } else { 0 };
            let take = left.min(184 - fixed);
            // the adaptation field, its length byte included
            let af = 184 - take;
            let b = &mut self.buf;
            b[0] = 0x47;
            b[1] = if first { 0x40 } else { 0 } | (pid >> 8) as u8 & 0x1F;
            b[2] = pid as u8;
            b[3] = if af > 0 { 0x30 } else { 0x10 } | st.cc;
            st.cc = (st.cc + 1) & 0x0F;
            if af > 0 {
                b[4] = (af - 1) as u8;
                if af > 1 {
                    b[5] = if rai { 0x40 } else { 0 } | if pcr_here.is_some() { 0x10 } else { 0 };
                    let mut p = 6;
                    if let Some(v) = pcr_here {
                        b[6..12].copy_from_slice(&pes::pcr(v));
                        p = 12;
                    }
                    b[p..4 + af].fill(0xFF);
                }
            }
            let mut at = 4 + af;
            for part in &mut parts {
                let n = part.len().min(PACKET_SIZE - at);
                b[at..at + n].copy_from_slice(&part[..n]);
                *part = &part[n..];
                at += n;
            }
            self.out.write_all(b)?;
            self.stats.packets += 1;
            left -= take;
            first = false;
        }
        Ok(())
    }
}

/// Writes a PSI section on `pid`: a pointer field of 0, the section, and 0xFF stuffing.
fn section_packets<W: Write>(out: &mut W, b: &mut [u8; PACKET_SIZE], pid: u16, cc: &mut u8, section: &[u8], stats: &mut Stats) -> Result<()> {
    let mut rest = section;
    let mut first = true;
    while first || !rest.is_empty() {
        b[0] = 0x47;
        b[1] = if first { 0x40 } else { 0 } | (pid >> 8) as u8 & 0x1F;
        b[2] = pid as u8;
        b[3] = 0x10 | *cc;
        *cc = (*cc + 1) & 0x0F;
        let mut at = 4;
        if first {
            b[4] = 0;
            at = 5;
        }
        let n = rest.len().min(PACKET_SIZE - at);
        b[at..at + n].copy_from_slice(&rest[..n]);
        rest = &rest[n..];
        b[at + n..].fill(0xFF);
        out.write_all(b)?;
        stats.packets += 1;
        first = false;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
