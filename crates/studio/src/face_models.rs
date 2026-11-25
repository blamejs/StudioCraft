//! The face tracking model: its download, its install from a file the user has, and its status.
//!
//! The model is kept in the models folder the speech models use, as
//! `<models dir>/<id>/<file name>`. `avatar.model_download` and `avatar.model_install` each run on
//! a thread of their own, one at a time, so neither the network nor the disk holds up a frame.
//! The studio collects a finished one on the next tick or command and reports it as a notice.
//! Every file is checked against the model's pinned size and SHA-256 before it is put in place,
//! and a file that fails the check is deleted.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use craft_face_track::models::{self, ModelInfo};
use craft_speech::SpeechError;
use craft_speech::models::ModelFile;
use serde_json::{Value, json};

use crate::model_jobs::Held;
use crate::{Notice, Studio, StudioError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Download,
    Install,
}

impl Kind {
    fn text(self) -> &'static str {
        match self {
            Kind::Download => "download",
            Kind::Install => "install",
        }
    }
}

/// How a download or install ended.
enum End {
    Done,
    Canceled,
    Failed(String),
}

/// A download or install that runs.
struct Job {
    kind: Kind,
    model: ModelInfo,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    handle: JoinHandle<End>,
}

pub(crate) struct FaceModels {
    catalog: &'static [ModelInfo],
    job: Option<Job>,
    /// Why the last download or install failed, kept until the next one starts.
    failure: Option<String>,
}

impl Default for FaceModels {
    fn default() -> FaceModels {
        FaceModels { catalog: models::catalog(), job: None, failure: None }
    }
}

impl Studio {
    /// Model `id` from the catalog, or the default model when `id` is `None`.
    fn face_model(&self, id: Option<&str>) -> Result<ModelInfo, StudioError> {
        let catalog = self.face_models.catalog;
        let found = match id {
            Some(id) => models::find_in(catalog, id),
            None => models::find_in(catalog, models::DEFAULT_MODEL).or_else(|| catalog.first()),
        };
        found.copied().ok_or_else(|| StudioError::Output(format!("there is no face tracking model `{}`", id.unwrap_or(models::DEFAULT_MODEL))))
    }

    /// The folder and the file of face tracking model `model`, or of the default model when none
    /// is named. Empty when there is no such model.
    pub fn face_model_paths(&self, model: Option<&str>) -> Vec<PathBuf> {
        self.face_model(model).map(|m| vec![models::model_dir(self.models_dir(), &m), models::model_path(self.models_dir(), &m)]).unwrap_or_default()
    }

    fn face_job_free(&self) -> Result<(), StudioError> {
        match &self.face_models.job {
            Some(j) => {
                let doing = match j.kind {
                    Kind::Download => "downloaded",
                    Kind::Install => "installed",
                };
                Err(StudioError::Output(format!("the face tracking model `{}` is already being {doing}", j.model.id)))
            }
            None => Ok(()),
        }
    }

    fn start_face_job(
        &mut self,
        kind: Kind,
        model: ModelInfo,
        work: impl FnOnce(&AtomicBool, &AtomicU64) -> End + Send + 'static,
    ) -> Result<(), StudioError> {
        // The worker holds the model's folder until it ends, even after its studio has closed.
        let held = Held::take(&models::model_dir(self.models_dir(), &model)).ok_or_else(|| {
            StudioError::Output(format!(
                "an earlier download or install of the face tracking model `{}` has not ended yet; try again shortly",
                model.id
            ))
        })?;
        let (cancel, done, total) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(model.size)));
        let (c, d) = (Arc::clone(&cancel), Arc::clone(&done));
        let handle = std::thread::Builder::new()
            .name(format!("face-model-{}", kind.text()))
            .spawn(move || {
                let end = work(&c, &d);
                drop(held);
                end
            })
            .map_err(|e| StudioError::Output(format!("the {} could not start: {e}", kind.text())))?;
        self.face_models.failure = None;
        self.face_models.job = Some(Job { kind, model, cancel, done, total, handle });
        Ok(())
    }

    /// Download `model`, or the default face tracking model, into the models folder.
    pub(crate) fn download_face_model(&mut self, model: Option<String>) -> Result<Value, StudioError> {
        let info = self.face_model(model.as_deref())?;
        self.face_job_free()?;
        let dir = models::model_dir(self.models_dir(), &info);
        let file = ModelFile { name: info.file_name, url: info.url, sha256: info.sha256, size: info.size };
        // The worker checks a file already there, so it is not hashed on this thread.
        self.start_face_job(Kind::Download, info, move |cancel, done| {
            match craft_speech::models::download_files(&dir, &[file], &mut |got, _, _| {
                done.store(got, Ordering::Relaxed);
                !cancel.load(Ordering::Relaxed)
            }) {
                Ok(()) => End::Done,
                Err(SpeechError::Canceled) => End::Canceled,
                Err(e) => End::Failed(e.to_string()),
            }
        })?;
        Ok(json!({"model": info.id, "downloading": true, "bytes": info.size}))
    }

    /// Copy the model file at `path` into the models folder as `model`, or as the default face
    /// tracking model. The file's size is checked here; its contents are read and checked on the
    /// install's thread.
    pub(crate) fn install_face_model(&mut self, path: String, model: Option<String>) -> Result<Value, StudioError> {
        let info = self.face_model(model.as_deref())?;
        self.face_job_free()?;
        let source = PathBuf::from(&path);
        let meta = std::fs::metadata(&source).map_err(|e| StudioError::Output(format!("the model file {path} could not be read: {e}")))?;
        if !meta.is_file() {
            return Err(StudioError::Output(format!("{path} is not a file")));
        }
        if meta.len() != info.size {
            return Err(StudioError::Output(format!("{path} is {} bytes; the face tracking model {} is {} bytes", meta.len(), info.name, info.size)));
        }
        let dir = self.models_dir().to_path_buf();
        self.start_face_job(Kind::Install, info, move |cancel, done| {
            match models::install(&source, &dir, &info, &mut |got| {
                done.store(got, Ordering::Relaxed);
                !cancel.load(Ordering::Relaxed)
            }) {
                Ok(_) => End::Done,
                Err(craft_face_track::Error::Canceled) => End::Canceled,
                Err(e) => End::Failed(e.to_string()),
            }
        })?;
        Ok(json!({"model": info.id, "installing": true, "bytes": info.size}))
    }

    /// Stop the face tracking model download or install that runs.
    pub(crate) fn cancel_face_model_job(&mut self) {
        if let Some(j) = &self.face_models.job {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn report_face_job(&mut self, kind: Kind, model: &ModelInfo, end: End) {
        let name = vec![("model", model.name.to_string())];
        let notice = match (kind, end) {
            (Kind::Download, End::Done) => Notice::new("Face tracking model {model} downloaded", name, false),
            (Kind::Install, End::Done) => Notice::new("Face tracking model {model} installed", name, false),
            (Kind::Download, End::Canceled) => Notice::new("The download of face tracking model {model} was canceled", name, false),
            (Kind::Install, End::Canceled) => Notice::new("The install of face tracking model {model} was canceled", name, false),
            (kind, End::Failed(e)) => {
                self.face_models.failure = Some(e.clone());
                let template = match kind {
                    Kind::Download => "Face tracking model {model} could not be downloaded: {error}",
                    Kind::Install => "Face tracking model {model} could not be installed: {error}",
                };
                let mut args = name;
                args.push(("error", e));
                Notice::new(template, args, true)
            }
        };
        self.notices.push(notice);
    }

    /// Report a download or install that has ended. Runs each tick and before each command.
    pub(crate) fn sync_face_models(&mut self) {
        if let Some(j) = self.face_models.job.take_if(|j| j.handle.is_finished()) {
            let end = j.handle.join().unwrap_or_else(|_| End::Failed(format!("the {} stopped unexpectedly", j.kind.text())));
            self.report_face_job(j.kind, &j.model, end);
        }
    }

    /// Stop the download or install (the studio is closing). One waiting on the network or the
    /// disk is not waited for; it stops at its next read and deletes its part file.
    pub(crate) fn finish_face_models_blocking(&mut self) {
        self.cancel_face_model_job();
        self.sync_face_models();
        self.face_models.job = None;
    }

    /// What `avatar.models` reports.
    pub(crate) fn face_models_json(&self) -> Value {
        let dir = self.models_dir();
        let rows: Vec<Value> = self
            .face_models
            .catalog
            .iter()
            .map(|m| {
                let shown = self.model_files_shown(dir, &[models::model_dir(dir, m), models::model_path(dir, m)]);
                let installed = if shown { Value::Bool(models::present(dir, m)) } else { Value::Null };
                json!({
                    "id": m.id, "name": m.name, "description": m.description, "bytes": m.size, "installed": installed,
                    "license": m.license, "license_url": m.license_url, "author": m.author, "source": m.source,
                })
            })
            .collect();
        let job = self.face_models.job.as_ref().map(
            |j| json!({"model": j.model.id, "kind": j.kind.text(), "done": j.done.load(Ordering::Relaxed), "total": j.total.load(Ordering::Relaxed)}),
        );
        json!({"models_dir": dir.display().to_string(), "models": rows, "job": job, "failure": self.face_models.failure})
    }

    /// Use `catalog` instead of the built-in one (tests give models they can serve).
    #[cfg(test)]
    pub(crate) fn set_face_model_catalog(&mut self, catalog: &'static [ModelInfo]) {
        self.face_models.catalog = catalog;
    }

    /// Wait for the download or install that runs, and report it.
    #[cfg(test)]
    pub(crate) fn wait_face_model_job(&mut self) {
        if let Some(j) = self.face_models.job.take() {
            let end = j.handle.join().unwrap_or_else(|_| End::Failed("the job panicked".into()));
            self.report_face_job(j.kind, &j.model, end);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use serde_json::json;

    use super::*;
    use crate::tests::headless;

    // SHA-256 of "abc".
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    /// A server on this computer that answers one request: its headers at once, and `body` only
    /// once something is sent on the returned channel, so a test decides when the download can end.
    fn serve(body: &'static [u8]) -> (String, mpsc::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/tiny.task", listener.local_addr().unwrap());
        let (go, wait) = mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let Ok(read) = stream.try_clone() else { return };
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.flush();
            let _ = wait.recv();
            let _ = stream.write_all(body);
            let _ = stream.flush();
        });
        (url, go, server)
    }

    /// A catalog of one 3-byte model, served from `url`, whose SHA-256 is that of "abc".
    fn tiny(url: String) -> &'static [ModelInfo] {
        let url: &'static str = Box::leak(url.into_boxed_str());
        Box::leak(Box::new([ModelInfo { id: "tiny", file_name: "tiny.task", url, sha256: ABC, size: 3, ..models::FACE_LANDMARKER }]))
    }

    fn templates(s: &mut Studio) -> Vec<&'static str> {
        s.take_notices().into_iter().map(|n| n.template).collect()
    }

    fn status(s: &mut Studio) -> Value {
        s.run("avatar.models", &json!({})).unwrap().0
    }

    #[test]
    fn the_catalog_lists_the_model_and_whether_it_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = headless();
        s.set_models_dir(dir.path().to_path_buf());
        let st = status(&mut s);
        let rows = st["models"].as_array().unwrap();
        assert_eq!(rows.len(), models::catalog().len());
        assert_eq!(rows[0]["id"], "face-landmarker");
        assert_eq!(rows[0]["bytes"], 3_758_596);
        assert_eq!(rows[0]["installed"], false);
        assert_eq!(rows[0]["license"], "Apache-2.0");
        assert_eq!((st["job"].clone(), st["failure"].clone()), (Value::Null, Value::Null));
        assert_eq!(st["models_dir"], dir.path().display().to_string());
        let e = s.run("avatar.model_download", &json!({"model": "nope"})).unwrap_err().to_string();
        assert!(e.contains("no face tracking model `nope`"), "{e}");
        let e = s.run("avatar.model_install", &json!({"path": "x.task", "model": "nope"})).unwrap_err().to_string();
        assert!(e.contains("no face tracking model `nope`"), "{e}");
        assert_eq!(
            s.face_model_paths(None),
            vec![dir.path().join("face-landmarker"), dir.path().join("face-landmarker").join("face_landmarker.task")]
        );
        assert!(s.face_model_paths(Some("nope")).is_empty());
    }

    #[test]
    fn a_download_runs_on_its_own_thread_and_puts_the_model_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (url, go, server) = serve(b"abc");
        let mut s = headless();
        s.set_models_dir(dir.path().to_path_buf());
        s.set_face_model_catalog(tiny(url));
        let answer = s.run("avatar.model_download", &json!({})).unwrap().0;
        assert_eq!(answer, json!({"model": "tiny", "downloading": true, "bytes": 3}));
        // The server holds the body back, and the studio goes on answering.
        let st = status(&mut s);
        assert_eq!(st["job"]["kind"], "download", "{st}");
        assert_eq!(st["job"]["total"], 3);
        assert_eq!(st["models"][0]["installed"], false);
        let e = s.run("avatar.model_download", &json!({})).unwrap_err().to_string();
        assert!(e.contains("already being downloaded"), "{e}");
        let e = s.run("avatar.model_install", &json!({"path": dir.path().display().to_string()})).unwrap_err().to_string();
        assert!(e.contains("already being downloaded"), "{e}");
        go.send(()).unwrap();
        s.wait_face_model_job();
        server.join().unwrap();
        assert_eq!(templates(&mut s), ["Face tracking model {model} downloaded"]);
        let st = status(&mut s);
        assert_eq!((st["models"][0]["installed"].clone(), st["job"].clone(), st["failure"].clone()), (json!(true), Value::Null, Value::Null));
        assert_eq!(std::fs::read(dir.path().join("tiny").join("tiny.task")).unwrap(), b"abc");
        assert!(!dir.path().join("tiny").join("tiny.task.part").exists());
    }

    #[test]
    fn a_canceled_download_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let (url, go, server) = serve(b"abc");
        let mut s = headless();
        s.set_models_dir(dir.path().to_path_buf());
        s.set_face_model_catalog(tiny(url));
        s.run("avatar.model_download", &json!({})).unwrap();
        s.run("avatar.model_download_cancel", &json!({})).unwrap();
        go.send(()).unwrap();
        s.wait_face_model_job();
        server.join().unwrap();
        assert_eq!(templates(&mut s), ["The download of face tracking model {model} was canceled"]);
        assert!(!dir.path().join("tiny").join("tiny.task").exists());
        assert!(!dir.path().join("tiny").join("tiny.task.part").exists());
        let st = status(&mut s);
        assert_eq!((st["models"][0]["installed"].clone(), st["failure"].clone()), (json!(false), Value::Null));
        // Canceling with nothing running does nothing.
        s.run("avatar.model_download_cancel", &json!({})).unwrap();
        assert!(s.take_notices().is_empty());
    }

    #[test]
    fn a_download_that_fails_its_checksum_is_deleted_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (url, go, server) = serve(b"abd");
        let mut s = headless();
        s.set_models_dir(dir.path().to_path_buf());
        s.set_face_model_catalog(tiny(url));
        s.run("avatar.model_download", &json!({"model": "tiny"})).unwrap();
        go.send(()).unwrap();
        s.wait_face_model_job();
        server.join().unwrap();
        let notices = s.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].template, "Face tracking model {model} could not be downloaded: {error}");
        assert!(notices[0].error && notices[0].text().contains("checksum"), "{}", notices[0].text());
        assert!(!dir.path().join("tiny").join("tiny.task").exists());
        assert!(!dir.path().join("tiny").join("tiny.task.part").exists());
        let st = status(&mut s);
        assert!(st["failure"].as_str().is_some_and(|f| f.contains("checksum")), "{st}");
        assert_eq!(st["models"][0]["installed"], false);
    }

    #[test]
    fn an_install_checks_the_size_at_once_and_the_contents_on_its_thread() {
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("models");
        let mut s = headless();
        s.set_models_dir(models_dir.clone());
        s.set_face_model_catalog(tiny("https://example.invalid/tiny.task".into()));
        let file = |name: &str, body: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            p.display().to_string()
        };
        let install = |s: &mut Studio, path: String| s.run("avatar.model_install", &json!({"path": path}));
        let e = install(&mut s, file("long.task", b"abcd")).unwrap_err().to_string();
        assert!(e.contains("is 4 bytes") && e.contains("is 3 bytes"), "{e}");
        let e = install(&mut s, dir.path().join("none.task").display().to_string()).unwrap_err().to_string();
        assert!(e.contains("could not be read"), "{e}");
        let e = install(&mut s, dir.path().display().to_string()).unwrap_err().to_string();
        assert!(e.contains("is not a file"), "{e}");
        assert_eq!(status(&mut s)["job"], Value::Null, "a refused file starts nothing");
        // A file of the right size with other contents is refused on the install's thread.
        assert_eq!(install(&mut s, file("wrong.task", b"abd")).unwrap().0, json!({"model": "tiny", "installing": true, "bytes": 3}));
        s.wait_face_model_job();
        let notices = s.take_notices();
        assert_eq!(notices.iter().map(|n| n.template).collect::<Vec<_>>(), ["Face tracking model {model} could not be installed: {error}"]);
        assert!(notices[0].error && notices[0].text().contains("SHA-256"), "{}", notices[0].text());
        assert!(!models_dir.join("tiny").join("tiny.task").exists());
        assert!(!models_dir.join("tiny").join("tiny.task.part").exists());
        assert!(status(&mut s)["failure"].as_str().is_some_and(|f| f.contains("SHA-256")));
        // The right file is installed, and the failure is forgotten.
        install(&mut s, file("good.task", b"abc")).unwrap();
        s.wait_face_model_job();
        assert_eq!(templates(&mut s), ["Face tracking model {model} installed"]);
        let st = status(&mut s);
        assert_eq!((st["models"][0]["installed"].clone(), st["failure"].clone()), (json!(true), Value::Null));
        assert_eq!(std::fs::read(models_dir.join("tiny").join("tiny.task")).unwrap(), b"abc");
    }

    #[test]
    fn closing_the_studio_stops_a_download_without_waiting_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let (url, go, server) = serve(b"abc");
        let mut s = headless();
        s.set_models_dir(dir.path().to_path_buf());
        s.set_face_model_catalog(tiny(url));
        s.run("avatar.model_download", &json!({})).unwrap();
        // The server still holds the body back, so a close that waited for the download would
        // not return.
        s.finish_face_models_blocking();
        assert_eq!(status(&mut s)["job"], Value::Null);
        // Another studio on the same models folder cannot start a job whose part file the stopped
        // download would delete when it ends.
        let mut later = headless();
        later.set_models_dir(dir.path().to_path_buf());
        later.set_face_model_catalog(models_of(&s));
        let e = later.run("avatar.model_download", &json!({})).unwrap_err().to_string();
        assert!(e.contains("has not ended yet"), "{e}");
        go.send(()).unwrap();
        server.join().unwrap();
    }

    fn models_of(s: &Studio) -> &'static [ModelInfo] {
        s.face_models.catalog
    }
}
