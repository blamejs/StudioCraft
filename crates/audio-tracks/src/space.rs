//! Free space on the disk the tracks go to, and how much of it the tracks leave alone.

use std::path::Path;

/// Bytes a second of one track takes: 48 kHz stereo 32-bit float.
pub const TRACK_BYTES_PER_SECOND: u64 = 48_000 * 8;
/// Space always left free: 1 GiB.
pub const MIN_RESERVE: u64 = 1 << 30;
/// Seconds of the active outputs' writing the reserve also holds.
pub const RESERVE_SECONDS: u64 = 120;
/// How much earlier than the reserve the status warns, in seconds of writing.
pub const WARN_SECONDS: u64 = 600;

/// Bytes free to this user on the disk holding `path`.
pub fn available(path: &Path) -> std::io::Result<u64> {
    #[cfg(any(unix, windows))]
    {
        fs4::available_space(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}

/// The free space the tracks stop at: the larger of 1 GiB and two minutes of the outputs that
/// write to the disk, at `outputs_bytes_per_second`.
pub fn reserve(outputs_bytes_per_second: u64) -> u64 {
    MIN_RESERVE.max(outputs_bytes_per_second.saturating_mul(RESERVE_SECONDS))
}

/// The free space below which the status warns: the reserve plus ten minutes of `tracks` tracks
/// and the outputs.
pub fn warning(outputs_bytes_per_second: u64, tracks: u64) -> u64 {
    let rate = outputs_bytes_per_second.saturating_add(tracks.saturating_mul(TRACK_BYTES_PER_SECOND));
    reserve(outputs_bytes_per_second).saturating_add(rate.saturating_mul(WARN_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserve_is_a_gibibyte_or_two_minutes_of_the_outputs() {
        assert_eq!(reserve(0), 1 << 30);
        assert_eq!(reserve(6_000_000 / 8), 1 << 30, "6 Mb/s for 2 minutes is 90 MB");
        assert_eq!(reserve(100_000_000 / 8), 1_500_000_000);
        assert_eq!(warning(0, 9), (1 << 30) + 9 * 384_000 * 600);
        assert!(available(&std::env::temp_dir()).is_ok_and(|b| b > 0));
    }
}
