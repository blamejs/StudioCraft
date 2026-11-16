//! Scene collection files at a path the user picks: importing one into the studio and exporting
//! the current one. The file holds the same versioned JSON as a collection in the settings folder
//! (`studiocraft_model::format`), and reading it runs the same migrations.

use std::io::Read;
use std::path::{Path, PathBuf};

use studiocraft_model::SceneCollection;
use studiocraft_model::format::{read_collection, write_collection};

/// The largest scene collection file that is read, in bytes.
pub const MAX_COLLECTION_BYTES: u64 = 16 << 20;

/// The name given to a scene when a file's collection has none.
const DEFAULT_SCENE: &str = "Scene";

/// Why a scene collection file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("the file is {bytes} bytes; a scene collection file can be at most {max} bytes")]
    TooLarge { bytes: u64, max: u64 },
    #[error("{}: {source}", .path.display())]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{0}")]
    Format(String),
}

impl FileError {
    fn io(path: &Path, source: std::io::Error) -> FileError {
        FileError::Io { path: path.to_path_buf(), source }
    }
}

/// The scene collection in the file at `path`, migrated to the current version. The file's size
/// is checked before anything is read.
pub fn read_collection_file(path: &Path) -> Result<SceneCollection, FileError> {
    let file = std::fs::File::open(path).map_err(|e| FileError::io(path, e))?;
    let bytes = file.metadata().map_err(|e| FileError::io(path, e))?.len();
    if bytes > MAX_COLLECTION_BYTES {
        return Err(FileError::TooLarge { bytes, max: MAX_COLLECTION_BYTES });
    }
    let mut data = Vec::new();
    // The file can grow between the size check and the read; read at most one byte past the cap.
    file.take(MAX_COLLECTION_BYTES.saturating_add(1)).read_to_end(&mut data).map_err(|e| FileError::io(path, e))?;
    let read = u64::try_from(data.len()).unwrap_or(u64::MAX);
    if read > MAX_COLLECTION_BYTES {
        return Err(FileError::TooLarge { bytes: read, max: MAX_COLLECTION_BYTES });
    }
    let text = String::from_utf8(data).map_err(|_| FileError::Format("the file is not UTF-8 text".into()))?;
    read_collection(&text, DEFAULT_SCENE).map_err(|e| FileError::Format(e.to_string()))
}

/// Write `collection` to `path`. The file is replaced in one step, so a failed write leaves any
/// earlier file at `path` as it was.
pub fn write_collection_file(path: &Path, collection: &SceneCollection) -> Result<(), FileError> {
    let text = write_collection(collection).map_err(|e| FileError::Format(e.to_string()))?;
    // A file larger than the reader accepts could not be opened again.
    let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    if bytes > MAX_COLLECTION_BYTES {
        return Err(FileError::TooLarge { bytes, max: MAX_COLLECTION_BYTES });
    }
    craft_atomic::atomic_write(path, text.as_bytes()).map_err(|e| FileError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use studiocraft_model::format::COLLECTION;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("studiocraft-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_collection_round_trips_through_a_file() {
        let dir = temp_dir("round");
        let path = dir.join("show.json");
        let mut c = SceneCollection::new("Show", "Intro");
        c.scenes[0].name = "Intro".into();
        write_collection_file(&path, &c).unwrap();
        assert_eq!(read_collection_file(&path).unwrap(), c);
        // A second write replaces the first.
        let other = SceneCollection::new("Other", "Scene");
        write_collection_file(&path, &other).unwrap();
        assert_eq!(read_collection_file(&path).unwrap().name, "Other");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_over_the_cap_is_refused_before_it_is_read() {
        let dir = temp_dir("large");
        let path = dir.join("huge.json");
        // A sparse file far over the cap: the error reports its full size, which only the size
        // check before the read knows.
        let size = 1u64 << 30;
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(size).unwrap();
        drop(f);
        match read_collection_file(&path) {
            Err(FileError::TooLarge { bytes, max }) => {
                assert_eq!(bytes, size);
                assert_eq!(max, MAX_COLLECTION_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_collection_too_large_to_read_back_is_not_written() {
        let dir = temp_dir("large-write");
        let path = dir.join("big.json");
        let mut c = SceneCollection::new("Big", "Scene");
        c.scenes[0].name = "x".repeat(usize::try_from(MAX_COLLECTION_BYTES).unwrap());
        let r = write_collection_file(&path, &c);
        let written = path.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(r, Err(FileError::TooLarge { .. })), "{r:?}");
        assert!(!written);
    }

    #[test]
    fn bad_contents_are_format_errors() {
        let dir = temp_dir("bad");
        let cases = [
            ("broken.json", "{ not json".to_string()),
            ("newer.json", format!(r#"{{"format":"{}","version":{},"data":{{}}}}"#, COLLECTION.name, COLLECTION.version + 1)),
            ("other.json", r#"{"format":"something.else","version":1,"data":{}}"#.to_string()),
        ];
        for (name, text) in cases {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            assert!(matches!(read_collection_file(&path), Err(FileError::Format(_))), "{name}");
        }
        let path = dir.join("binary.json");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(read_collection_file(&path), Err(FileError::Format(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_io_error_naming_the_path() {
        let dir = temp_dir("missing");
        let path = dir.join("absent.json");
        let e = read_collection_file(&path).unwrap_err();
        assert!(matches!(e, FileError::Io { .. }));
        assert!(e.to_string().contains("absent.json"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
