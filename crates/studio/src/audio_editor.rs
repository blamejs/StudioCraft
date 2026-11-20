//! Opening a take's session in the audio editor, or its folder in the file manager.
//!
//! Each opening runs on a short thread of its own ("audio-tracks-open"), so looking the editor up
//! on the `PATH`, checking the editor path and starting a process never run on the studio's
//! thread. The thread ends once the program has started; a process started for the editor is
//! waited on by a thread of its own, so it leaves no zombie process behind. When the editor cannot
//! be found or started, the take's folder opens instead and the reason comes back for a notice.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The audio editor's program name, looked up on the `PATH` when no editor path is set.
pub const EDITOR_PROGRAM: &str = "soundcraft";

/// Starts programs for [`open_session`] and [`show_folder`]. The studio uses [`SystemLauncher`];
/// tests use their own.
pub trait Launcher: Send + Sync {
    /// Start `program` with `session` as its one argument. Returns once the program started.
    fn open_with(&self, program: &Path, session: &Path) -> std::io::Result<()>;
    /// Show `folder` in the file manager.
    fn show_folder(&self, folder: &Path) -> std::io::Result<()>;
    /// The file of the program `name` in the first `PATH` folder that holds it.
    fn find_on_path(&self, name: &str) -> Option<PathBuf>;
}

/// The launcher that starts real processes.
pub struct SystemLauncher;

impl Launcher for SystemLauncher {
    fn open_with(&self, program: &Path, session: &Path) -> std::io::Result<()> {
        let mut child = Command::new(program).arg(session).spawn()?;
        // The editor runs for as long as the user keeps it open; this thread only collects its
        // exit status.
        let _ = std::thread::Builder::new().name("audio-editor-wait".into()).spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    fn show_folder(&self, folder: &Path) -> std::io::Result<()> {
        studiocraft_platform::shell::open_folder(folder).map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn find_on_path(&self, name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let names: Vec<String> = if cfg!(windows) { vec![format!("{name}.exe"), name.to_string()] } else { vec![name.to_string()] };
        std::env::split_paths(&path).flat_map(|dir| names.iter().map(move |n| dir.join(n))).find(|p| p.is_file())
    }
}

/// What an opening did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Opened {
    /// The editor started with the session.
    Editor { program: PathBuf, session: PathBuf },
    /// The folder opened in the file manager: as asked, or with the reason the editor did not
    /// start.
    Folder { folder: PathBuf, editor_error: Option<String> },
}

/// The editor to start: the editor path when it is set, else the program found on the `PATH`.
/// The error says why there is none.
fn editor(launcher: &dyn Launcher, editor_path: &str) -> Result<PathBuf, String> {
    if editor_path.is_empty() {
        return launcher.find_on_path(EDITOR_PROGRAM).ok_or_else(|| "the audio editor is not on the PATH and no editor path is set".to_string());
    }
    let p = PathBuf::from(editor_path);
    if p.is_file() { Ok(p) } else { Err(format!("the editor path {editor_path} is not a file")) }
}

/// Open `session` in the audio editor ([`editor`]), or its folder when the editor cannot be
/// found or started. An error only when the folder could not be shown either.
pub fn open_session(launcher: &dyn Launcher, session: &Path, editor_path: &str) -> Result<Opened, String> {
    let tried = editor(launcher, editor_path).and_then(|program| match launcher.open_with(&program, session) {
        Ok(()) => Ok(program),
        Err(e) => Err(format!("{} could not be started: {e}", program.display())),
    });
    match tried {
        Ok(program) => Ok(Opened::Editor { program, session: session.to_path_buf() }),
        Err(why) => {
            let folder = session.parent().unwrap_or(session).to_path_buf();
            match launcher.show_folder(&folder) {
                Ok(()) => Ok(Opened::Folder { folder, editor_error: Some(why) }),
                Err(e) => Err(format!("{why}, and the folder {} could not be shown: {e}", folder.display())),
            }
        }
    }
}

/// Show the folder of `session` in the file manager.
pub fn show_folder(launcher: &dyn Launcher, session: &Path) -> Result<Opened, String> {
    let folder = session.parent().unwrap_or(session).to_path_buf();
    match launcher.show_folder(&folder) {
        Ok(()) => Ok(Opened::Folder { folder, editor_error: None }),
        Err(e) => Err(format!("the folder {} could not be shown: {e}", folder.display())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A launcher that records what it was asked to start and fails as it is told.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) on_path: Option<PathBuf>,
        pub(crate) editor_fails: bool,
        pub(crate) folder_fails: bool,
        pub(crate) log: Mutex<Vec<String>>,
    }

    impl Launcher for Fake {
        fn open_with(&self, program: &Path, session: &Path) -> std::io::Result<()> {
            if self.editor_fails {
                return Err(std::io::Error::other("no such program"));
            }
            self.log.lock().unwrap().push(format!("editor {} {}", program.display(), session.display()));
            Ok(())
        }

        fn show_folder(&self, folder: &Path) -> std::io::Result<()> {
            if self.folder_fails {
                return Err(std::io::Error::other("no file manager"));
            }
            self.log.lock().unwrap().push(format!("folder {}", folder.display()));
            Ok(())
        }

        fn find_on_path(&self, name: &str) -> Option<PathBuf> {
            assert_eq!(name, EDITOR_PROGRAM);
            self.on_path.clone()
        }
    }

    #[test]
    fn a_missing_editor_shows_the_folder_and_says_why() {
        let session = Path::new("Videos").join("Show Audio").join("Show.scraft");
        let folder = session.parent().unwrap().to_path_buf();
        let fake = Fake::default();
        let got = open_session(&fake, &session, "").unwrap();
        assert_eq!(
            got,
            Opened::Folder { folder: folder.clone(), editor_error: Some("the audio editor is not on the PATH and no editor path is set".into()) }
        );
        let missing = std::env::temp_dir().join(format!("studiocraft-no-editor-{}", std::process::id()));
        let Opened::Folder { editor_error: Some(why), .. } = open_session(&fake, &session, &missing.display().to_string()).unwrap() else {
            panic!("an editor path that is not a file shows the folder");
        };
        assert!(why.contains("is not a file"), "{why}");
        let failing = Fake { on_path: Some(PathBuf::from("/bin/editor")), editor_fails: true, ..Fake::default() };
        let Opened::Folder { editor_error: Some(why), .. } = open_session(&failing, &session, "").unwrap() else {
            panic!("an editor that does not start shows the folder");
        };
        assert!(why.contains("could not be started: no such program"), "{why}");
        let nothing = Fake { folder_fails: true, ..Fake::default() };
        let err = open_session(&nothing, &session, "").unwrap_err();
        assert!(err.contains("could not be shown: no file manager"), "{err}");
        assert_eq!(*fake.log.lock().unwrap(), vec![format!("folder {}", folder.display()); 2]);
    }

    #[test]
    fn the_editor_on_the_path_opens_the_session_unless_an_editor_path_is_set() {
        let session = Path::new("Show Audio").join("Show.scraft");
        let fake = Fake { on_path: Some(PathBuf::from("/opt/editor")), ..Fake::default() };
        let got = open_session(&fake, &session, "").unwrap();
        assert_eq!(got, Opened::Editor { program: PathBuf::from("/opt/editor"), session: session.clone() });
        // A set path wins over the PATH.
        let exe = std::env::current_exe().unwrap();
        let got = open_session(&fake, &session, &exe.display().to_string()).unwrap();
        assert_eq!(got, Opened::Editor { program: exe.clone(), session: session.clone() });
        assert_eq!(show_folder(&fake, &session).unwrap(), Opened::Folder { folder: PathBuf::from("Show Audio"), editor_error: None });
        assert_eq!(
            *fake.log.lock().unwrap(),
            [
                format!("editor /opt/editor {}", session.display()),
                format!("editor {} {}", exe.display(), session.display()),
                "folder Show Audio".into()
            ]
        );
    }
}
