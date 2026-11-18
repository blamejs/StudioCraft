//! The WAV files audio tracks are written to: 48 kHz stereo 32-bit float, little-endian, with the
//! header craft-audio writes for recorders ([`craft_audio::wav_float_header`]).
//!
//! The header has the same length for every frame count a track file can reach (files are closed
//! at [`MAX_SEGMENT_BYTES`], below the 4 GiB at which the layout changes to RF64), so the disk
//! thread rewrites it in place while the file grows. A file cut short by a crash still reads up to
//! the frame count of its last header; [`repair`] makes the header match the file.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use studiocraft_media::audio::{CHANNELS, SAMPLE_RATE};

/// Bytes of one stereo frame of 32-bit float samples.
pub const FRAME_BYTES: u64 = (CHANNELS * 4) as u64;
/// Largest file the writer makes: the largest segment setting, 3 GiB.
pub const MAX_SEGMENT_BYTES: u64 = 3 << 30;

/// The header of a file holding `frames` frames.
pub fn header(frames: u64) -> std::io::Result<Vec<u8>> {
    craft_audio::wav_float_header(SAMPLE_RATE, CHANNELS as u16, frames).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Bytes before the audio.
pub fn header_len() -> std::io::Result<u64> {
    Ok(header(0)?.len() as u64)
}

/// Append `samples` to `out` as little-endian 32-bit floats.
pub fn append_samples(out: &mut Vec<u8>, samples: &[f32]) {
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// Write the header of a file holding `frames` frames at the start of `file`, and go back to its
/// end.
pub fn rewrite_header(file: &mut File, frames: u64) -> std::io::Result<()> {
    let h = header(frames)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&h)?;
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

/// Make the file at `path` a valid WAV of the whole frames it holds: a partial frame at the end is
/// cut off and the header is written for the rest. The frames it holds.
pub fn repair(path: &Path) -> std::io::Result<u64> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = check_file(&mut file)?;
    let head = header_len()?;
    let frames = (len - head) / FRAME_BYTES;
    file.set_len(head + frames * FRAME_BYTES)?;
    rewrite_header(&mut file, frames)?;
    file.sync_all()?;
    Ok(frames)
}

/// Check, without changing it, that the file at `path` is one [`repair`] can repair. Its length.
pub fn check(path: &Path) -> std::io::Result<u64> {
    check_file(&mut File::open(path)?)
}

/// The length of `file`, or an error when it is shorter than a header or its header is not the one
/// the writer makes: every byte but the sizes, which depend on the frame count, must match.
fn check_file(file: &mut File) -> std::io::Result<u64> {
    let len = file.metadata()?.len();
    // Every byte of every size field differs between these two counts, and only those bytes do.
    let (empty, full) = (header(0)?, header(0x0202_0202)?);
    let mut head = vec![0u8; empty.len()];
    let ours = len >= empty.len() as u64
        && file.read_exact(&mut head).is_ok()
        && head.iter().zip(empty.iter().zip(&full)).all(|(b, (e, f))| e != f || b == e);
    if !ours {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not an audio track file"));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_has_the_same_length_for_every_frame_count_a_file_reaches() {
        let len = header_len().unwrap();
        let largest = MAX_SEGMENT_BYTES / FRAME_BYTES;
        for frames in [0u64, 1, 1023, 48_000, largest / 2, largest] {
            let h = header(frames).unwrap();
            assert_eq!(h.len() as u64, len, "{frames} frames");
            assert_eq!(&h[..4], b"RIFF");
        }
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
        // A file the writer made, with a stale frame count and a partial frame, is repaired.
        let mut ours = header(1).unwrap();
        ours.extend_from_slice(&[0u8; 3 * FRAME_BYTES as usize + 2]);
        std::fs::write(&path, &ours).unwrap();
        assert_eq!(repair(&path).unwrap(), 3);
        // Any frame count the writer can reach passes the check.
        for frames in [0u64, 1, 3, 255, 256, 65_537, 48_000 * 3600, MAX_SEGMENT_BYTES / FRAME_BYTES] {
            std::fs::write(&path, header(frames).unwrap()).unwrap();
            assert!(check(&path).is_ok(), "{frames} frames");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn samples_are_written_little_endian() {
        let mut out = Vec::new();
        append_samples(&mut out, &[1.0, -0.5]);
        assert_eq!(out, [1.0f32.to_le_bytes(), (-0.5f32).to_le_bytes()].concat());
    }
}
