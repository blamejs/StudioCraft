//! The face tracking model catalog and the model store.
//!
//! A model is kept in `<models dir>/<id>/<file name>`. It reaches that folder only when the user
//! asks: an app downloads it from its pinned URL, or [`install`] copies a file the user already
//! has. A model is never bundled with an app or committed to a repository. Every file is checked
//! against its pinned size and SHA-256 before it is put in place.
//!
//! License: the Face Landmarker bundle is published under the Apache-2.0 license (its model card,
//! [`ModelInfo::license_url`]).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::Digest;

use crate::{Error, Result};

/// A face tracking model: one bundle file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    pub author: &'static str,
    /// The source page.
    pub source: &'static str,
    /// The file's name in the model's folder and at the end of its URL.
    pub file_name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// The file's size in bytes.
    pub size: u64,
}

/// The float16 Face Landmarker bundle that [`crate::Bundle::open`] reads.
pub const FACE_LANDMARKER: ModelInfo = ModelInfo {
    id: "face-landmarker",
    name: "Face Landmarker",
    description: "Finds a face and follows 478 points of it with depth, on the processor.",
    license: "Apache-2.0",
    license_url: "https://storage.googleapis.com/mediapipe-assets/Model%20Card%20MediaPipe%20Face%20Mesh%20V2.pdf",
    author: "Google LLC",
    source: "https://developers.google.com/edge/mediapipe/solutions/vision/face_landmarker",
    file_name: "face_landmarker.task",
    url: "https://storage.googleapis.com/mediapipe-models/face_landmarker/face_landmarker/float16/1/face_landmarker.task",
    sha256: "64184e229b263107bc2b804c6625db1341ff2bb731874b0bcc2fe6544e0bc9ff",
    size: 3_758_596,
};

static CATALOG: &[ModelInfo] = &[FACE_LANDMARKER];

/// The model a tracker uses when none is named.
pub const DEFAULT_MODEL: &str = FACE_LANDMARKER.id;

pub fn catalog() -> &'static [ModelInfo] {
    CATALOG
}

/// The model with id `id` in `catalog`.
pub fn find_in<'a>(catalog: &'a [ModelInfo], id: &str) -> Option<&'a ModelInfo> {
    catalog.iter().find(|m| m.id == id)
}

/// The model with id `id`.
pub fn find(id: &str) -> Option<&'static ModelInfo> {
    find_in(CATALOG, id)
}

/// The folder model `m` is kept in under `models_dir`.
pub fn model_dir(models_dir: &Path, m: &ModelInfo) -> PathBuf {
    models_dir.join(m.id)
}

/// The file of model `m` under `models_dir`.
pub fn model_path(models_dir: &Path, m: &ModelInfo) -> PathBuf {
    model_dir(models_dir, m).join(m.file_name)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\'])
        && !matches!(name, "." | "..")
        && Path::new(name).components().count() == 1
        && Path::new(name).file_name().is_some_and(|part| part == name)
}

/// The model's file is in place with its pinned size. A link in its place does not count. Reads
/// none of the file, so it suits a status that is drawn often; [`installed`] also checks the
/// contents.
pub fn present(models_dir: &Path, m: &ModelInfo) -> bool {
    valid_name(m.id)
        && valid_name(m.file_name)
        && std::fs::symlink_metadata(model_path(models_dir, m)).is_ok_and(|meta| meta.is_file() && meta.len() == m.size)
}

/// The model's file is in place with its pinned size and SHA-256. Reads the whole file.
pub fn installed(models_dir: &Path, m: &ModelInfo) -> bool {
    present(models_dir, m) && check(&model_path(models_dir, m), m).is_ok()
}

/// Install progress: the bytes of the offered file read so far. Returning `false` stops the
/// install with [`Error::Canceled`].
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> bool;

/// The file at `path` is a file of model `m`'s pinned size. Reads none of it.
fn size_checked(path: &Path, m: &ModelInfo) -> Result<()> {
    let fail = |why: String| Error::Install(format!("{}: {why}", path.display()));
    let meta = std::fs::metadata(path).map_err(|e| fail(e.to_string()))?;
    if !meta.is_file() {
        return Err(fail("this is not a file".into()));
    }
    if meta.len() != m.size {
        return Err(fail(format!("the file is {} bytes; the model {} is {} bytes", meta.len(), m.name, m.size)));
    }
    Ok(())
}

/// Read `real` to its end, never past model `m`'s pinned size, hashing it and writing it to `out`
/// (with its path, for errors) when one is given, and check its length and SHA-256. Errors name
/// `shown`, the path the caller gave. `progress` hears the bytes read after each read.
fn read_checked(real: &Path, shown: &Path, m: &ModelInfo, mut out: Option<(&mut dyn Write, &Path)>, progress: Progress) -> Result<()> {
    let fail = |why: String| Error::Install(format!("{}: {why}", shown.display()));
    let mut input = std::fs::File::open(real).map_err(|e| fail(e.to_string()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    loop {
        let n = match input.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(fail(e.to_string())),
        };
        if n == 0 {
            break;
        }
        got = got.saturating_add(n as u64);
        // The file grew after its size was checked.
        if got > m.size {
            return Err(fail(format!("the file is longer than the {} bytes of the model {}", m.size, m.name)));
        }
        let chunk = buf.get(..n).unwrap_or_default();
        hasher.update(chunk);
        if let Some((w, path)) = out.as_mut() {
            w.write_all(chunk).map_err(|e| Error::Install(format!("{}: {e}", path.display())))?;
        }
        if !progress(got) {
            return Err(Error::Canceled);
        }
    }
    let sha: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != m.size || !sha.eq_ignore_ascii_case(m.sha256) {
        return Err(fail(format!("the file's SHA-256 is {sha} ({got} bytes); the model {} has {} ({} bytes)", m.name, m.sha256, m.size)));
    }
    Ok(())
}

/// Check the file at `path` against model `m`: its size first, without reading it, and then its
/// SHA-256.
pub fn check(path: &Path, m: &ModelInfo) -> Result<()> {
    size_checked(path, m)?;
    read_checked(path, path, m, None, &mut |_| true)
}

/// Copy the model file at `source` into `models_dir` as model `m`, and return where it went.
///
/// The source's size is checked before any of it is read. The copy streams into
/// `<file name>.part` beside the destination, is never longer than the pinned size, and is
/// renamed into place only when its SHA-256 matches; otherwise the part file is deleted and the
/// model already in place, if any, is kept. A model already installed is kept as it is once the
/// offered file passes the same check. `progress` is told the bytes read after each read, and
/// before anything is read.
pub fn install(source: &Path, models_dir: &Path, m: &ModelInfo, progress: Progress) -> Result<PathBuf> {
    if !valid_name(m.id) || !valid_name(m.file_name) {
        return Err(Error::Install(format!("invalid model name: {:?}", m.id)));
    }
    if !progress(0) {
        return Err(Error::Canceled);
    }
    size_checked(source, m)?;
    // The file a link leads to is read, so removing a link in the part file's place below does
    // not lose the source.
    let real = std::fs::canonicalize(source).map_err(|e| Error::Install(format!("{}: {e}", source.display())))?;
    let dest = model_path(models_dir, m);
    if installed(models_dir, m) {
        // The offered file is still checked, so a wrong one is an error.
        read_checked(&real, source, m, None, progress)?;
        return Ok(dest);
    }
    let dir = model_dir(models_dir, m);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Install(format!("{}: {e}", dir.display())))?;
    let part = dir.join(format!("{}.part", m.file_name));
    // The source is the part file itself: it is checked where it is and renamed into place, so a
    // file that fails the check is kept. A link in the part file's place is removed below and its
    // target copied, so the installed model is never a link.
    if std::fs::symlink_metadata(&part).is_ok_and(|meta| meta.is_file()) && std::fs::canonicalize(&part).is_ok_and(|p| p == real) {
        read_checked(&part, source, m, None, progress)?;
        std::fs::rename(&part, &dest).map_err(|e| Error::Install(format!("{}: {e}", dest.display())))?;
        return Ok(dest);
    }
    // A part file left behind, or a link in its place, is removed rather than written through.
    match std::fs::remove_file(&part) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Install(format!("{}: {e}", part.display()))),
    }
    let result = copy_checked(&real, source, &part, m, progress).and_then(|()| {
        std::fs::rename(&part, &dest).map_err(|e| Error::Install(format!("{}: {e}", dest.display())))?;
        Ok(dest)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Copy `real` into a new `part` file and check it; errors name `source`, the path the caller gave.
fn copy_checked(real: &Path, source: &Path, part: &Path, m: &ModelInfo, progress: Progress) -> Result<()> {
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(part).map_err(|e| Error::Install(format!("{}: {e}", part.display())))?;
    let mut out = std::io::BufWriter::new(file);
    read_checked(real, source, m, Some((&mut out, part)), progress)?;
    out.flush().map_err(|e| Error::Install(format!("{}: {e}", part.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_pinned_and_checksummed() {
        assert_eq!(find(DEFAULT_MODEL), Some(&FACE_LANDMARKER));
        for m in catalog() {
            assert!(valid_name(m.id) && valid_name(m.file_name), "{}", m.id);
            assert!(m.url.starts_with("https://") && m.url.ends_with(m.file_name), "{}", m.url);
            assert_eq!(m.sha256.len(), 64);
            assert!(m.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(m.size > 0 && m.size <= crate::MAX_BUNDLE_BYTES, "{}", m.size);
        }
        assert_eq!(FACE_LANDMARKER.size, 3_758_596);
        assert_eq!(FACE_LANDMARKER.sha256, "64184e229b263107bc2b804c6625db1341ff2bb731874b0bcc2fe6544e0bc9ff");
    }

    #[test]
    fn names_stay_inside_the_model_folder() {
        for name in ["../escape", "/tmp/escape", r"..\escape", r"C:\escape", "", ".", ".."] {
            assert!(!valid_name(name), "accepted {name:?}");
        }
        assert!(valid_name("face_landmarker.task"));
    }

    // SHA-256 of "abc".
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn tiny() -> ModelInfo {
        ModelInfo { id: "tiny", file_name: "tiny.task", sha256: ABC, size: 3, ..FACE_LANDMARKER }
    }

    fn put(source: &Path, models_dir: &Path) -> Result<PathBuf> {
        install(source, models_dir, &tiny(), &mut |_| true)
    }

    #[test]
    fn a_missing_model_is_neither_present_nor_installed() {
        let root = tempfile::tempdir().unwrap();
        assert!(!present(root.path(), &tiny()));
        assert!(!installed(root.path(), &tiny()));
    }

    #[test]
    fn same_length_corruption_is_present_but_not_installed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(model_dir(root.path(), &tiny())).unwrap();
        std::fs::write(model_path(root.path(), &tiny()), b"abd").unwrap();
        assert!(present(root.path(), &tiny()));
        assert!(!installed(root.path(), &tiny()));
        std::fs::write(model_path(root.path(), &tiny()), b"abc").unwrap();
        assert!(installed(root.path(), &tiny()));
    }

    #[test]
    fn a_good_file_is_copied_into_place() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("mine.task");
        std::fs::write(&source, b"abc").unwrap();
        let models = root.path().join("models");
        let mut heard = Vec::new();
        let dest = install(&source, &models, &tiny(), &mut |n| {
            heard.push(n);
            true
        })
        .unwrap();
        assert_eq!(heard, [0, 3], "progress before the first read and after each read");
        assert_eq!(dest, models.join("tiny").join("tiny.task"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        assert!(installed(&models, &tiny()));
        assert!(!models.join("tiny").join("tiny.task.part").exists());
        // Installing the installed file again keeps it.
        assert_eq!(put(&dest, &models).unwrap(), dest);
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        // A wrong file offered while the model is installed is still refused.
        std::fs::write(&source, b"abd").unwrap();
        let e = put(&source, &models).unwrap_err().to_string();
        assert!(e.contains("SHA-256"), "{e}");
        assert!(installed(&models, &tiny()));
        // So is one stopped while it is checked.
        std::fs::write(&source, b"abc").unwrap();
        let e = install(&source, &models, &tiny(), &mut |n| n == 0).unwrap_err();
        assert_eq!(e, Error::Canceled);
        assert!(installed(&models, &tiny()));
    }

    #[test]
    fn a_wrong_file_is_refused_and_leaves_nothing_behind() {
        for (body, why) in [(&b"abd"[..], "SHA-256"), (&b"abcd"[..], "is 4 bytes"), (&b""[..], "is 0 bytes")] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("mine.task");
            std::fs::write(&source, body).unwrap();
            let models = root.path().join("models");
            let e = put(&source, &models).unwrap_err().to_string();
            assert!(e.contains(why), "{e}");
            assert!(!model_path(&models, &tiny()).exists());
            assert!(!models.join("tiny").join("tiny.task.part").exists());
        }
        let root = tempfile::tempdir().unwrap();
        let e = put(&root.path().join("none.task"), root.path()).unwrap_err();
        assert!(matches!(e, Error::Install(_)), "{e}");
        let e = put(root.path(), root.path()).unwrap_err().to_string();
        assert!(e.contains("not a file"), "{e}");
    }

    #[test]
    fn a_wrong_file_keeps_the_model_already_in_place() {
        let root = tempfile::tempdir().unwrap();
        // A same-length damaged model in place, and a same-length wrong file offered.
        std::fs::create_dir_all(model_dir(root.path(), &tiny())).unwrap();
        std::fs::write(model_path(root.path(), &tiny()), b"xyz").unwrap();
        let source = root.path().join("mine.task");
        std::fs::write(&source, b"abd").unwrap();
        assert!(put(&source, root.path()).is_err());
        assert_eq!(std::fs::read(model_path(root.path(), &tiny())).unwrap(), b"xyz");
        // The right file replaces the damaged one.
        std::fs::write(&source, b"abc").unwrap();
        put(&source, root.path()).unwrap();
        assert!(installed(root.path(), &tiny()));
    }

    #[test]
    fn the_part_file_offered_as_the_source_is_checked_where_it_is() {
        let root = tempfile::tempdir().unwrap();
        let part = model_dir(root.path(), &tiny()).join("tiny.task.part");
        std::fs::create_dir_all(model_dir(root.path(), &tiny())).unwrap();
        std::fs::write(&part, b"abd").unwrap();
        assert!(put(&part, root.path()).is_err());
        assert_eq!(std::fs::read(&part).unwrap(), b"abd", "a wrong file is kept");
        std::fs::write(&part, b"abc").unwrap();
        for stop_at in [0, 3] {
            assert_eq!(install(&part, root.path(), &tiny(), &mut |n| n < stop_at).unwrap_err(), Error::Canceled);
            assert!(part.exists() && !installed(root.path(), &tiny()), "a stopped install changes nothing");
        }
        put(&part, root.path()).unwrap();
        assert!(installed(root.path(), &tiny()));
        assert!(!part.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_part_files_place_is_replaced_by_a_copy() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("mine.task");
        std::fs::write(&source, b"abc").unwrap();
        let part = model_dir(root.path(), &tiny()).join("tiny.task.part");
        std::fs::create_dir_all(model_dir(root.path(), &tiny())).unwrap();
        std::os::unix::fs::symlink(&source, &part).unwrap();
        put(&part, root.path()).unwrap();
        assert!(installed(root.path(), &tiny()), "the model is a file, not a link");
        assert_eq!(std::fs::read(&source).unwrap(), b"abc", "the linked file is kept");
    }

    #[test]
    fn a_stopped_install_deletes_its_part_file() {
        for stop_at in [0, 3] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("mine.task");
            std::fs::write(&source, b"abc").unwrap();
            let e = install(&source, root.path(), &tiny(), &mut |n| n < stop_at).unwrap_err();
            assert_eq!(e, Error::Canceled);
            assert!(!model_path(root.path(), &tiny()).exists());
            assert!(!root.path().join("tiny").join("tiny.task.part").exists());
        }
    }

    #[test]
    fn check_reads_the_size_before_the_contents() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("big.task");
        std::fs::write(&path, vec![0u8; 4]).unwrap();
        let e = check(&path, &tiny()).unwrap_err().to_string();
        assert!(e.contains("is 4 bytes"), "{e}");
        std::fs::write(&path, b"abc").unwrap();
        check(&path, &tiny()).unwrap();
    }

    /// The published bundle, when the tests are given it (`CRAFT_FACE_LANDMARKER`), matches the
    /// catalog's pins and installs.
    #[test]
    fn the_published_bundle_matches_its_pins() {
        let Some(path) = std::env::var_os("CRAFT_FACE_LANDMARKER").map(PathBuf::from) else {
            eprintln!("skipped: CRAFT_FACE_LANDMARKER is not set");
            return;
        };
        check(&path, &FACE_LANDMARKER).unwrap();
        let root = tempfile::tempdir().unwrap();
        let dest = install(&path, root.path(), &FACE_LANDMARKER, &mut |_| true).unwrap();
        assert!(installed(root.path(), &FACE_LANDMARKER));
        crate::Bundle::open(&dest).unwrap();
    }
}
