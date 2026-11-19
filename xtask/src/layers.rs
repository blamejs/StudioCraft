//! Crate layering (docs/architecture.md › Layers): a crate may depend on workspace crates of a
//! lower layer only. Dev-dependencies may also reach the same layer, for tests.

use serde_json::Value;

/// Where a workspace crate sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Layer(u8),
    /// Binaries and tooling: exempt.
    Exempt,
}

/// The layer table. Names are package names without the `studiocraft-` prefix.
pub const TABLE: &[(&str, Class)] = &[
    ("model", Class::Layer(0)),
    ("media", Class::Layer(0)),
    // Shared crates built here before they move to craft-libs.
    ("craft-tsmux", Class::Layer(0)),
    ("craft-flv", Class::Layer(0)),
    ("craft-packet", Class::Layer(0)),
    ("craft-color-transform", Class::Layer(0)),
    ("craft-scopes", Class::Layer(0)),
    ("craft-audio-dsp", Class::Layer(0)),
    ("craft-ease", Class::Layer(0)),
    ("craft-gpu-device", Class::Layer(0)),
    ("craft-gpu-yuv", Class::Layer(0)),
    ("craft-hwenc", Class::Layer(0)),
    ("craft-hwenc-native", Class::Layer(0)),
    ("plugin", Class::Layer(1)),
    ("craft-rtmp", Class::Layer(1)),
    ("engine", Class::Layer(2)),
    ("render", Class::Layer(2)),
    ("platform", Class::Layer(2)),
    ("encode", Class::Layer(2)),
    ("sources", Class::Layer(3)),
    ("outputs", Class::Layer(3)),
    ("audio-tracks", Class::Layer(3)),
    ("studio", Class::Layer(4)),
    ("ui-egui", Class::Layer(5)),
    ("mcp", Class::Layer(5)),
    // Apps and tooling.
    ("studiocraft", Class::Exempt),
    ("cli", Class::Exempt),
    ("xtask", Class::Exempt),
];

/// A workspace dependency edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub dev: bool,
}

fn short(name: &str) -> &str {
    if name == "studiocraft" { name } else { name.strip_prefix("studiocraft-").unwrap_or(name) }
}

fn class(name: &str) -> Option<Class> {
    TABLE.iter().find(|(n, _)| *n == short(name)).map(|(_, c)| *c)
}

/// The rule violations among `crates` and `edges`.
pub fn check(crates: &[String], edges: &[Edge]) -> Vec<String> {
    let mut problems = Vec::new();
    for c in crates {
        if class(c).is_none() {
            problems.push(format!("`{c}` is not in the layer table (xtask/src/layers.rs)"));
        }
    }
    for e in edges {
        let (Some(Class::Layer(a)), Some(to)) = (class(&e.from), class(&e.to)) else { continue };
        match to {
            Class::Exempt => problems.push(format!("`{}` depends on the app or tool `{}`", e.from, e.to)),
            Class::Layer(b) if b > a || (b == a && !e.dev) => {
                problems.push(format!("`{}` (layer {a}) depends on `{}` (layer {b}){}", e.from, e.to, if e.dev { " for tests" } else { "" }));
            }
            Class::Layer(_) => {}
        }
    }
    problems
}

pub fn from_metadata(meta: &Value) -> (Vec<String>, Vec<Edge>) {
    let packages = meta["packages"].as_array().cloned().unwrap_or_default();
    let names: Vec<String> = packages.iter().filter_map(|p| p["name"].as_str().map(String::from)).collect();
    let mut edges = Vec::new();
    for p in &packages {
        let Some(from) = p["name"].as_str() else { continue };
        for d in p["dependencies"].as_array().into_iter().flatten() {
            let Some(to) = d["name"].as_str() else { continue };
            if names.iter().any(|n| n == to) {
                edges.push(Edge { from: from.into(), to: to.into(), dev: d["kind"].as_str() == Some("dev") });
            }
        }
    }
    (names, edges)
}

pub fn run() -> Result<(), String> {
    eprintln!("$ layers");
    let out = crate::cargo().args(["metadata", "--format-version", "1", "--no-deps"]).output().map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!("cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    let meta: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata: {e}"))?;
    let (crates, edges) = from_metadata(&meta);
    let problems = check(&crates, &edges);
    if problems.is_empty() { Ok(()) } else { Err(format!("layering:\n  {}", problems.join("\n  "))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(from: &str, to: &str, dev: bool) -> Edge {
        Edge { from: from.into(), to: to.into(), dev }
    }

    #[test]
    fn lower_layers_only() {
        let crates: Vec<String> =
            ["studiocraft-model", "studiocraft-engine", "studiocraft-render", "studiocraft-sources", "studiocraft"].map(String::from).to_vec();
        assert!(check(&crates, &[e("studiocraft-engine", "studiocraft-model", false), e("studiocraft", "studiocraft-engine", false)]).is_empty());
        assert_eq!(check(&crates, &[e("studiocraft-model", "studiocraft-engine", false)]).len(), 1);
        assert_eq!(check(&crates, &[e("studiocraft-engine", "studiocraft-render", false)]).len(), 1, "same layer");
        assert!(check(&crates, &[e("studiocraft-engine", "studiocraft-render", true)]).is_empty(), "same layer, tests only");
        assert_eq!(check(&crates, &[e("studiocraft-engine", "studiocraft", false)]).len(), 1);
        assert_eq!(check(&["studiocraft-new".to_string()], &[]).len(), 1, "every crate is placed");
    }

    #[test]
    fn the_mcp_server_sits_beside_the_window() {
        assert_eq!(class("studiocraft-mcp"), Some(Class::Layer(5)));
        let crates: Vec<String> = ["studiocraft-studio", "studiocraft-ui-egui", "studiocraft-mcp", "studiocraft-cli"].map(String::from).to_vec();
        assert!(check(&crates, &[e("studiocraft-mcp", "studiocraft-studio", false), e("studiocraft-cli", "studiocraft-mcp", false)]).is_empty());
        assert_eq!(check(&crates, &[e("studiocraft-mcp", "studiocraft-ui-egui", false)]).len(), 1, "same layer");
        assert_eq!(check(&crates, &[e("studiocraft-studio", "studiocraft-mcp", false)]).len(), 1, "lower layer");
        assert_eq!(check(&crates, &[e("studiocraft-mcp", "studiocraft-cli", false)]).len(), 1, "app");
    }
}
