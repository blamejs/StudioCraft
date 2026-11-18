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
    let len = file.metadata()?.len();
    let head = header_len()?;
    let mut magic = [0u8; 4];
    if len < head || file.read_exact(&mut magic).is_err() || &magic != b"RIFF" {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not an audio track file"));
    }
    let frames = (len - head) / FRAME_BYTES;
    file.set_len(head + frames * FRAME_BYTES)?;
    rewrite_header(&mut file, frames)?;
    file.sync_all()?;
    Ok(frames)
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
    fn samples_are_written_little_endian() {
        let mut out = Vec::new();
        append_samples(&mut out, &[1.0, -0.5]);
        assert_eq!(out, [1.0f32.to_le_bytes(), (-0.5f32).to_le_bytes()].concat());
    }
}
