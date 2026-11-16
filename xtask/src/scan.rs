//! Checks over the files of the repository: asset attribution and product names.

use std::path::{Path, PathBuf};

/// Folders never scanned.
const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules", ".cargo"];

/// Every file under `root`, relative, with `/` separators.
pub fn files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p: PathBuf = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    walk(root, &p, out);
                }
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

const ASSET_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "icns", "svg", "ttf", "otf", "woff", "woff2", "wav", "mp3", "flac", "ogg",
    "mp4", "mkv", "mov", "webm",
];

fn ext(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Asset files not listed in `ASSETS.md` as `` `path` ``.
pub fn unlisted_assets(files: &[String], assets_md: &str) -> Vec<String> {
    files.iter().filter(|f| ASSET_EXT.contains(&ext(f).as_str())).filter(|f| !assets_md.contains(&format!("`{f}`"))).cloned().collect()
}

pub fn assets(root: &Path) -> Result<(), String> {
    eprintln!("$ assets");
    let md = std::fs::read_to_string(root.join("ASSETS.md")).map_err(|e| format!("ASSETS.md: {e}"))?;
    let missing = unlisted_assets(&files(root), &md);
    if missing.is_empty() { Ok(()) } else { Err(format!("files missing from ASSETS.md:\n  {}", missing.join("\n  "))) }
}

/// Product names the app's text and docs must not use, matched as whole words.
pub const BRANDS: &[&str] = &["OBS", "obs-studio", "obsproject", "Open Broadcaster", "Streamlabs", "XSplit", "vMix", "Wirecast"];

/// Files allowed to name other products: the mechanics tables that cite where a value came from,
/// and this check's own list.
const BRAND_EXEMPT: &[&str] = &["MECHANICS.md", "mechanics.json", "xtask/src/scan.rs"];

const TEXT_EXT: &[&str] = &["rs", "md", "toml", "json", "yml", "yaml", "txt", "wgsl", "tsv"];

fn word_at(text: &str, i: usize, len: usize) -> bool {
    let before = text[..i].chars().next_back();
    let after = text[i + len..].chars().next();
    let edge = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
    edge(before) && edge(after)
}

/// The brand names found in `text`, as `(line, name)`.
pub fn brand_hits(text: &str) -> Vec<(usize, &'static str)> {
    let mut hits = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for b in BRANDS {
            let mut from = 0;
            while let Some(i) = line[from..].find(b) {
                let at = from + i;
                if word_at(line, at, b.len()) {
                    hits.push((n + 1, *b));
                }
                from = at + b.len();
            }
        }
    }
    hits
}

pub fn brands(root: &Path) -> Result<(), String> {
    eprintln!("$ brands");
    let problems = brand_problems(root);
    if problems.is_empty() { Ok(()) } else { Err(format!("other products named:\n  {}", problems.join("\n  "))) }
}

/// Every product name in the text files under `root`, as `file:line: name`.
fn brand_problems(root: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    for f in files(root) {
        if BRAND_EXEMPT.contains(&f.as_str()) || f == "Cargo.lock" || !TEXT_EXT.contains(&ext(&f).as_str()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(&f)) else { continue };
        for (line, name) in brand_hits(&text) {
            problems.push(format!("{f}:{line}: {name}"));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_names_match_whole_words() {
        assert_eq!(brand_hits("Works like OBS does").len(), 1);
        assert!(brand_hits("observe the jobs, OBSERVER").is_empty());
        assert_eq!(brand_hits("a\nsee obs-studio here").first().map(|h| h.0), Some(2));
    }

    /// The MCP server's instructions, prompt texts and tool descriptions are string literals in
    /// `crates/mcp/src`.
    #[test]
    fn mcp_text_names_no_other_products() {
        let dir = crate::root().join("crates/mcp/src");
        assert!(files(&dir).iter().any(|f| f == "tools.rs"), "no MCP sources under {}", dir.display());
        assert_eq!(brand_problems(&dir), Vec::<String>::new());
    }

    #[test]
    fn assets_must_be_listed() {
        let files = vec!["assets/icon.png".to_string(), "src/main.rs".to_string()];
        assert_eq!(unlisted_assets(&files, "nothing"), ["assets/icon.png"]);
        assert!(unlisted_assets(&files, "- `assets/icon.png`: drawn for StudioCraft").is_empty());
    }
}
