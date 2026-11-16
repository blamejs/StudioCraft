//! MCP over standard input and output against the built `studiocraft-cli mcp` binary.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_studiocraft-cli");
/// A well-formed session token; nothing listens with it.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn command() -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env("STUDIOCRAFT_CONTROL_TOKEN", TOKEN);
    cmd
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("studiocraft-cli-mcp-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn spawn(args: &[&str]) -> Self {
        let mut child = command()
            .arg("mcp")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn studiocraft-cli");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, next: 1 }
    }

    fn send(&mut self, v: &Value) {
        let w = self.stdin.as_mut().unwrap();
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).unwrap();
        assert!(n > 0, "the server closed its output");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not a JSON line: {line}: {e}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let v = self.read();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], id, "a reply to another request: {v}");
        v
    }

    fn initialize(&mut self) -> Value {
        let v =
            self.request("initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}}));
        assert_eq!(v["result"]["serverInfo"]["name"], "studiocraft", "{v}");
        assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        v
    }

    /// The tool's result, which must not be an error.
    fn tool(&mut self, name: &str, args: Value) -> Value {
        let v = self.request("tools/call", json!({"name": name, "arguments": args}));
        assert!(v.get("error").is_none(), "{name}: {v}");
        assert_ne!(v["result"]["isError"], json!(true), "{name} {args}: {v}");
        v["result"].clone()
    }

    /// The tool's text result read as JSON.
    fn tool_json(&mut self, name: &str, args: Value) -> Value {
        let r = self.tool(name, args);
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn scene_names(&mut self) -> Vec<String> {
        let doc = self.tool_json("doc_inspect", json!({}));
        doc["collection"]["scenes"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_owned()).collect()
    }

    fn close(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        assert!(status.success(), "the server exited with {status}");
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn run(args: &[&str]) -> Output {
    command().args(args).stdin(Stdio::null()).output().unwrap()
}

#[test]
fn headless_server_lists_tools_runs_commands_and_inspects() {
    let mut m = Mcp::spawn(&[]);
    let init = m.initialize();
    assert!(init["result"]["instructions"].as_str().unwrap().contains("Backend:"), "{init}");

    let list = m.request("tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in ["command_list", "command_run", "command_batch", "doc_inspect", "render_preview", "scene_add", "collection_save"] {
        assert!(names.contains(&want), "{want} is missing from {names:?}");
    }
    assert!(names.iter().all(|n| !n.starts_with("ui_")), "window tools on the headless server: {names:?}");

    let before = m.scene_names().len();
    let added = m.tool_json("command_run", json!({"id": "scene.add", "params": {"name": "Intro"}}));
    assert!(added["scene"].is_u64(), "{added}");
    let scenes = m.scene_names();
    assert_eq!(scenes.len(), before + 1, "{scenes:?}");
    assert!(scenes.iter().any(|s| s == "Intro"), "{scenes:?}");

    // A failed command is a tool error, and the server keeps answering.
    let v = m.request("tools/call", json!({"name": "command_run", "arguments": {"id": "no.such.command"}}));
    assert_eq!(v["result"]["isError"], json!(true), "{v}");
    assert_eq!(m.scene_names(), scenes);
    m.close();
}

#[test]
fn collection_opens_a_saved_file_before_serving() {
    let dir = tmpdir("collection");
    let file = dir.join("show.json");
    let file_text = file.to_string_lossy().into_owned();

    let mut m = Mcp::spawn(&[]);
    m.initialize();
    m.tool("command_run", json!({"id": "scene.add", "params": {"name": "Be right back"}}));
    m.tool("collection_save", json!({"path": file_text}));
    let saved = m.scene_names();
    m.close();
    assert!(file.is_file());

    let mut m = Mcp::spawn(&["--collection", &file_text]);
    m.initialize();
    assert_eq!(m.scene_names(), saved);
    m.close();

    let missing = dir.join("missing.json");
    let out = run(&["mcp", "--collection", &missing.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("could not be opened"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn connect_to_a_port_with_no_app_fails_on_the_first_tool_call() {
    // `mcp <port>` is the earlier spelling of `mcp --connect <port>`.
    for args in [&["--connect", "1"][..], &["1"]] {
        let mut m = Mcp::spawn(args);
        let init = m.initialize();
        assert!(init["result"]["instructions"].as_str().unwrap().contains("127.0.0.1:1"), "{init}");
        let v = m.request("tools/call", json!({"name": "doc_inspect", "arguments": {}}));
        assert_eq!(v["result"]["isError"], json!(true), "{v}");
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("StudioCraft is not listening at 127.0.0.1:1"), "{args:?}: {text}");
        m.close();
    }
}

#[test]
fn bad_mcp_options_exit_with_code_two() {
    let dir = tmpdir("options");
    let root = dir.to_string_lossy().into_owned();
    let cases: [&[&str]; 7] = [
        &["mcp", "--bogus"],
        &["mcp", "--connect", "0"],
        &["mcp", "--connect", "70000"],
        &["mcp", "--connect", "9876", "--connect", "9877"],
        &["mcp", "9876", "9877"],
        &["mcp", "--connect", "9876", "--read-root", &root],
        &["mcp", "--connect"],
    ];
    for args in cases {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).starts_with("studiocraft-cli: "), "{args:?}: {out:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn help_names_the_connect_form_and_the_namespaced_methods() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for want in ["mcp --connect <port>", "`mcp <port>` is read as", "--read-root <dir>", "--collection <file>", "engine.execute", "document.inspect"]
    {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert_eq!(run(&["frobnicate"]).status.code(), Some(2));
}
