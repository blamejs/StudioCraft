//! Checks over the files of the repository: asset attribution and product names.

use std::path::{Path, PathBuf};

/// Folders never scanned.
const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules", ".cargo"];

/// Folders at the top of `root` never scanned: the craft-fonts and craft-i18n checkouts that CI
/// makes inside the workspace (also listed in `.gitignore`).
const SKIP_ROOT_DIRS: &[&str] = &["craft-fonts", "craft-i18n"];

/// Every file under `root`, relative, with `/` separators.
pub fn files(root: &Path) -> Vec<String> {
    walk_all(root).0
}

/// Every file under `root`, or an error naming each folder or entry that could not be read.
pub fn try_files(root: &Path) -> Result<Vec<String>, String> {
    let (out, errors) = walk_all(root);
    if errors.is_empty() { Ok(out) } else { Err(format!("could not read:\n  {}", errors.join("\n  "))) }
}

fn walk_all(root: &Path) -> (Vec<String>, Vec<String>) {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, errors: &mut Vec<String>) {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => return errors.push(format!("{}: {e}", dir.display())),
        };
        for e in rd {
            let e = match e {
                Ok(e) => e,
                Err(err) => {
                    errors.push(format!("{}: {err}", dir.display()));
                    continue;
                }
            };
            let p: PathBuf = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) && !(dir == root && SKIP_ROOT_DIRS.contains(&name.as_str())) {
                    walk(root, &p, out, errors);
                }
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    let mut errors = Vec::new();
    walk(root, root, &mut out, &mut errors);
    out.sort();
    (out, errors)
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

/// Product names the app's text and docs must not use, matched as whole words: other streaming and
/// recording software, then the streaming services of the preset list.
pub const BRANDS: &[&str] = &[
    "OBS",
    "obs-studio",
    "obsproject",
    "Open Broadcaster",
    "Streamlabs",
    "XSplit",
    "vMix",
    "Wirecast",
    "Twitch",
    "YouTube",
    "Facebook",
    "Kick",
    "LinkedIn",
    "TikTok",
];

/// Files allowed to name other products: the mechanics tables that cite where a value came from,
/// and this check's own list.
const BRAND_EXEMPT: &[&str] = &["MECHANICS.md", "mechanics.json", "xtask/src/scan.rs"];

/// The service names each file may use (AGENTS.md 1.3): the preset list of the streaming services
/// and the `ASSETS.md` rows of their logos. Every other name in these files is still refused.
const BRAND_ALLOWED: &[(&str, &[&str])] = &[("crates/model/src/services.rs", SERVICE_NAMES), ("ASSETS.md", SERVICE_NAMES)];

/// The streaming services of the preset list, as [`BRANDS`] writes them.
const SERVICE_NAMES: &[&str] = &["Twitch", "YouTube", "Facebook", "Kick", "LinkedIn", "TikTok"];

/// Whether `file` may name `brand`.
fn brand_allowed(file: &str, brand: &str) -> bool {
    BRAND_ALLOWED.iter().any(|(f, names)| *f == file && names.contains(&brand))
}

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
        problems.extend(file_brand_problems(&f, &text));
    }
    problems
}

/// The product names in the text of `file` that it may not use, as `file:line: name`.
fn file_brand_problems(file: &str, text: &str) -> Vec<String> {
    brand_hits(text).into_iter().filter(|(_, name)| !brand_allowed(file, name)).map(|(line, name)| format!("{file}:{line}: {name}")).collect()
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

    #[test]
    fn service_names_pass_only_in_the_files_allowed_to_use_them() {
        let text = "Stream to Twitch\nGo live on YouTube";
        assert_eq!(file_brand_problems("crates/model/src/services.rs", text), Vec::<String>::new());
        assert_eq!(file_brand_problems("ASSETS.md", text), Vec::<String>::new());
        assert_eq!(
            file_brand_problems("crates/ui-egui/src/logos.rs", text),
            ["crates/ui-egui/src/logos.rs:1: Twitch", "crates/ui-egui/src/logos.rs:2: YouTube"]
        );
        assert_eq!(file_brand_problems("README.md", "Works with TikTok"), ["README.md:1: TikTok"]);
        // The allowed files still name no other product.
        assert_eq!(file_brand_problems("crates/model/src/services.rs", "a\nlike OBS"), ["crates/model/src/services.rs:2: OBS"]);
        assert_eq!(file_brand_problems("ASSETS.md", "From Streamlabs"), ["ASSETS.md:1: Streamlabs"]);
        // Lowercase ids and the words inside other words are not names.
        assert!(file_brand_problems("crates/ui-egui/src/logos.rs", "\"twitch\" kicked Kickstart").is_empty());
        assert_eq!(file_brand_problems("crates/ui-egui/src/logos.rs", "a Kick drum").len(), 1, "Kick as written is a name");
    }

    #[test]
    fn the_preset_list_names_every_allowed_service() {
        let presets = std::fs::read_to_string(crate::root().join("crates/model/src/services.rs")).unwrap();
        for name in SERVICE_NAMES {
            assert!(brand_hits(&presets).iter().any(|(_, n)| n == name), "the preset list names {name}");
        }
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

    #[test]
    fn the_scan_skips_the_ci_checkouts_and_build_output() {
        let root = std::env::temp_dir().join(format!("studiocraft-xtask-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for f in ["assets/icon.png", "craft-fonts/fonts/A.ttf", "craft-i18n/de.tsv", "target/debug/b.png", "docs/craft-fonts/c.png"] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        let listed = files(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(listed, ["assets/icon.png", "docs/craft-fonts/c.png"]);
    }
}
