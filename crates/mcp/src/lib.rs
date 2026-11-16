//! StudioCraft's MCP server.
//!
//! [`Server`] answers MCP requests (JSON-RPC, one message per line) with tools, resources, resource
//! templates, prompts and completions. It
//! works through a [`Backend`]: [`Headless`] runs a studio in this process without a window, and
//! [`Remote`] forwards every call to a running app's control channel
//! (`docs/control-protocol.md`). Both answer the same control method names, so every tool works
//! the same way on either.

mod backend;
mod headless;
mod prompts;
mod remote;
mod resources;
mod roots;
mod server;
mod tools;

pub use backend::{Backend, McpError};
pub use headless::{Headless, ServerOptions};
pub use remote::{MAX_REPLY, Remote, TOKEN_ENV, parse_target};
pub use roots::{Access, AccessError, Roots};
pub use server::{INSTRUCTIONS, MAX_MESSAGE, Server};
pub use tools::{MAX_BATCH_STEPS, tool_definitions};

#[cfg(test)]
mod tests;
