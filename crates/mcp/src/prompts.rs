//! MCP prompts (`prompts/list`, `prompts/get`) and `completion/complete`.
//!
//! A prompt is a StudioCraft workflow: `prompts/get` returns one user message with the arguments
//! filled in. Completion values for prompt arguments and resource-template variables are read from
//! the backend each time, so they name the commands, scenes, sources and plug-ins that exist now.

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::resources::{self, Live};

/// The most values one `completion/complete` reply carries.
pub const MAX_VALUES: usize = 100;

/// Where one prompt argument's completion values come from.
#[derive(Clone, Copy)]
pub enum Values {
    /// A fixed list.
    Fixed(&'static [&'static str]),
    /// A list read from the backend.
    Live(Live),
}

/// One prompt argument.
pub struct PromptArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
    /// Put in place of `{name}` when the caller leaves the argument out.
    pub default: &'static str,
    pub values: Values,
}

/// One prompt: a workflow message with `{argument}` placeholders.
pub struct PromptDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub arguments: &'static [PromptArg],
    pub text: &'static str,
}

static NONE: &[&str] = &[];

/// Every prompt, in `prompts/list` order.
pub static PROMPTS: &[PromptDef] = &[
    PromptDef {
        name: "build_scene",
        title: "Build a scene",
        description: "Build a new scene from a description: add the scene, add its sources, place them on the canvas and check the result.",
        arguments: &[PromptArg {
            name: "description",
            description: "What the scene shows and how it is laid out",
            required: true,
            default: "",
            values: Values::Fixed(NONE),
        }],
        text: "\
Build a scene in StudioCraft.

Scene: {description}

Canvas coordinates are pixels, x to the right and y down, with the origin at the top-left.
doc_inspect reports the canvas size under `canvas`.

Steps:
1. scene_add with a short name taken from the description. The reply carries the new scene's id.
2. scene_select the new scene so that it is the scene on the program output.
3. source_add once for each element the description asks for, with `scene` set to the new scene,
a `kind`, a `name` and `settings`. command_list and the studiocraft://plugin/{kind} resource list
each plug-in kind with its settings and their defaults. Read them before choosing settings.
4. item_transform each item into place: `position` is the top-left corner, `scale` multiplies the
source's own size and `rotation` is in degrees. Items later in the scene are drawn on top.
5. render_preview and look at the result. Move or resize what is out of place, then look again.

Report the scene's id and the id and name of every source you added.",
    },
    PromptDef {
        name: "lower_third",
        title: "Add a lower third",
        description: "Add a text source to a scene and place it in the lower band of the canvas.",
        arguments: &[
            PromptArg { name: "text", description: "The words to show", required: true, default: "", values: Values::Fixed(NONE) },
            PromptArg {
                name: "color",
                description: "The text color as #rrggbbaa",
                required: false,
                default: "#ffffffff",
                values: Values::Fixed(&["#ffffffff", "#000000ff", "#ffd400ff", "#00b4ffff"]),
            },
            PromptArg {
                name: "scene",
                description: "The scene to add it to, by name or id",
                required: false,
                default: "the scene being edited",
                values: Values::Live(Live::Scenes),
            },
        ],
        text: "\
Add a lower third to {scene} in StudioCraft.

Text: {text}
Color: {color}

Steps:
1. doc_inspect to read the canvas width and height under `canvas` and the scene's id.
2. source_add with kind `text`, a name such as \"Lower third\", and settings with `text` set to the
words above and `color` set to {color}. Read studiocraft://plugin/text for the other settings, such
as `size`, `outline` and `background`.
3. item_transform the new item so that it sits in the lower band of the canvas: its top edge at
about 80 percent of the canvas height, and its left edge about 5 percent of the canvas width in
from the left. Keep the whole text inside the canvas, and lower `size` with source.settings if it
does not fit.
4. render_preview and check that the text is readable and inside the canvas.

Report the new source's id and name.",
    },
    PromptDef {
        name: "check_collection",
        title: "Check the scene collection",
        description: "Inspect the scene collection and report missing files, empty scenes and scenes whose items are all hidden.",
        arguments: &[],
        text: "\
Check the StudioCraft scene collection and report what needs attention. Do not change anything.

Steps:
1. Read studiocraft://document for the full scene collection with every source's settings.
2. Missing files: for each source, read studiocraft://plugin/{kind} for its kind and find the
settings fields of type `path`. List each source whose file setting names a file that does not
exist, or is empty when the source needs a file. When you have no way to check a file, list it as
unchecked.
3. Empty scenes: list each scene with no items.
4. Hidden-only scenes: list each scene that has items, none of which is visible.
5. Also note sources that no scene places and that are not global audio sources.

Report each finding with the scene or source name and id, grouped by kind. Say so plainly when a
check finds nothing.",
    },
];

/// The message text with the source's line breaks joined, keeping blank lines between paragraphs
/// and starting each numbered step on a line of its own.
fn flow(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| {
            let mut out = String::new();
            for line in paragraph.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if !out.is_empty() {
                    out.push(if is_step(line) { '\n' } else { ' ' });
                }
                out.push_str(line);
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A numbered step such as `2. scene_select ...`.
fn is_step(line: &str) -> bool {
    line.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.len() <= 2 && n.bytes().all(|c| c.is_ascii_digit()))
}

/// The `prompts/list` result.
pub fn list() -> Value {
    let prompts: Vec<Value> = PROMPTS
        .iter()
        .map(|p| {
            let arguments: Vec<Value> =
                p.arguments.iter().map(|a| json!({"name": a.name, "description": a.description, "required": a.required})).collect();
            json!({"name": p.name, "title": p.title, "description": p.description, "arguments": arguments})
        })
        .collect();
    json!({ "prompts": prompts })
}

/// The `prompts/get` result: the prompt's message with its arguments filled in. An unknown prompt,
/// a missing or empty required argument, or an argument the prompt does not take is an error.
pub fn get(name: &str, args: Option<&Value>) -> Result<Value, String> {
    let p = PROMPTS.iter().find(|p| p.name == name).ok_or_else(|| {
        let names: Vec<&str> = PROMPTS.iter().map(|p| p.name).collect();
        format!("there is no prompt `{name}`; use {}", names.join(", "))
    })?;
    let args = match args {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => return Err("`arguments` must be an object".into()),
    };
    if let Some(extra) = args.and_then(|m| m.keys().find(|k| !p.arguments.iter().any(|a| a.name == k.as_str()))) {
        return Err(format!("prompt `{name}` has no argument `{extra}`"));
    }
    let mut values: Vec<(String, String)> = Vec::with_capacity(p.arguments.len());
    for a in p.arguments {
        let given = match args.and_then(|m| m.get(a.name)) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.trim().to_owned()),
            Some(_) => return Err(format!("argument `{}` of prompt `{name}` must be a string", a.name)),
        };
        let value = match given {
            Some(v) if !v.is_empty() => v,
            _ if a.required => return Err(format!("prompt `{name}` needs the `{}` argument", a.name)),
            _ => a.default.to_owned(),
        };
        values.push((format!("{{{}}}", a.name), value));
    }
    // One pass over the template, so a value that looks like a placeholder is left as written.
    let template = flow(p.text);
    let mut text = String::with_capacity(template.len());
    let mut rest = template.as_str();
    while let Some(c) = rest.chars().next() {
        match values.iter().find(|(placeholder, _)| rest.starts_with(placeholder.as_str())) {
            Some((placeholder, value)) => {
                text.push_str(value);
                rest = rest.get(placeholder.len()..).unwrap_or_default();
            }
            None => {
                text.push(c);
                rest = rest.get(c.len_utf8()..).unwrap_or_default();
            }
        }
    }
    Ok(json!({
        "description": p.description,
        "messages": [{"role": "user", "content": {"type": "text", "text": text}}],
    }))
}

/// The `completion/complete` result: values for a prompt argument or a resource-template variable
/// that start with what was typed, ignoring case, at most [`MAX_VALUES`] of them. An unknown
/// prompt, argument or template gives no values.
pub fn complete(b: &mut dyn Backend, params: Option<&Value>) -> Value {
    let reference = params.and_then(|p| p.get("ref"));
    let argument = params.and_then(|p| p.get("argument"));
    let name = argument.and_then(|a| a.get("name")).and_then(Value::as_str).unwrap_or("");
    let typed = argument.and_then(|a| a.get("value")).and_then(Value::as_str).unwrap_or("").to_lowercase();
    let values = match reference.and_then(|r| r.get("type")).and_then(Value::as_str) {
        Some("ref/prompt") => {
            let prompt = reference.and_then(|r| r.get("name")).and_then(Value::as_str).and_then(|n| PROMPTS.iter().find(|p| p.name == n));
            match prompt.and_then(|p| p.arguments.iter().find(|a| a.name == name)).map(|a| a.values) {
                Some(Values::Fixed(v)) => v.iter().map(|s| (*s).to_owned()).collect(),
                Some(Values::Live(live)) => resources::live_values(b, live),
                None => Vec::new(),
            }
        }
        Some("ref/resource") => match reference.and_then(|r| r.get("uri")).and_then(Value::as_str).and_then(resources::template_of) {
            Some(t) => resources::live_values(b, t.live),
            None => Vec::new(),
        },
        _ => Vec::new(),
    };
    let mut matches: Vec<String> = values.into_iter().filter(|v| v.to_lowercase().starts_with(&typed)).collect();
    let total = matches.len();
    matches.truncate(MAX_VALUES);
    json!({"completion": {"values": matches, "total": total, "hasMore": total > MAX_VALUES}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_joins_lines_and_keeps_steps() {
        assert_eq!(flow("one\ntwo\n\nSteps:\n1. a\nb\n2. c"), "one two\n\nSteps:\n1. a b\n2. c");
        assert_eq!(flow(""), "");
    }

    #[test]
    fn every_prompt_renders_without_placeholders_left() {
        for p in PROMPTS {
            let args: serde_json::Map<String, Value> = p.arguments.iter().filter(|a| a.required).map(|a| (a.name.to_owned(), json!("x"))).collect();
            let v = get(p.name, Some(&Value::Object(args))).unwrap();
            let text = v["messages"][0]["content"]["text"].as_str().unwrap();
            for a in p.arguments {
                assert!(!text.contains(&format!("{{{}}}", a.name)), "{}: {{{}}} left", p.name, a.name);
            }
            assert!(!text.contains("  ") && !text.contains('\u{2014}'), "{}", p.name);
        }
    }
}
