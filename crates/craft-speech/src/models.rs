//! The speech model catalog and the model store.
//!
//! A model is downloaded only when the user asks, into `<models dir>/<id>/`. Models are never
//! bundled with an app or committed to a repository. Every file is pinned to a Hugging Face
//! revision of OpenAI's own repositories and checked against its size and SHA-256 before it is
//! used.
//!
//! License: OpenAI released the Whisper code and model weights under the MIT license
//! (<https://github.com/openai/whisper/blob/main/LICENSE>); the safetensors conversions OpenAI
//! publishes at `huggingface.co/openai/whisper-*` are labeled Apache-2.0.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::Digest;

/// One file of a model.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

/// A downloadable speech model.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// Recognizes languages other than English (and detects the language).
    pub multilingual: bool,
    pub description: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    pub author: &'static str,
    /// The source page.
    pub source: &'static str,
    pub files: &'static [ModelFile],
}

impl ModelInfo {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

const LICENSE: &str = "MIT (OpenAI Whisper weights); Hugging Face conversion: Apache-2.0";
const LICENSE_URL: &str = "https://github.com/openai/whisper/blob/main/LICENSE";
const AUTHOR: &str = "OpenAI";

macro_rules! hf {
    ($repo:literal, $rev:literal, $name:literal, $sha:literal, $size:expr) => {
        ModelFile { name: $name, url: concat!("https://huggingface.co/openai/", $repo, "/resolve/", $rev, "/", $name), sha256: $sha, size: $size }
    };
}

const TOKENIZER_MULTI: &str = "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566";

static CATALOG: &[ModelInfo] = &[
    ModelInfo {
        id: "whisper-tiny",
        name: "Whisper tiny (multilingual)",
        multilingual: true,
        description: "39 M parameters. The fastest; fine for clear speech, rough on noisy audio.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-tiny",
        files: &[
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "config.json",
                "ffdccec4f3211f4c63310f2b7098f309fe70f3952cedc5e4d11e43f5b2379b98",
                1983
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "generation_config.json",
                "a5d5325911f16e74001a72fa13d6e208eee51548f994646de1f4b4cc8b35b512",
                3747
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "model.safetensors",
                "7ebd0e69e78190ffe1438491fa05cc1f5c1aa3a4c4db3bc1723adbb551ea2395",
                151061672
            ),
        ],
    },
    ModelInfo {
        id: "whisper-base",
        name: "Whisper base (multilingual)",
        multilingual: true,
        description: "74 M parameters. The default: a balance of speed and accuracy.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-base",
        files: &[
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "config.json",
                "a153c53883a6799b6f056b4a8d1a515c9926d03994682ba88a7616618d7da0c1",
                1983
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "generation_config.json",
                "444b3f636d2fff89dd9ecf549e2a085b61f7ff0fa0246d4628bac6a3b8cc9ba4",
                3807
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "model.safetensors",
                "07cadb9f25677c8d50df603e66a98fbd842cce45047139baeb16e6219a1e807b",
                290403936
            ),
        ],
    },
    ModelInfo {
        id: "whisper-small",
        name: "Whisper small (multilingual)",
        multilingual: true,
        description: "244 M parameters. The most accurate here; about 4 times slower than base.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-small",
        files: &[
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "config.json",
                "e6a2b489da1b5aed65a8eb8d1e7466fa867ad5643a8bc138ba708bd56b2875c4",
                1967
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "generation_config.json",
                "71565b8ef50d0bf7a1193ed4bbed195b94e70c18894d81bba2f1233dcec3ab53",
                3868
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "model.safetensors",
                "1d7734884874f1a1513ed9aa760a4f8e97aaa02fd6d93a3a85d27b2ae9ca596b",
                966995080
            ),
        ],
    },
];

/// The default model.
pub const DEFAULT_MODEL: &str = "whisper-base";

pub fn catalog() -> &'static [ModelInfo] {
    CATALOG
}

pub fn find(id: &str) -> Option<&'static ModelInfo> {
    CATALOG.iter().find(|m| m.id == id)
}

/// Where model `m` is kept under `models_dir`.
pub fn model_dir(models_dir: &Path, m: &ModelInfo) -> PathBuf {
    models_dir.join(m.id)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\'])
        && !matches!(name, "." | "..")
        && Path::new(name).components().count() == 1
        && Path::new(name).file_name().is_some_and(|part| part == name)
}

fn right_size(path: &Path, file: &ModelFile) -> bool {
    valid_name(file.name) && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() == file.size)
}

fn valid_file(path: &Path, file: &ModelFile) -> bool {
    if !right_size(path, file) {
        return false;
    }
    let Ok(mut input) = std::fs::File::open(path) else { return false };
    let mut hasher = sha2::Sha256::new();
    if std::io::copy(&mut input, &mut hasher).is_err() {
        return false;
    }
    format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(file.sha256)
}

/// Every file is present with its pinned size and SHA-256. Reads every byte of the model; use
/// [`present`] for a quick look.
pub fn installed(models_dir: &Path, m: &ModelInfo) -> bool {
    let d = model_dir(models_dir, m);
    m.files.iter().all(|f| valid_file(&d.join(f.name), f))
}

/// Every file is present with its pinned size. Reads no file's contents, so it suits a list
/// that is drawn often; [`installed`] also checks the contents.
pub fn present(models_dir: &Path, m: &ModelInfo) -> bool {
    let d = model_dir(models_dir, m);
    m.files.iter().all(|f| right_size(&d.join(f.name), f))
}

/// Bytes still to download for `m` (files present with their pinned size and SHA-256 are skipped).
pub fn missing_bytes(models_dir: &Path, m: &ModelInfo) -> u64 {
    let d = model_dir(models_dir, m);
    m.files.iter().filter(|f| !valid_file(&d.join(f.name), f)).map(|f| f.size).sum()
}

/// The tokenizer shared by the multilingual models has one checksum.
pub fn shared_tokenizer_sha() -> &'static str {
    TOKENIZER_MULTI
}

/// Download progress: `(bytes done, bytes total, file)`; return `false` to stop.
pub type DownloadProgress<'a> = &'a mut dyn FnMut(u64, u64, &str) -> bool;

/// Download the missing files of `m` into `models_dir/<id>/` (feature `download`). See
/// [`download_files`].
pub fn download(models_dir: &Path, m: &ModelInfo, progress: DownloadProgress) -> Result<(), crate::SpeechError> {
    download_files(&model_dir(models_dir, m), m.files, progress)
}

/// Download the files of `files` that `dir` lacks. Each streams into `<name>.part`, which is never
/// longer than the file's pinned size: a server that sends more, a length header that differs
/// from the pinned size, or a wrong SHA-256 deletes the part file and fails. A good file is then
/// renamed into place. Stopping through `progress` deletes the part file.
pub fn download_files(dir: &Path, files: &[ModelFile], progress: DownloadProgress) -> Result<(), crate::SpeechError> {
    #[cfg(feature = "download")]
    {
        let fetch = |url: &str| -> Result<(Option<u64>, Box<dyn std::io::Read>), crate::SpeechError> {
            let resp = agent().get(url).call().map_err(|e| crate::SpeechError::Download(format!("{url}: {e}")))?;
            let length = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
            Ok((length, Box::new(resp.into_body().into_reader())))
        };
        fetch_files(dir, files, &fetch, progress)
    }
    #[cfg(not(feature = "download"))]
    {
        let _ = (dir, files, progress);
        Err(crate::SpeechError::Unavailable("built without the `download` feature".into()))
    }
}

/// A source of one file's bytes: its length header when the server sent one, and the body.
pub type Fetch<'a> = &'a dyn Fn(&str) -> Result<(Option<u64>, Box<dyn std::io::Read>), crate::SpeechError>;

/// [`download_files`] with the bytes from `fetch` (tests give it bytes from memory).
pub fn fetch_files(dir: &Path, files: &[ModelFile], fetch: Fetch, progress: DownloadProgress) -> Result<(), crate::SpeechError> {
    use crate::SpeechError;
    use std::io::{Read, Write};
    if let Some(file) = files.iter().find(|file| !valid_name(file.name)) {
        return Err(SpeechError::Download(format!("invalid model file name: {:?}", file.name)));
    }
    std::fs::create_dir_all(dir)?;
    let total: u64 = files.iter().filter(|f| !valid_file(&dir.join(f.name), f)).map(|f| f.size).sum();
    let mut done = 0u64;
    for f in files {
        let dest = dir.join(f.name);
        if valid_file(&dest, f) {
            continue;
        }
        let part = dir.join(format!("{}.part", f.name));
        let fail = |e: SpeechError| {
            let _ = std::fs::remove_file(&part);
            e
        };
        let (length, mut reader) = fetch(f.url)?;
        if let Some(n) = length
            && n != f.size
        {
            return Err(SpeechError::Download(format!("{}: the server offers {n} bytes; {} bytes were expected", f.name, f.size)));
        }
        // A part file left behind, or a link in its place, is removed rather than written through.
        match std::fs::remove_file(&part) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut out = std::io::BufWriter::new(std::fs::OpenOptions::new().write(true).create_new(true).open(&part)?);
        let mut hasher = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut got = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    drop(out);
                    return Err(fail(SpeechError::Download(format!("{}: {e}", f.name))));
                }
            };
            if n == 0 {
                break;
            }
            got = got.saturating_add(n as u64);
            if got > f.size {
                drop(out);
                return Err(fail(SpeechError::Download(format!("{}: the server sent more than the expected {} bytes", f.name, f.size))));
            }
            let chunk = buf.get(..n).unwrap_or_default();
            hasher.update(chunk);
            if let Err(e) = out.write_all(chunk) {
                drop(out);
                return Err(fail(e.into()));
            }
            if !progress(done.saturating_add(got), total, f.name) {
                drop(out);
                return Err(fail(SpeechError::Canceled));
            }
        }
        if let Err(e) = out.flush() {
            drop(out);
            return Err(fail(e.into()));
        }
        drop(out);
        let sha: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !sha.eq_ignore_ascii_case(f.sha256) || got != f.size {
            return Err(fail(SpeechError::Download(format!(
                "{}: checksum mismatch (got {sha}, {got} bytes; expected {}, {} bytes)",
                f.name, f.sha256, f.size
            ))));
        }
        std::fs::rename(&part, &dest).map_err(|e| fail(e.into()))?;
        done = done.saturating_add(f.size);
    }
    Ok(())
}

#[cfg(feature = "download")]
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(60)))
        // A file's body fails when it has not arrived within two hours (the largest model at
        // about 130 KB/s).
        .timeout_recv_body(Some(std::time::Duration::from_secs(2 * 3600)))
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpeechError;

    #[test]
    fn catalog_is_pinned_and_checksummed() {
        assert!(find(DEFAULT_MODEL).is_some());
        for m in catalog() {
            assert!(m.size() > 100_000_000, "{}", m.id);
            for f in m.files {
                assert!(f.url.starts_with("https://huggingface.co/openai/whisper-"), "{}", f.url);
                assert!(f.url.contains("/resolve/") && f.url.ends_with(f.name));
                assert_eq!(f.sha256.len(), 64);
                assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
                if f.name == "tokenizer.json" {
                    assert_eq!(f.sha256, shared_tokenizer_sha());
                }
            }
        }
    }

    #[test]
    fn a_missing_model_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let m = find("whisper-tiny").unwrap();
        assert!(!installed(dir.path(), m));
        assert!(!present(dir.path(), m));
        assert_eq!(missing_bytes(dir.path(), m), m.size());
    }

    #[test]
    fn file_names_stay_inside_the_model_folder() {
        for name in ["../escape", "/tmp/escape", r"..\escape", r"C:\escape", "", ".", ".."] {
            assert!(!valid_name(name), "accepted {name:?}");
        }
        assert!(valid_name("model.safetensors"));
    }

    // SHA-256 of "abc".
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    static FILES: &[ModelFile] = &[ModelFile { name: "tiny.bin", url: "https://example.invalid/tiny.bin", sha256: ABC, size: 3 }];

    fn model() -> ModelInfo {
        ModelInfo {
            id: "integrity-test",
            name: "",
            multilingual: false,
            description: "",
            license: "",
            license_url: "",
            author: "",
            source: "",
            files: FILES,
        }
    }

    #[test]
    fn same_length_corruption_is_present_but_not_installed() {
        let root = tempfile::tempdir().unwrap();
        let directory = model_dir(root.path(), &model());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("tiny.bin"), b"bad").unwrap();
        assert!(present(root.path(), &model()));
        assert!(!installed(root.path(), &model()));
        assert_eq!(missing_bytes(root.path(), &model()), 3);
        std::fs::write(directory.join("tiny.bin"), b"abc").unwrap();
        assert!(installed(root.path(), &model()));
    }

    fn serve(length: Option<u64>, body: &'static [u8]) -> impl Fn(&str) -> Result<(Option<u64>, Box<dyn std::io::Read>), SpeechError> {
        move |_| Ok((length, Box::new(body) as Box<dyn std::io::Read>))
    }

    #[test]
    fn a_good_download_is_checked_and_put_in_place() {
        let root = tempfile::tempdir().unwrap();
        let mut seen = Vec::new();
        fetch_files(root.path(), FILES, &serve(Some(3), b"abc"), &mut |done, total, _| {
            seen.push((done, total));
            true
        })
        .unwrap();
        assert_eq!(std::fs::read(root.path().join("tiny.bin")).unwrap(), b"abc");
        assert!(!root.path().join("tiny.bin.part").exists());
        assert_eq!(seen.last(), Some(&(3, 3)));
        // A second call finds nothing to fetch.
        let refuse = |_: &str| -> Result<(Option<u64>, Box<dyn std::io::Read>), SpeechError> { Err(SpeechError::Download("fetched".into())) };
        fetch_files(root.path(), FILES, &refuse, &mut |_, _, _| true).unwrap();
    }

    #[test]
    fn a_bad_download_leaves_nothing_behind() {
        for (length, body, why) in [
            (Some(3), &b"abd"[..], "checksum"),
            (None, &b"abcabcabc"[..], "more than"),
            (Some(9), &b"abcabcabc"[..], "offers 9 bytes"),
            (None, &b"ab"[..], "checksum"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let body: &'static [u8] = Box::leak(body.to_vec().into_boxed_slice());
            let e = fetch_files(root.path(), FILES, &serve(length, body), &mut |_, _, _| true).unwrap_err();
            assert!(e.to_string().contains(why), "{e}");
            assert!(!root.path().join("tiny.bin").exists());
            assert!(!root.path().join("tiny.bin.part").exists());
        }
    }

    #[test]
    fn a_stopped_download_deletes_its_part_file() {
        let root = tempfile::tempdir().unwrap();
        let e = fetch_files(root.path(), FILES, &serve(Some(3), b"abc"), &mut |_, _, _| false).unwrap_err();
        assert_eq!(e, SpeechError::Canceled);
        assert!(!root.path().join("tiny.bin.part").exists());
    }
}
