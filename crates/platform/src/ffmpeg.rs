//! Finding ffmpeg.
//!
//! StudioCraft runs ffmpeg as a separate program for encoding and decoding. The search order is:
//! the path set in Settings › Output, the `STUDIOCRAFT_FFMPEG` environment variable, an `ffmpeg`
//! folder next to the StudioCraft program, then every folder on `PATH`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::PlatformError;

/// The environment variable that names the ffmpeg program.
pub const ENV: &str = "STUDIOCRAFT_FFMPEG";

fn exe_name() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

/// The ffmpeg program to run. `configured` is the path from the settings (empty when unset).
pub fn locate(configured: &str) -> Result<PathBuf, PlatformError> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return if p.is_file() { Ok(p) } else { Err(PlatformError::NotFound(format!("ffmpeg at {}", p.display()))) };
    }
    if let Some(p) = std::env::var_os(ENV).map(PathBuf::from).filter(|p| p.is_file()) {
        return Ok(p);
    }
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("ffmpeg").join(exe_name())));
    if let Some(p) = beside.filter(|p| p.is_file()) {
        return Ok(p);
    }
    on_path(exe_name()).ok_or_else(|| PlatformError::NotFound("ffmpeg (set its location in Settings › Output)".into()))
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// A command running `path` without opening a console window.
pub fn command(path: &Path) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// The first line of `ffmpeg -version`, to show in the settings.
pub fn version(path: &Path) -> Option<String> {
    let out = command(path).arg("-hide_banner").arg("-version").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

/// The names of the video encoders this ffmpeg was built with (from `ffmpeg -encoders`).
pub fn encoders(path: &Path) -> Vec<String> {
    let Ok(out) = command(path).args(["-hide_banner", "-encoders"]).stdin(Stdio::null()).stderr(Stdio::null()).output() else {
        return Vec::new();
    };
    parse_encoders(&String::from_utf8_lossy(&out.stdout))
}

/// Video encoder names from `ffmpeg -encoders` output: lines whose flags start with `V`.
fn parse_encoders(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let flags = parts.next()?;
            let name = parts.next()?;
            (flags.len() == 6 && flags.starts_with('V') && name != "=").then(|| name.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_path_that_does_not_exist_is_an_error() {
        let e = locate("Z:/no/such/ffmpeg.exe").unwrap_err();
        assert!(matches!(e, PlatformError::NotFound(_)));
    }

    #[test]
    fn encoder_lists_are_parsed() {
        let text = " V..... = Video\n ------\n V....D libx264              libx264 H.264\n A....D aac                  AAC\n V....D h264_nvenc           NVIDIA NVENC\n";
        assert_eq!(parse_encoders(text), vec!["libx264", "h264_nvenc"]);
    }

    #[test]
    fn finds_an_installed_ffmpeg() {
        // Machines without ffmpeg pass without checking.
        if let Ok(p) = locate("") {
            let v = version(&p).unwrap_or_default();
            assert!(v.starts_with("ffmpeg version"), "{v}");
        }
    }
}
