//! MCP resources: the scene collection and the command list as JSON, and the templates that read
//! one scene, source, command or plug-in kind at a time.

use craft_jsonrpc::RpcError;
use serde_json::{Value, json};

use crate::backend::{Backend, McpError};

pub const DOCUMENT_URI: &str = "studiocraft://document";
pub const COMMANDS_URI: &str = "studiocraft://commands";

/// The most entries read from one catalog or collection list.
const MAX_CATALOG: usize = 20_000;

/// A list the completion values and template lookups are read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Live {
    /// Every command id (`engine.commands`).
    Commands,
    /// Scene names in the scene collection.
    Scenes,
    /// Source names in the scene collection.
    Sources,
    /// Source, filter and transition plug-in ids (`plugin.list`).
    Plugins,
}

/// One resource template: a URI with one `{variable}`.
pub struct Template {
    pub uri: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub live: Live,
}

/// Every template, in `resources/templates/list` order.
pub static TEMPLATES: &[Template] = &[
    Template {
        uri: "studiocraft://scene/{id}",
        name: "scene",
        title: "One scene",
        description: "The scene with this id or name: its items with their transforms, visibility and the name of the source each one \
                      shows, and whether it is the program or preview scene.",
        live: Live::Scenes,
    },
    Template {
        uri: "studiocraft://source/{id}",
        name: "source",
        title: "One source",
        description: "The source with this id or name, with its settings, filters, audio state and the names of the scenes that place it.",
        live: Live::Sources,
    },
    Template {
        uri: "studiocraft://command/{id}",
        name: "command",
        title: "One command",
        description: "The command with this id: label, parameters, undo, scope and whether it can run now (engine.commands).",
        live: Live::Commands,
    },
    Template {
        uri: "studiocraft://plugin/{kind}",
        name: "plugin",
        title: "One plug-in kind",
        description: "The source, filter or transition plug-in with this id, with its name, version and settings schema (plugin.list).",
        live: Live::Plugins,
    },
];

/// The `resources/list` result.
pub fn list() -> Value {
    json!({"resources": [
        {
            "uri": DOCUMENT_URI,
            "name": "document",
            "title": "Scene collection",
            "description": "The scene collection with every setting, studio mode, outputs, statistics and canvas (document.inspect with detail full).",
            "mimeType": "application/json",
        },
        {
            "uri": COMMANDS_URI,
            "name": "commands",
            "title": "Commands",
            "description": "Every command: id, label, parameters, undo, scope and whether it can run now (engine.commands).",
            "mimeType": "application/json",
        },
    ]})
}

/// The `resources/templates/list` result.
pub fn templates() -> Value {
    let list: Vec<Value> = TEMPLATES
        .iter()
        .map(|t| json!({"uriTemplate": t.uri, "name": t.name, "title": t.title, "description": t.description, "mimeType": "application/json"}))
        .collect();
    json!({ "resourceTemplates": list })
}

/// The text before a template's `{`.
fn prefix(template: &str) -> &str {
    template.split_once('{').map_or(template, |(p, _)| p)
}

/// The template `uri` is an instance of. The template itself (`studiocraft://scene/{id}`) counts
/// as an instance.
pub fn template_of(uri: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| uri.starts_with(prefix(t.uri)))
}

/// The `resources/read` result for `params.uri`. An unknown URI, or a template value that names
/// nothing, is error -32002.
pub fn read(b: &mut dyn Backend, params: Option<&Value>) -> Result<Value, RpcError> {
    let uri = params.and_then(|p| p.get("uri")).and_then(Value::as_str).ok_or_else(|| RpcError::new(-32602, "`uri` must be a string"))?;
    let not_found = || RpcError::new(-32002, format!("Resource not found: {uri}"));
    let failed = |e: McpError| RpcError::new(-32603, e.to_string());
    let value = match uri {
        DOCUMENT_URI => b.call("document.inspect", json!({"detail": "full"})).map_err(failed)?,
        COMMANDS_URI => b.call("engine.commands", json!({})).map_err(failed)?,
        other => {
            let template = template_of(other).ok_or_else(not_found)?;
            let var = decode(other.get(prefix(template.uri).len()..).unwrap_or_default());
            if var.is_empty() {
                return Err(not_found());
            }
            let found = match template.live {
                Live::Scenes => scene(&b.call("document.inspect", json!({})).map_err(failed)?, &var),
                Live::Sources => source(&b.call("document.inspect", json!({"detail": "full"})).map_err(failed)?, &var),
                Live::Commands => find(entries(&b.call("engine.commands", json!({})).map_err(failed)?), "id", &var).cloned(),
                Live::Plugins => plugin(&plugin_list(b).map_err(failed)?, &var),
            };
            found.ok_or_else(not_found)?
        }
    };
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]}))
}

/// The values a template's variable or a prompt argument can take, read fresh from the backend.
/// A backend that cannot answer gives none.
pub(crate) fn live_values(b: &mut dyn Backend, live: Live) -> Vec<String> {
    let names = |v: &Value, key: &str| -> Vec<String> {
        entries(v.get("collection").and_then(|c| c.get(key)).unwrap_or(&Value::Null))
            .iter()
            .filter_map(|e| e.get("name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect()
    };
    match live {
        Live::Commands => b.call("engine.commands", json!({})).map(|v| ids(entries(&v))).unwrap_or_default(),
        Live::Scenes => b.call("document.inspect", json!({})).map(|v| names(&v, "scenes")).unwrap_or_default(),
        Live::Sources => b.call("document.inspect", json!({})).map(|v| names(&v, "sources")).unwrap_or_default(),
        Live::Plugins => plugin_list(b)
            .map(|v| PLUGIN_TYPES.iter().flat_map(|(key, _)| ids(entries(v.get(*key).unwrap_or(&Value::Null)))).collect())
            .unwrap_or_default(),
    }
}

/// The plug-in lists a `plugin/{kind}` resource reads, with the type each entry is reported as.
const PLUGIN_TYPES: [(&str, &str); 3] = [("sources", "source"), ("filters", "filter"), ("transitions", "transition")];

/// The registry's plug-ins. `plugin.list` is an engine command, so it reaches both backends.
fn plugin_list(b: &mut dyn Backend) -> Result<Value, McpError> {
    b.call("engine.execute", json!({"command": "plugin.list", "params": {}}))
}

/// The entries of a JSON array, at most [`MAX_CATALOG`] of them.
fn entries(v: &Value) -> &[Value] {
    let all = v.as_array().map_or(&[][..], Vec::as_slice);
    all.get(..MAX_CATALOG).unwrap_or(all)
}

fn ids(list: &[Value]) -> Vec<String> {
    list.iter().filter_map(|e| e.get("id").and_then(Value::as_str)).map(str::to_owned).collect()
}

fn find<'a>(list: &'a [Value], field: &str, value: &str) -> Option<&'a Value> {
    list.iter().find(|e| e.get(field).and_then(Value::as_str) == Some(value))
}

/// The entry whose numeric `id` is `var`, or else the one whose `name` is `var`.
fn by_id_or_name<'a>(list: &'a [Value], var: &str) -> Option<&'a Value> {
    let by_id = var.parse::<u64>().ok().and_then(|n| list.iter().find(|e| e.get("id").and_then(Value::as_u64) == Some(n)));
    by_id.or_else(|| find(list, "name", var))
}

fn collection_list<'a>(state: &'a Value, key: &str) -> &'a [Value] {
    entries(state.get("collection").and_then(|c| c.get(key)).unwrap_or(&Value::Null))
}

fn scene(state: &Value, var: &str) -> Option<Value> {
    let sources = collection_list(state, "sources");
    let mut found = by_id_or_name(collection_list(state, "scenes"), var)?.clone();
    let id = found.get("id").cloned().unwrap_or(Value::Null);
    if let Some(items) = found.get_mut("items").and_then(Value::as_array_mut) {
        for item in items.iter_mut() {
            let source = item.get("source").and_then(Value::as_u64);
            let name = sources
                .iter()
                .find(|s| source.is_some() && s.get("id").and_then(Value::as_u64) == source)
                .and_then(|s| s.get("name"))
                .cloned()
                .unwrap_or(Value::Null);
            if let Some(o) = item.as_object_mut() {
                o.insert("source_name".into(), name);
            }
        }
    }
    let studio_mode = state.get("studio_mode").and_then(Value::as_bool).unwrap_or(false);
    if let Some(o) = found.as_object_mut() {
        o.insert("program".into(), json!(state.get("program_scene") == Some(&id)));
        o.insert("preview".into(), json!(studio_mode && state.get("preview_scene") == Some(&id)));
    }
    Some(found)
}

fn source(state: &Value, var: &str) -> Option<Value> {
    let mut found = by_id_or_name(collection_list(state, "sources"), var)?.clone();
    let id = found.get("id").and_then(Value::as_u64);
    let scenes: Vec<Value> = collection_list(state, "scenes")
        .iter()
        .filter(|sc| entries(sc.get("items").unwrap_or(&Value::Null)).iter().any(|i| i.get("source").and_then(Value::as_u64) == id))
        .filter_map(|sc| sc.get("name").cloned())
        .collect();
    let global = collection_list(state, "global_audio").iter().any(|g| g.as_u64() == id);
    if let Some(o) = found.as_object_mut() {
        o.insert("scenes".into(), Value::Array(scenes));
        o.insert("global_audio".into(), json!(global));
    }
    Some(found)
}

fn plugin(list: &Value, kind: &str) -> Option<Value> {
    PLUGIN_TYPES.iter().find_map(|(key, kind_type)| {
        let mut found = find(entries(list.get(*key).unwrap_or(&Value::Null)), "id", kind)?.clone();
        if let Some(o) = found.as_object_mut() {
            o.insert("type".into(), json!(kind_type));
        }
        Some(found)
    })
}

/// Percent-decoding, so a scene called "Be right back" can be read as `Be%20right%20back`. A `%`
/// not followed by two hexadecimal digits stays as it is.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let byte = if b == b'%' { bytes.get(i.saturating_add(1)..i.saturating_add(3)).and_then(hex_byte) } else { None };
        match byte {
            Some(v) => {
                out.push(v);
                i = i.saturating_add(3);
            }
            None => {
                out.push(b);
                i = i.saturating_add(1);
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_byte(h: &[u8]) -> Option<u8> {
    let digit = |c: u8| char::from(c).to_digit(16).and_then(|d| u8::try_from(d).ok());
    match h {
        [hi, lo] => Some(digit(*hi)?.checked_mul(16)?.checked_add(digit(*lo)?)?),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_reads_two_hex_digits_only() {
        assert_eq!(decode("Be%20right%20back"), "Be right back");
        assert_eq!(decode("50%25"), "50%");
        assert_eq!(decode("a%+1b"), "a%+1b");
        assert_eq!(decode("a%4"), "a%4");
        assert_eq!(decode("%"), "%");
    }

    #[test]
    fn uris_find_their_template() {
        assert_eq!(template_of("studiocraft://scene/3").map(|t| t.live), Some(Live::Scenes));
        assert_eq!(template_of("studiocraft://plugin/{kind}").map(|t| t.live), Some(Live::Plugins));
        assert!(template_of("studiocraft://document").is_none());
    }
}
