//! File names for tracks: a source's name made safe on every system, numbered when two tracks of
//! one take would share it.

use std::collections::HashSet;

/// Names Windows keeps for devices, in any letter case and with any extension
/// (<https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>).
const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];

/// Whether `stem` is a Windows device name: one of [`RESERVED`], or `COM` or `LPT` followed by
/// one digit from 0 to 9 or a superscript 1, 2 or 3.
fn reserved(stem: &str) -> bool {
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return true;
    }
    let mut chars = stem.chars();
    let head: String = chars.by_ref().take(3).collect();
    if !(head.eq_ignore_ascii_case("COM") || head.eq_ignore_ascii_case("LPT")) {
        return false;
    }
    matches!((chars.next(), chars.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
}

/// Bytes of UTF-8 a track's file name keeps at most, before a number, a part number and `.wav`
/// are added: file systems allow 255 bytes in a name.
const MAX_BYTES: usize = 200;

/// Whether `c` is left out of a track's file name: a control character, a character Windows does
/// not allow in a name, or `~`, which Windows' short names use.
fn removed(c: char) -> bool {
    c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '~')
}

/// `name` as a file name: path and control characters and `~` removed, spaces and dots trimmed
/// from the end, reserved device names prefixed, at most [`MAX_BYTES`] bytes of whole characters.
/// An empty result is `Source <n>`.
pub fn sanitize(name: &str, n: usize) -> String {
    let kept: String = name.chars().filter(|c| !removed(*c)).collect();
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
    if reserved(stem) {
        return format!("_{kept}");
    }
    kept
}

/// Whether `file` has the form of a track file's name the writer makes: a name [`sanitize`] keeps
/// as it is, possibly followed by a number and a part number, then `.wav`, in at most 255 bytes.
/// A name with a character [`sanitize`] removes, one that starts with a space, ends in a dot or a
/// space before `.wav`, or names a Windows device is not.
pub fn is_track_file(file: &str) -> bool {
    let Some(stem) = file.strip_suffix(".wav") else { return false };
    let device = stem.split('.').next().is_some_and(|s| reserved(s.trim_end()));
    file.len() <= 255 && !stem.is_empty() && !stem.chars().any(removed) && !stem.starts_with(' ') && !stem.ends_with(['.', ' ']) && !device
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
        assert_eq!(sanitize("COM¹", 1), "_COM¹");
        assert_eq!(sanitize("lpt³.wav", 1), "_lpt³.wav");
        assert_eq!(sanitize("COM0", 1), "_COM0");
        assert_eq!(sanitize("LPT0", 1), "_LPT0");
        assert_eq!(sanitize("COM⁴", 1), "COM⁴", "only superscript 1, 2 and 3 are device names");
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
        assert_eq!(sanitize("Mic~1", 1), "Mic1");
    }

    #[test]
    fn only_names_the_writer_makes_are_track_files() {
        for ok in ["Mic.wav", "Mic 2.wav", "Mic part 2.wav", "_con.wav", "Música 🎤.wav", "Game.v2.wav"] {
            assert!(is_track_file(ok), "{ok}");
        }
        for bad in [
            "notes.txt",
            "Mic.WAV",
            ".wav",
            "Mic..wav",
            "Mic .wav",
            " Mic.wav",
            "Mic.wav.",
            "Mic.wav ",
            "MICPAR~1.wav",
            "a:b.wav",
            "con.wav",
            "../Mic.wav",
            "a\tb.wav",
        ] {
            assert!(!is_track_file(bad), "{bad}");
        }
        assert!(!is_track_file(&format!("{}.wav", "x".repeat(252))));
        let mut used = HashSet::new();
        for name in ["Mic", "con", "Game.", "  Desk  ", "~x~"] {
            let file = unique(&sanitize(name, 1), &mut used) + ".wav";
            assert!(is_track_file(&file), "{file}");
        }
    }
}
