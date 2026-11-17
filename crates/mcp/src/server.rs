//! The MCP server loop: JSON-RPC messages, one per line, answered through a [`Backend`].

use std::io::{BufRead, Write};

use craft_jsonrpc::{Limits, RpcError};
use craft_mcp::{Policy, Session, tool_call, tool_list};
use serde_json::{Value, json};
use studiocraft_engine::links;

use crate::backend::{Backend, McpError};
use crate::{prompts, resources, tools};

/// Longest message accepted from the MCP client.
pub const MAX_MESSAGE: usize = 4 << 20;

/// Protocol versions the server answers, newest first.
const VERSIONS: [&str; 2] = [craft_mcp::PROTOCOL_VERSION, "2024-11-05"];

/// What the MCP client is told about StudioCraft at initialization.
pub const INSTRUCTIONS: &str = "StudioCraft is a recording and live-streaming studio. A scene collection holds scenes, sources \
(cameras, screens, pictures, text, media, audio devices) and scene items, which place a source in a scene. Coordinates are canvas \
pixels, x to the right and y down, with the origin at the top-left; the default canvas is 1920x1080. Rotation is in degrees, volume \
gain is 0 to 1 and durations are in milliseconds. Every action is a command: command_list shows ids and parameters, command_run runs \
one and command_batch runs several. A typical flow is scene_add, then source_add {kind, settings}, then item_transform, then \
render_preview to look at the result and doc_inspect to read ids back. Studio mode keeps a preview scene and a program scene; \
studio_transition moves the preview to the program. record_start and record_stop control the recording, stream_start and \
stream_stop the stream to the configured RTMP server, replay_start, replay_stop and replay_save the replay buffer, and \
output_status reports all three. prompts/list has ready-made workflows \
(build_scene, lower_third, check_collection), completion/complete finishes a command id, a scene or source name or a plug-in kind, \
and studiocraft://scene/{id}, studiocraft://source/{id}, studiocraft://command/{id} and studiocraft://plugin/{kind} read one thing \
at a time instead of the whole scene collection; scenes and sources are named by id or by name.";

/// An MCP server over one backend.
pub struct Server {
    session: Session,
    backend: Box<dyn Backend>,
    limits: Limits,
}

impl Server {
    pub fn new(backend: Box<dyn Backend>) -> Result<Server, McpError> {
        let info = json!({"name": links::APP, "title": "StudioCraft", "version": env!("CARGO_PKG_VERSION")});
        // The legacy form is the one that returns the instructions at initialization.
        let mut session = Session::new(links::APP, env!("CARGO_PKG_VERSION"), Policy::Compatible)
            .with_capabilities(json!({"tools": {}, "resources": {}, "prompts": {}, "completions": {}}))
            .map_err(|e| McpError::Invalid(e.message))?
            .with_legacy_initialize(&VERSIONS, info);
        let window = if backend.has_ui() {
            " The ui_* tools work the window: ui_inspect and ui_screenshot show it, ui_click, ui_drag, ui_key and ui_type send input, \
             ui_menu_list and ui_menu_invoke use the menus, ui_dialog answers a name prompt or the exit confirmation, and app_quit closes \
             the app. Window coordinates are points from the top-left."
        } else {
            ""
        };
        session.set_instructions(format!(
            "{INSTRUCTIONS}{window} Backend: {}. Community: {} (Discord), {} (source). The app.links command returns these links.",
            backend.describe(),
            links::DISCORD,
            links::GITHUB
        ));
        Ok(Server { session, backend, limits: Limits { max_message_bytes: MAX_MESSAGE, ..Limits::default() } })
    }

    pub fn backend(&self) -> &dyn Backend {
        self.backend.as_ref()
    }

    /// Answer one line from the client: the reply line, or `None` for notifications and blank
    /// lines.
    pub fn handle_line(&mut self, line: &[u8]) -> Option<String> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        let msg = match craft_jsonrpc::parse_json(line, self.limits) {
            Ok(m) => m,
            Err(_) => return Some(craft_jsonrpc::parse_error_response().to_string()),
        };
        let in_batch = msg.is_array();
        let mut notes = Vec::new();
        let (session, backend) = (&mut self.session, &mut self.backend);
        let reply = craft_jsonrpc::process(
            &msg,
            self.limits,
            |req| {
                if let Some(r) = session.request(req, in_batch) {
                    return r;
                }
                match req.method {
                    "tools/list" => Ok(tool_list(Value::Array(tools::tool_definitions(backend.has_ui())))),
                    "tools/call" => {
                        let empty = json!({});
                        let (name, args) = tool_call(req.params.unwrap_or(&empty))?;
                        tools::call_tool(backend.as_mut(), name, &args)
                    }
                    "resources/list" => Ok(resources::list()),
                    "resources/templates/list" => Ok(resources::templates()),
                    "resources/read" => resources::read(backend.as_mut(), req.params),
                    "prompts/list" => Ok(prompts::list()),
                    "prompts/get" => {
                        let name = req
                            .params
                            .and_then(|p| p.get("name"))
                            .and_then(Value::as_str)
                            .ok_or_else(|| RpcError::new(-32602, "`name` must be a string"))?;
                        prompts::get(name, req.params.and_then(|p| p.get("arguments"))).map_err(|e| RpcError::new(-32602, e))
                    }
                    "completion/complete" => Ok(prompts::complete(backend.as_mut(), req.params)),
                    other => Err(RpcError::new(-32601, format!("Method not found: {other}"))),
                }
            },
            |req| notes.push(req.method.to_string()),
        );
        for n in notes {
            let note = json!({"jsonrpc": "2.0", "method": n});
            if let Some(req) = craft_jsonrpc::request(&note, self.limits) {
                self.session.notification(req);
            }
        }
        let reply = reply.unwrap_or_else(|_| Some(craft_jsonrpc::error_response(Value::Null, RpcError::new(-32600, "batch too large"))));
        reply.map(|r| r.to_string())
    }

    /// Serve `input` until it ends, writing each reply as a line to `output`.
    pub fn serve(&mut self, input: &mut impl BufRead, output: &mut impl Write) -> Result<(), McpError> {
        loop {
            let line = match craft_jsonrpc::read_line(input, MAX_MESSAGE) {
                Ok(Some(l)) => l,
                Ok(None) => return Ok(()),
                Err(e) => return Err(McpError::Io(format!("reading the MCP client: {e:?}"))),
            };
            if let Some(reply) = self.handle_line(&line) {
                writeln!(output, "{reply}").and_then(|()| output.flush()).map_err(|e| McpError::Io(e.to_string()))?;
            }
        }
    }
}
