//! File names for tracks: a source's name made safe on every system, numbered when two tracks of
//! one take would share it.

use std::collections::HashSet;

/// Names Windows keeps for devices, in any letter case and with any extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
    "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Bytes of UTF-8 a track's file name keeps at most, before a number, a part number and `.wav`
/// are added: file systems allow 255 bytes in a name.
const MAX_BYTES: usize = 200;

/// `name` as a file name: path and control characters removed, spaces and dots trimmed from the
/// end, reserved device names prefixed, at most [`MAX_BYTES`] bytes of whole characters. An
/// empty result is `Source <n>`.
pub fn sanitize(name: &str, n: usize) -> String {
    let kept: String = name.chars().filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')).collect();
    let mut kept = kept.trim().to_string();
    // One byte is kept for the prefix of a reserved name.
    let mut cut = MAX_BYTES - 1;
    while cut < kept.len() && !kept.is_char_boundary(cut) {
        cut -= 1;
    }
    kept.truncate(cut);
    while kept.ends_with(['.', ' ']) {
        kept.pop();
    }
    let kept = kept.trim_start().to_string();
    if kept.is_empty() {
        return format!("Source {n}");
    }
    let stem = kept.split('.').next().unwrap_or_default().trim_end();
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return format!("_{kept}");
    }
    kept
}

/// The first of `base`, `base 2`, `base 3` … not in `used` (compared without letter case), which
/// is then added to it.
pub fn unique(base: &str, used: &mut HashSet<String>) -> String {
    let mut n = 1usize;
    loop {
        let name = if n == 1 { base.to_string() } else { format!("{base} {n}") };
        if used.insert(name.to_lowercase()) {
            return name;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe_and_kept_apart() {
        assert_eq!(sanitize("Mic / Desk: \"Left\"", 1), "Mic  Desk Left");
        assert_eq!(sanitize("..", 4), "Source 4");
        assert_eq!(sanitize("   ", 2), "Source 2");
        assert_eq!(sanitize("Game.", 1), "Game");
        assert_eq!(sanitize("Game. . ", 1), "Game");
        assert_eq!(sanitize("con", 1), "_con");
        assert_eq!(sanitize("LPT3.txt", 1), "_LPT3.txt");
        assert_eq!(sanitize("Console", 1), "Console", "a name that only starts like a device stays");
        assert_eq!(sanitize("a\u{0}b\tc", 1), "abc");
        assert_eq!(sanitize(&"x".repeat(500), 1).len(), MAX_BYTES - 1);
        let wide = sanitize(&"é🎤".repeat(200), 1);
        assert!(wide.len() < MAX_BYTES && wide.len() > MAX_BYTES - 6, "{} bytes", wide.len());
        assert!(format!("{wide} 99 part 999.wav").len() <= 255);
        assert_eq!(sanitize("Música 🎤", 1), "Música 🎤");
        let mut used = HashSet::new();
        assert_eq!(unique("Mic", &mut used), "Mic");
        assert_eq!(unique("mic", &mut used), "mic 2", "names that differ only in case would share a file on some systems");
        assert_eq!(unique("Mic", &mut used), "Mic 3");
    }
}
