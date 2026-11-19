//! Which application a lane or a capture means, kept across restarts of the application.

use serde::{Deserialize, Serialize};

/// Longest program file name, in bytes.
const MAX_KEY_BYTES: usize = 1024;
/// Longest program path, in bytes: Windows' longest path, 32,767 UTF-16 units, at 3 bytes each.
const MAX_PATH_BYTES: usize = 32_767 * 3;

/// An application, named so the name survives the application quitting and starting again with a
/// new process id. On Windows it is the program file's name in lower case (`player.exe`). `path`,
/// the program file's full path in lower case, is set only to tell apart two running applications
/// whose files have the same name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AppKey {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl AppKey {
    /// The key of a program file, from its full path or its file name. `None` for an empty name.
    pub fn from_exe(exe: &str) -> Option<AppKey> {
        let name = exe.rsplit(['\\', '/']).next().unwrap_or(exe).trim().to_lowercase();
        (!name.is_empty() && name.len() <= MAX_KEY_BYTES).then_some(AppKey { name, path: None })
    }

    /// The key with the program file's full path, to tell it apart from another file of the same
    /// name. A path longer than any Windows path is left out.
    pub fn with_path(mut self, path: &str) -> AppKey {
        let path = path.trim().to_lowercase();
        self.path = (!path.is_empty() && path.len() <= MAX_PATH_BYTES).then_some(path);
        self
    }

    /// The key as one settings value: the name, or `name|path`.
    pub fn encode(&self) -> String {
        match &self.path {
            Some(p) => format!("{}|{p}", self.name),
            None => self.name.clone(),
        }
    }

    /// A key from [`AppKey::encode`]'s text, which reads every key `encode` writes. `None` for any
    /// other text: an empty or oversized one, a name with a folder in it, or a `|` with no path
    /// after it.
    pub fn decode(text: &str) -> Option<AppKey> {
        if text.len() > MAX_KEY_BYTES + 1 + MAX_PATH_BYTES {
            return None;
        }
        let (name, path) = match text.split_once('|') {
            Some((name, path)) => (name, Some(path)),
            None => (text, None),
        };
        if name.contains(['\\', '/']) {
            return None;
        }
        let key = AppKey::from_exe(name)?;
        match path {
            None => Some(key),
            // A path no Windows program has, measured after lower-casing, is refused rather than
            // read as the name alone, and so is an empty one.
            Some(path) => Some(key.with_path(path)).filter(|k| k.path.is_some()),
        }
    }

    /// Whether an application listed as `other` is this one: the same name, and the same path when
    /// this key has one, compared after the lower-casing [`AppKey::with_path`] applies.
    pub fn matches(&self, other: &AppKey, other_path: &str) -> bool {
        self.name == other.name && self.path.as_ref().is_none_or(|p| *p == other_path.trim().to_lowercase())
    }

    /// A number for the key, the same on every run (64-bit FNV-1a of the encoded key), never 0.
    pub fn id(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.encode().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h.max(1)
    }
}

impl std::fmt::Display for AppKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}
