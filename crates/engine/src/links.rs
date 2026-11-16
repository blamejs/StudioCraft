//! Community and project links and the app's identity (Help menu, About, CLI, MCP).

use serde_json::{Value, json};

/// The app's slug.
pub const APP: &str = "studiocraft";
/// The ArtCraft community Discord.
pub const DISCORD: &str = "https://discord.gg/artcraft";
/// The ArtCraft website.
pub const WEBSITE: &str = "https://getartcraft.com";
/// StudioCraft's source repository.
pub const GITHUB: &str = "https://github.com/blamejs/StudioCraft";
/// Where to report bugs and request features.
pub const ISSUES: &str = "https://github.com/blamejs/StudioCraft/issues";

/// Every link key, in menu order.
pub const KEYS: [&str; 4] = ["discord", "website", "github", "issues"];

/// All links as JSON (the `app.links` command without `link`).
pub fn all() -> Value {
    json!({
        "discord": DISCORD,
        "website": WEBSITE,
        "github": GITHUB,
        "issues": ISSUES,
    })
}

/// A link by key (`discord`, `website`, `github`, `issues`).
pub fn get(key: &str) -> Option<&'static str> {
    Some(match key {
        "discord" => DISCORD,
        "website" => WEBSITE,
        "github" => GITHUB,
        "issues" => ISSUES,
        _ => return None,
    })
}

/// The app's name, id, version, license and number of listed contributors (the `app.info`
/// command).
pub fn info() -> Value {
    json!({
        "name": "StudioCraft",
        "id": APP,
        "version": env!("CARGO_PKG_VERSION"),
        "license": "MIT OR Apache-2.0",
        "contributors": crate::credits::CONTRIBUTORS.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_follow_the_app_slug() {
        assert!(GITHUB.to_ascii_lowercase().ends_with(&format!("/{APP}")));
        assert!(ISSUES.starts_with(GITHUB));
        assert_eq!(all().as_object().map(|o| o.len()), Some(KEYS.len()));
        for k in KEYS {
            assert!(get(k).is_some_and(|u| u.starts_with("https://")), "{k}");
            assert_eq!(all()[k], get(k).unwrap());
        }
        assert_eq!(get("nope"), None);
        assert_eq!(info()["id"], APP);
        assert_eq!(info()["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(info()["contributors"], crate::credits::CONTRIBUTORS.len());
    }
}
