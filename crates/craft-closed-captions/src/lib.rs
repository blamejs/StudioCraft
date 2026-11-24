//! Closed captions in a live H.264 or HEVC stream.
//!
//! - [`RollUp`] takes text a phrase at a time and gives CEA-608 byte pairs for channel 1 of field
//!   1 (47 CFR 15.119) in roll-up mode, one frame at a time ([`RollUp::frame`]), at the field 1
//!   rate of 30000/1001 pairs a second whatever the frame rate.
//! - [`sei::cc_data`] writes the pairs as ATSC A/53 Part 4 `cc_data`, [`sei::sei_nal`] wraps that in
//!   a `user_data_registered_itu_t_t35` SEI message (ITU-T H.264 D.1.6, H.265 D.2.6) with the
//!   `GA94` identifier, and [`sei::insert_sei`] puts the SEI NAL unit into a length-prefixed access
//!   unit before its first picture NAL unit. [`sei::read_sei`] reads the pairs back.
//!
//! # Roll-up captions
//!
//! - Each line starts with the roll-up command (2, 3 or 4 rows), a carriage return and a preamble
//!   address code for row 15, column 0, each sent twice as 608 decoders expect of control codes.
//!   A decoder that starts reading in the middle of the stream shows text from the next line on.
//! - Words wrap at [`COLUMNS`]; a longer word is broken at the edge. Characters outside the 608
//!   sets are left out, and [`RollUp::push`] counts them. An extended character goes after a
//!   basic stand-in that it replaces, for decoders without the extended sets.
//! - At most [`MAX_PENDING`] pairs (about a minute) wait to be sent. When text comes faster, the
//!   oldest lines that have not started to go out are dropped, so the stream keeps up with the
//!   speaker, and [`RollUp::dropped`] counts the pairs.
//! - After [`Config::clear_after_ms`] without new text, the captions are erased.
//!
//! The pairs of one frame go into the SEI of that frame's picture, so the frames must be given in
//! presentation order, as a stream without B-frames has them.
//!
//! No dependencies beyond `std` and `craft-captions`, no `unsafe`, builds for
//! `wasm32-unknown-unknown`.

#![warn(missing_docs)]

use std::collections::VecDeque;

use craft_captions::cea608::{self, Encoded, misc};

pub mod sei;

/// Characters on one caption row.
pub const COLUMNS: usize = 32;
/// The row the roll-up captions sit on (the bottom row).
pub const BASE_ROW: usize = 15;
/// The most pairs that wait to be sent: about a minute at the field 1 rate.
pub const MAX_PENDING: usize = 1800;
/// The most pairs one frame carries: the largest `cc_count`.
pub const MAX_PAIRS_PER_FRAME: usize = 31;
/// The pair sent where a byte is unused: a null with odd parity.
pub const NULL: u8 = 0x80;

/// Field 1 carries 30000/1001 pairs a second.
const PAIRS_NUM: u64 = 30_000;
const PAIRS_DEN: u64 = 1001;
/// Pairs at the start of each line: roll-up, carriage return and the address, each twice.
const LINE_START: usize = 6;

/// How [`RollUp`] shows the captions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Rows the captions roll up through: 2, 3 or 4.
    pub rows: u8,
    /// Milliseconds without new text after which the captions are erased; 0 keeps them up.
    pub clear_after_ms: u32,
}

impl Default for Config {
    /// Three rows, erased after 8 s without new text (StudioCraft's own values).
    fn default() -> Self {
        Config { rows: 3, clear_after_ms: 8000 }
    }
}

/// Why a [`RollUp`] could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The row count is not 2, 3 or 4.
    Rows(u8),
    /// The frame rate's numerator or denominator is 0.
    FrameRate {
        /// Frames.
        num: u32,
        /// Seconds.
        den: u32,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Rows(n) => write!(f, "roll-up captions use 2, 3 or 4 rows, not {n}"),
            Error::FrameRate { num, den } => write!(f, "the frame rate {num}/{den} is not valid"),
        }
    }
}

impl std::error::Error for Error {}

/// A control code on channel 1, with parity.
fn control(b1: u8, b2: u8) -> [u8; 2] {
    [cea608::with_parity(b1), cea608::with_parity(b2)]
}

/// CEA-608 roll-up captions, written a phrase at a time and sent a frame at a time.
#[derive(Debug)]
pub struct RollUp {
    /// The roll-up command for the row count.
    rows_code: u8,
    /// Lines waiting to be sent, each starting with its line start; the first may be partly sent.
    lines: VecDeque<Vec<[u8; 2]>>,
    /// Pairs of the first line already sent.
    sent: usize,
    /// Pairs not yet sent.
    pending: usize,
    /// A line is open: the last line takes more characters.
    open: bool,
    /// Characters on the open line.
    column: usize,
    /// A basic character waiting for the second byte of its pair.
    half: Option<u8>,
    /// Pacing: what each frame adds, what a pair costs, and what is saved up.
    gain: u64,
    cost: u64,
    credit: u64,
    /// Frames since the last text, and after how many the captions are erased (0: never).
    idle: u64,
    clear_after: u64,
    /// Text was given since the last erase.
    shown: bool,
    dropped: u64,
}

impl RollUp {
    /// Roll-up captions for a stream of `fps_num / fps_den` frames a second.
    pub fn new(fps_num: u32, fps_den: u32, cfg: Config) -> Result<RollUp, Error> {
        let rows_code = match cfg.rows {
            2 => misc::RU2,
            3 => misc::RU3,
            4 => misc::RU4,
            n => return Err(Error::Rows(n)),
        };
        if fps_num == 0 || fps_den == 0 {
            return Err(Error::FrameRate { num: fps_num, den: fps_den });
        }
        let clear_after = u64::from(cfg.clear_after_ms) * u64::from(fps_num) / (1000 * u64::from(fps_den));
        Ok(RollUp {
            rows_code,
            lines: VecDeque::new(),
            sent: 0,
            pending: 0,
            open: false,
            column: 0,
            half: None,
            gain: PAIRS_NUM * u64::from(fps_den),
            cost: PAIRS_DEN * u64::from(fps_num),
            credit: 0,
            idle: 0,
            clear_after: if cfg.clear_after_ms == 0 { 0 } else { clear_after.max(1) },
            shown: false,
            dropped: 0,
        })
    }

    /// Add a phrase. It continues the open line, wrapping at word boundaries. Returns the number
    /// of characters left out because 608 cannot show them.
    pub fn push(&mut self, text: &str) -> usize {
        let (mut skipped, mut wrote) = (0, false);
        let mut word: Vec<Encoded> = Vec::new();
        for w in text.split_whitespace() {
            word.clear();
            for c in w.chars() {
                match cea608::encode_char(c) {
                    Some(e) => word.push(e),
                    None => skipped += 1,
                }
            }
            if word.is_empty() {
                continue;
            }
            wrote = true;
            if !self.open || self.column + 1 + word.len() > COLUMNS {
                self.new_line();
            } else {
                self.basic(b' ');
                self.column += 1;
            }
            for &e in &word {
                if self.column >= COLUMNS {
                    self.new_line();
                }
                self.put(e);
                self.column += 1;
            }
        }
        self.flush_half();
        if wrote {
            self.idle = 0;
            self.shown = true;
        }
        skipped
    }

    /// The pairs for the next frame, in `out` (cleared first); none when nothing is due.
    pub fn frame(&mut self, out: &mut Vec<[u8; 2]>) {
        out.clear();
        self.credit = self.credit.saturating_add(self.gain).min(self.cost.saturating_mul(MAX_PAIRS_PER_FRAME as u64));
        if self.pending == 0 {
            self.idle = self.idle.saturating_add(1);
            if self.shown && self.clear_after > 0 && self.idle >= self.clear_after {
                self.erase();
            }
        }
        while self.credit >= self.cost {
            let Some(p) = self.next_pair() else { break };
            out.push(p);
            self.credit -= self.cost;
        }
    }

    /// Pairs waiting to be sent.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Pairs dropped because text came faster than the stream carries it.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Queue an erase of the captions shown; the next text starts a new line.
    fn erase(&mut self) {
        self.flush_half();
        self.trim();
        let edm = control(0x14, misc::EDM);
        self.lines.push_back(vec![edm, edm]);
        self.pending += 2;
        self.open = false;
        self.column = 0;
        self.shown = false;
    }

    fn next_pair(&mut self) -> Option<[u8; 2]> {
        loop {
            let last = self.lines.len() == 1;
            let front = self.lines.front_mut()?;
            if let Some(&p) = front.get(self.sent) {
                self.sent += 1;
                self.pending -= 1;
                return Some(p);
            }
            self.sent = 0;
            if last {
                // The open line goes on: keep it, empty, for the characters that follow.
                front.clear();
                return None;
            }
            self.lines.pop_front();
        }
    }

    /// Drop the oldest lines not yet started while too many pairs wait.
    fn trim(&mut self) {
        while self.pending > MAX_PENDING && self.lines.len() > 1 {
            if let Some(l) = self.lines.remove(1) {
                self.pending -= l.len();
                self.dropped += l.len() as u64;
            }
        }
    }

    fn new_line(&mut self) {
        self.flush_half();
        self.trim();
        let ru = control(0x14, self.rows_code);
        let cr = control(0x14, misc::CR);
        let (b1, b2) = cea608::encode_pac(BASE_ROW, 0);
        let pac = control(b1, b2);
        let mut line = Vec::with_capacity(LINE_START + COLUMNS);
        line.extend_from_slice(&[ru, ru, cr, cr, pac, pac]);
        self.lines.push_back(line);
        self.pending += LINE_START;
        self.open = true;
        self.column = 0;
    }

    fn push_pair(&mut self, p: [u8; 2]) {
        if let Some(line) = self.lines.back_mut() {
            line.push(p);
            self.pending += 1;
        }
    }

    fn basic(&mut self, b: u8) {
        match self.half.take() {
            Some(h) => self.push_pair([cea608::with_parity(h), cea608::with_parity(b)]),
            None => self.half = Some(b),
        }
    }

    fn flush_half(&mut self) {
        if let Some(h) = self.half.take() {
            self.push_pair([cea608::with_parity(h), NULL]);
        }
    }

    fn put(&mut self, e: Encoded) {
        match e {
            Encoded::Basic(b) => self.basic(b),
            Encoded::Pair { b1, b2, fallback } => {
                if let Some(f) = fallback {
                    self.basic(f);
                }
                self.flush_half();
                let p = control(b1, b2);
                self.push_pair(p);
                self.push_pair(p);
            }
        }
    }
}

#[cfg(test)]
mod tests;
