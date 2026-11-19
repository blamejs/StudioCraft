//! The running applications that have audio.

use serde::Serialize;

use crate::AppKey;

/// One running application with audio.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AppInfo {
    pub key: AppKey,
    /// The name to show: the program file's name without its extension.
    pub name: String,
    /// The program file's full path, or empty when the system does not say.
    pub path: String,
    /// The processes of this application that have audio, lowest first.
    pub pids: Vec<u32>,
    /// The process a capture targets: the topmost process in the parent chain whose program file
    /// has the same name, so capturing it with its child processes includes every process of the
    /// application.
    pub target_pid: u32,
    /// Whether one of the application's audio sessions is active: a stream is running, which may
    /// be paused or silent. The level is in the peak row.
    pub playing: bool,
    /// The `app` value ([`AppInfo::choice`]) of another listed application whose capture already
    /// records the process tree a capture of this one takes, because `target_pid` is a child
    /// process of that application's target. Capturing both records that tree twice. Other
    /// instances of this application, in trees of their own, are not part of it.
    pub within: Option<String>,
    /// The application's row in the [`crate::PeakTable`], or `None` past the table's last row.
    pub slot: Option<u8>,
    /// [`AppKey::id`] of `key`, worked out once when the row is assigned, so meter reads do not
    /// encode the key again. 0 before a row is assigned.
    #[serde(skip)]
    pub key_id: u64,
}

/// The applications one scan found.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AppList {
    /// Goes up with every list that differs from the one before. 0 is the empty list before the
    /// first scan.
    pub generation: u64,
    pub apps: Vec<AppInfo>,
    /// Why the applications could not be listed. English.
    pub error: Option<String>,
}

impl AppInfo {
    /// The value a source saves for this application: the key with the program's path, so the
    /// choice stays with this program when another one with the same file name starts.
    pub fn choice(&self) -> String {
        self.key.clone().with_path(&self.path).encode()
    }
}

impl AppList {
    /// The application `key` names. A key with a path finds the application at that path; when
    /// none is there, and for a key without a path, it finds the one application with that file
    /// name, so a program that moved after an update is still found. When several applications
    /// have the file name and none has the key's path, it finds none rather than guess.
    pub fn find(&self, key: &AppKey) -> Option<&AppInfo> {
        if key.path.is_some()
            && let Some(a) = self.apps.iter().find(|a| key.matches(&a.key, &a.path))
        {
            return Some(a);
        }
        let mut named = self.apps.iter().filter(|a| a.key.name == key.name);
        match (named.next(), named.next()) {
            (Some(a), None) => Some(a),
            _ => None,
        }
    }

    /// Sort by name, and keep keys unique: when two listed applications have program files of the
    /// same name in different folders, each key gets its path. The order does not depend on which
    /// applications play, so a list someone is choosing from does not reorder when a stream
    /// starts or stops.
    pub fn arrange(apps: &mut [AppInfo]) {
        let mut names: Vec<&str> = apps.iter().map(|a| a.key.name.as_str()).collect();
        names.sort_unstable();
        let repeated: Vec<String> = names.windows(2).filter(|w| w[0] == w[1]).map(|w| w[0].to_string()).collect();
        for a in apps.iter_mut() {
            if repeated.contains(&a.key.name) && a.key.path.is_none() {
                let path = a.path.clone();
                a.key = a.key.clone().with_path(&path);
            }
        }
        apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.key.cmp(&b.key)));
    }
}
