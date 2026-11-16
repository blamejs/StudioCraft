//! `send`: one control request to a running app, through the MCP crate's control client.

use serde_json::{Value, json};
pub use studiocraft_mcp::Remote;

use crate::CliError;

/// `send <port> <method> [<params as JSON>]`: run one control method and print the reply.
pub fn send(args: &[String]) -> Result<(), CliError> {
    let target = args.first().ok_or_else(|| CliError::Usage("a port is needed".into()))?;
    let method = args.get(1).ok_or_else(|| CliError::Usage("name a method, such as engine.execute or document.inspect".into()))?;
    let params: Value = match args.get(2) {
        Some(p) => serde_json::from_str(p).map_err(|e| CliError::Usage(format!("the parameters are not JSON: {e}")))?,
        None => json!({}),
    };
    if args.len() > 3 {
        return Err(CliError::Usage("give the parameters as one JSON argument".into()));
    }
    studiocraft_mcp::parse_target(target).map_err(|e| CliError::Usage(e.to_string()))?;
    let mut remote = Remote::from_env(target).map_err(|e| CliError::Failed(e.to_string()))?;
    let reply = studiocraft_mcp::Backend::call(&mut remote, method, params).map_err(|e| CliError::Failed(e.to_string()))?;
    println!("{}", serde_json::to_string_pretty(&reply).unwrap_or_default());
    Ok(())
}
