//! The WAV files audio tracks are written to: 48 kHz stereo 32-bit float, little-endian, with the
//! header craft-audio writes for recorders ([`craft_audio::wav_float_header`]) and a `bext` chunk
//! (EBU Tech 3285, <https://tech.ebu.ch/publications/tech3285>) whose time reference is the
//! sample of the recording's or stream's timeline the file's first sample is at.
//!
//! The header has the same length for every frame count a track file can reach (files are closed
//! at [`MAX_SEGMENT_BYTES`], below the 4 GiB at which the layout changes to RF64), so the disk
//! thread rewrites it in place while the file grows. A file cut short by a crash still reads up to
//! the frame count of its last header; [`repair`] makes the header match the file and keeps its
//! `bext` chunk.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use studiocraft_media::audio::{CHANNELS, SAMPLE_RATE};

/// Bytes of one stereo frame of 32-bit float samples.
pub const FRAME_BYTES: u64 = (CHANNELS * 4) as u64;
/// Largest file the writer makes: the largest segment setting, 3 GiB.
pub const MAX_SEGMENT_BYTES: u64 = 3 << 30;
/// Bytes of the `bext` chunk's body: the version 1 fields with no coding history.
const BEXT_LEN: usize = 602;
/// Where craft-audio's header ends its `JUNK` chunk, after which the `bext` chunk goes.
const BEXT_AT: usize = 12 + 8 + 28;

/// What a file's `bext` chunk records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    /// The sample of the recording's or stream's timeline the file's first sample is at, at
    /// 48 kHz: 0 for a file that starts with the recording.
    pub time_reference: u64,
    /// The local date the take started, `yyyy-mm-dd`.
    pub date: String,
    /// The local time the take started, `hh:mm:ss`.
    pub time: String,
}

/// A stamp in which every byte the stamp sets differs from the default stamp's.
fn other_stamp() -> Stamp {
    Stamp { time_reference: 0x0202_0202_0202_0202, date: "\u{2}".repeat(10), time: "\u{2}".repeat(8) }
}

/// `s` in a field of `len` bytes, cut to fit, the rest zero.
fn put_fixed(out: &mut Vec<u8>, s: &str, len: usize) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(len);
    out.extend_from_slice(bytes.get(..n).unwrap_or_default());
    out.resize(out.len() + (len - n), 0);
}

/// The header of a file holding `frames` frames, stamped with `stamp`.
pub fn header(frames: u64, stamp: &Stamp) -> std::io::Result<Vec<u8>> {
    let plain = craft_audio::wav_float_header(SAMPLE_RATE, CHANNELS as u16, frames).map_err(|e| std::io::Error::other(e.to_string()))?;
    if plain.get(BEXT_AT - 36..BEXT_AT - 32) != Some(b"JUNK".as_slice()) {
        return Err(std::io::Error::other("the WAV header has an unexpected layout"));
    }
    let mut out = Vec::with_capacity(plain.len() + 8 + BEXT_LEN);
    out.extend_from_slice(plain.get(..BEXT_AT).unwrap_or_default());
    out.extend_from_slice(b"bext");
    out.extend_from_slice(&(BEXT_LEN as u32).to_le_bytes());
    let body = out.len();
    // Description, originator and originator reference stay empty.
    out.resize(body + 256 + 32 + 32, 0);
    put_fixed(&mut out, &stamp.date, 10);
    put_fixed(&mut out, &stamp.time, 8);
    out.extend_from_slice(&stamp.time_reference.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.resize(body + BEXT_LEN, 0);
    out.extend_from_slice(plain.get(BEXT_AT..).unwrap_or_default());
    // The RIFF size counts the new chunk.
    if let Some(size) = out.get_mut(4..8) {
        let riff = u32::from_le_bytes([size[0], size[1], size[2], size[3]]).saturating_add(8 + BEXT_LEN as u32);
        size.copy_from_slice(&riff.to_le_bytes());
    }
    Ok(out)
}

/// Bytes before the audio.
pub fn header_len() -> std::io::Result<u64> {
    Ok(header(0, &Stamp::default())?.len() as u64)
}

/// The time reference in a header made by [`header`].
pub fn time_reference(head: &[u8]) -> Option<u64> {
    let at = BEXT_AT + 8 + 256 + 32 + 32 + 10 + 8;
    head.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes)
}

/// Append `samples` to `out` as little-endian 32-bit floats.
pub fn append_samples(out: &mut Vec<u8>, samples: &[f32]) {
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// Write the header of a file holding `frames` frames, stamped with `stamp`, at the start of
/// `file`, and go back to its end.
pub fn rewrite_header(file: &mut File, frames: u64, stamp: &Stamp) -> std::io::Result<()> {
    let h = header(frames, stamp)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&h)?;
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

/// Make the file at `path` a valid WAV of the whole frames it holds: a partial frame at the end is
/// cut off and the header is written for the rest, with the file's own `bext` stamp. The frames
/// it holds.
pub fn repair(path: &Path) -> std::io::Result<u64> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let (len, old) = check_file(&mut file)?;
    let head = header_len()?;
    let frames = (len - head) / FRAME_BYTES;
    file.set_len(head + frames * FRAME_BYTES)?;
    // The stamp's bytes come from the file; the sizes from the frames it holds.
    let (a, b) = (header(frames, &Stamp::default())?, header(frames, &other_stamp())?);
    let new: Vec<u8> = a.iter().zip(&b).zip(&old).map(|((a, b), o)| if a == b { *a } else { *o }).collect();
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&new)?;
    file.sync_all()?;
    Ok(frames)
}

/// Shorten the file at `path`, checked by [`check`], to at most `frames` frames after its header.
/// [`repair`] then writes the header for what is left.
pub fn cut(path: &Path, frames: u64) -> std::io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    // `frames` comes from the journal: a length past any file leaves the file as it is.
    let keep = header_len()?.saturating_add(frames.saturating_mul(FRAME_BYTES));
    if file.metadata()?.len() > keep {
        file.set_len(keep)?;
    }
    Ok(())
}

/// Check, without changing it, that the file at `path` is one [`repair`] can repair. Its length.
pub fn check(path: &Path) -> std::io::Result<u64> {
    check_file(&mut File::open(path)?).map(|(len, _)| len)
}

/// The length and header of `file`, or an error when it is shorter than a header or its header is
/// not the one the writer makes: every byte but the sizes, which depend on the frame count, and
/// the stamp must match.
fn check_file(file: &mut File) -> std::io::Result<(u64, Vec<u8>)> {
    let len = file.metadata()?.len();
    // Every byte of every size field differs between these two counts, and every byte of the
    // stamp between these two stamps; only those bytes do.
    let (empty, full) = (header(0, &Stamp::default())?, header(0x0202_0202, &other_stamp())?);
    let mut head = vec![0u8; empty.len()];
    let ours = len >= empty.len() as u64
        && file.read_exact(&mut head).is_ok()
        && head.iter().zip(empty.iter().zip(&full)).all(|(b, (e, f))| e != f || b == e);
    if !ours {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not an audio track file"));
    }
    Ok((len, head))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> Stamp {
        Stamp { time_reference: 123_456_789, date: "2026-11-18".into(), time: "21:04:05".into() }
    }

    #[test]
    fn the_header_has_the_same_length_for_every_frame_count_a_file_reaches() {
        let len = header_len().unwrap();
        let largest = MAX_SEGMENT_BYTES / FRAME_BYTES;
        for frames in [0u64, 1, 1023, 48_000, largest / 2, largest] {
            let h = header(frames, &stamp()).unwrap();
            assert_eq!(h.len() as u64, len, "{frames} frames");
            assert_eq!(&h[..4], b"RIFF");
        }
    }

    #[test]
    fn the_time_reference_reads_back_as_a_broadcast_wave_time_reference() {
        let mut bytes = header(3, &stamp()).unwrap();
        assert_eq!(time_reference(&bytes), Some(123_456_789));
        bytes.extend_from_slice(&[0u8; 3 * FRAME_BYTES as usize]);
        let info = craft_audio::probe(&bytes, Some("wav")).unwrap();
        let bwf = info.bwf.unwrap();
        assert_eq!((bwf.time_reference, bwf.origination_date.as_str(), bwf.origination_time.as_str()), (123_456_789, "2026-11-18", "21:04:05"));
        let (_, buf) = craft_audio::decode(&bytes, Some("wav")).unwrap();
        assert_eq!(buf.frames(), 3);
    }

    #[test]
    fn repair_leaves_a_file_of_another_riff_kind_alone() {
        let path = std::env::temp_dir().join(format!("studiocraft-wav-riff-{}.wav", std::process::id()));
        let mut bytes = b"RIFF\0\0\0\0AVI LIST".to_vec();
        bytes.resize(header_len().unwrap() as usize + 100, 7);
        std::fs::write(&path, &bytes).unwrap();
        let err = repair(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "the file is unchanged");
        // A file the writer made, with a stale frame count and a partial frame, is repaired and
        // keeps its stamp.
        let mut ours = header(1, &stamp()).unwrap();
        ours.extend_from_slice(&[0u8; 3 * FRAME_BYTES as usize + 2]);
        std::fs::write(&path, &ours).unwrap();
        assert_eq!(repair(&path).unwrap(), 3);
        assert_eq!(std::fs::read(&path).unwrap()[..header_len().unwrap() as usize], header(3, &stamp()).unwrap()[..]);
        // Any frame count the writer can reach passes the check, with any stamp.
        for frames in [0u64, 1, 3, 255, 256, 65_537, 48_000 * 3600, MAX_SEGMENT_BYTES / FRAME_BYTES] {
            for s in [Stamp::default(), stamp()] {
                std::fs::write(&path, header(frames, &s).unwrap()).unwrap();
                assert!(check(&path).is_ok(), "{frames} frames");
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_cut_shortens_a_file_and_a_cut_past_its_end_leaves_it_alone() {
        let path = std::env::temp_dir().join(format!("studiocraft-wav-cut-{}.wav", std::process::id()));
        let mut bytes = header(5, &stamp()).unwrap();
        bytes.extend_from_slice(&[1u8; 5 * FRAME_BYTES as usize]);
        std::fs::write(&path, &bytes).unwrap();
        for frames in [u64::MAX, u64::MAX / FRAME_BYTES + 1, 6] {
            cut(&path, frames).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{frames} frames");
        }
        cut(&path, 2).unwrap();
        assert_eq!(repair(&path).unwrap(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn samples_are_written_little_endian() {
        let mut out = Vec::new();
        append_samples(&mut out, &[1.0, -0.5]);
        assert_eq!(out, [1.0f32.to_le_bytes(), (-0.5f32).to_le_bytes()].concat());
    }
}
