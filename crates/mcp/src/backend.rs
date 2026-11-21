//! Where tool calls go: an in-process studio or a running app.

use serde_json::Value;
use studiocraft_studio::{FileError, StudioError};

use crate::roots::AccessError;

/// Why a backend call failed. Tools show the message to the MCP client as is.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// A parameter is missing, has the wrong type or is out of range.
    #[error("{0}")]
    Invalid(String),
    /// The call needs the app's window.
    #[error("{0}")]
    NeedsApp(String),
    #[error(transparent)]
    Studio(#[from] StudioError),
    #[error(transparent)]
    File(#[from] FileError),
    /// A path lies outside the roots the server was started with.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// The server was not started with the option the call needs.
    #[error("{0}")]
    NotAllowed(String),
    /// The control target is not a loopback port.
    #[error("{0}")]
    BadTarget(String),
    /// The session token is missing or malformed.
    #[error("{0}")]
    Token(String),
    #[error("StudioCraft is not listening at {addr}: {source}")]
    Connect { addr: String, source: std::io::Error },
    /// The connection failed before the request was sent.
    #[error("the connection to the app failed: {0}")]
    Io(String),
    /// The request was sent and no reply came back.
    #[error("{reason}; `{method}` may or may not have run; inspect before repeating it")]
    Lost { method: String, reason: String },
    /// The app answered with an error.
    #[error("{0}")]
    App(String),
}

/// A place tool calls run: control methods (`engine.execute`, `document.inspect`, ...) with JSON
/// parameters.
pub trait Backend {
    /// Run one control method.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, McpError>;
    /// Whether a window is attached (the `ui.*` methods work).
    fn has_ui(&self) -> bool;
    /// One line for the server instructions.
    fn describe(&self) -> String;
    /// Run housekeeping while the MCP client sends nothing ([`crate::Server::serve_idle`]).
    fn idle(&mut self) {}
}
