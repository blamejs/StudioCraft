//! Opening a take's files without following a link in the last name of the path. On Unix a
//! symbolic link is refused. On Windows the reparse point itself is opened and never followed. A
//! reparse point whose tag has the name-surrogate bit (`0x2000_0000`) set is refused: symbolic
//! links (`IO_REPARSE_TAG_SYMLINK`), junctions (`IO_REPARSE_TAG_MOUNT_POINT`) and every other tag
//! with that bit. A reparse point whose tag lacks the bit, such as a cloud file placeholder or a
//! deduplicated file, is opened as itself and accepted when it is a file. Anything that is not a
//! file, a folder included, is refused.

use std::fs::{File, OpenOptions};
use std::path::Path;

/// Open the file at `path` with `options`, refusing a link in its last name (see the module
/// documentation for which ones) and anything that is not a file.
pub(crate) fn no_follow(options: &mut OpenOptions, path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: the link itself is opened, and refused below.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    // std reports a name-surrogate reparse point as a link, not a file.
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{} is not a file", path.display())));
    }
    Ok(file)
}

/// Which file an open file is, and how many names (hard links) it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The device and inode on Unix; the volume serial number and file index on Windows. Two
    /// names with the same `id` name the same file.
    pub(crate) id: (u64, u64),
    pub(crate) links: u64,
}

/// The [`Identity`] of `file`.
pub(crate) fn identity(file: &File) -> std::io::Result<Identity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        Ok(Identity { id: (m.dev(), m.ino()), links: m.nlink() })
    }
    #[cfg(windows)]
    {
        let i = winapi_util::file::information(file)?;
        Ok(Identity { id: (i.volume_serial_number(), i.file_index()), links: i.number_of_links() })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "file identity is not available on this system"))
    }
}

/// An error when `file`, opened from `path`, has more than one name: changing it would change the
/// file under its other names too.
pub(crate) fn one_name(file: &File, path: &Path) -> std::io::Result<Identity> {
    let id = identity(file)?;
    if id.links > 1 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{} has {} names (hard links)", path.display(), id.links)));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("studiocraft-open-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_plain_file_opens_and_a_folder_does_not() {
        let dir = scratch("plain");
        std::fs::write(dir.join("a.wav"), b"data").unwrap();
        assert!(no_follow(OpenOptions::new().read(true), &dir.join("a.wav")).is_ok());
        assert!(no_follow(OpenOptions::new().read(true), &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_has_one_identity_under_each_of_its_names() {
        let dir = scratch("identity");
        std::fs::write(dir.join("a.wav"), b"data").unwrap();
        std::fs::write(dir.join("b.wav"), b"data").unwrap();
        let open = |name: &str| no_follow(OpenOptions::new().read(true), &dir.join(name)).unwrap();
        let (a, b) = (identity(&open("a.wav")).unwrap(), identity(&open("b.wav")).unwrap());
        assert_ne!(a.id, b.id);
        assert_eq!(a.links, 1);
        assert!(one_name(&open("a.wav"), &dir.join("a.wav")).is_ok());
        std::fs::hard_link(dir.join("a.wav"), dir.join("c.wav")).unwrap();
        let c = identity(&open("c.wav")).unwrap();
        assert_eq!(c.id, a.id);
        assert_eq!(c.links, 2);
        let err = one_name(&open("a.wav"), &dir.join("a.wav")).unwrap_err();
        assert!(err.to_string().contains("2 names"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_symbolic_link_to_a_file_is_refused() {
        let dir = scratch("win-link");
        std::fs::write(dir.join("target.wav"), b"data").unwrap();
        // Making a symbolic link needs Developer Mode or the privilege to make links.
        if let Err(e) = std::os::windows::fs::symlink_file(dir.join("target.wav"), dir.join("link.wav")) {
            eprintln!("skipped: a symbolic link could not be made: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert!(no_follow(OpenOptions::new().read(true), &dir.join("link.wav")).is_err());
        assert!(no_follow(OpenOptions::new().read(true).write(true), &dir.join("link.wav")).is_err());
        assert_eq!(std::fs::read(dir.join("target.wav")).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_file_is_refused() {
        let dir = scratch("link");
        std::fs::write(dir.join("target.wav"), b"data").unwrap();
        std::os::unix::fs::symlink(dir.join("target.wav"), dir.join("link.wav")).unwrap();
        assert!(no_follow(OpenOptions::new().read(true), &dir.join("link.wav")).is_err());
        assert!(no_follow(OpenOptions::new().read(true).write(true), &dir.join("link.wav")).is_err());
        assert_eq!(std::fs::read(dir.join("target.wav")).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
