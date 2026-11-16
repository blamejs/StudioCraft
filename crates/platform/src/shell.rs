//! Opening folders in the file manager and web pages in the browser.

use std::path::Path;
use std::process::Command;

use crate::PlatformError;

/// The program that opens a folder or address with the desktop's default handler.
fn opener() -> Command {
    if cfg!(windows) {
        Command::new("explorer.exe")
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    }
}

fn spawn(mut cmd: Command) -> Result<(), PlatformError> {
    cmd.spawn().map(drop).map_err(|e| PlatformError::Failed(format!("the file manager could not be opened: {e}")))
}

/// Show `folder` in the file manager, creating it first when it does not exist.
pub fn open_folder(folder: &Path) -> Result<(), PlatformError> {
    std::fs::create_dir_all(folder).map_err(|e| PlatformError::Failed(format!("{}: {e}", folder.display())))?;
    let mut cmd = opener();
    cmd.arg(folder);
    spawn(cmd)
}

/// Open an `https://` address in the default browser.
pub fn open_url(url: &str) -> Result<(), PlatformError> {
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c == '"') {
        return Err(PlatformError::Failed(format!("`{url}` is not a web address")));
    }
    let mut cmd = opener();
    cmd.arg(url);
    spawn(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_addresses_open() {
        assert!(open_url("file:///etc/passwd").is_err());
        assert!(open_url("https://example.com/a b").is_err());
        assert!(open_url("calc.exe").is_err());
    }
}
