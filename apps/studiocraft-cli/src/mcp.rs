//! `mcp`: the MCP server on standard input and output, on a studio in this process (the default)
//! or on a running app (`--connect`).

use std::path::PathBuf;

use serde_json::json;
use studiocraft_mcp::{Backend, Headless, McpError, Remote, Roots, Server, ServerOptions, parse_target};
use studiocraft_render::Gpu;

use crate::CliError;

/// What `mcp` was asked for: a running app to control, or the in-process studio's options and the
/// scene collection it opens first.
#[derive(Debug, PartialEq)]
pub struct McpArgs {
    /// The app's control address as given (a port or `127.0.0.1:<port>`), already checked.
    pub connect: Option<String>,
    pub options: ServerOptions,
    /// A scene collection file the in-process studio opens before serving.
    pub collection: Option<String>,
}

/// `mcp`'s arguments. `mcp <port>` is read as `mcp --connect <port>`. The roots,
/// `--allow-streaming` and `--collection` apply to the in-process studio only, so they can't be
/// combined with a port.
pub fn parse_mcp_args(args: &[String]) -> Result<McpArgs, String> {
    let (mut connect, mut collection, mut allow_streaming) = (None::<String>, None::<String>, false);
    let (mut read, mut write) = (Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        let port = match a.as_str() {
            "--read-root" | "--automation-read-root" => {
                read.push(PathBuf::from(value(a)?));
                continue;
            }
            "--write-root" | "--automation-write-root" => {
                write.push(PathBuf::from(value(a)?));
                continue;
            }
            "--allow-streaming" => {
                allow_streaming = true;
                continue;
            }
            "--collection" => {
                let file = value(a)?;
                if collection.replace(file).is_some() {
                    return Err("give --collection once".into());
                }
                continue;
            }
            "--connect" => value(a)?,
            other if other.starts_with('-') => return Err(format!("unknown mcp option `{other}`")),
            // The earlier form, `mcp <port>`.
            other => other.to_owned(),
        };
        parse_target(&port).map_err(|e| e.to_string())?;
        if connect.replace(port).is_some() {
            return Err("give one port: --connect (or a bare port) may appear once".into());
        }
    }
    if connect.is_some() {
        if !(read.is_empty() && write.is_empty()) {
            return Err("--read-root and --write-root confine the in-process studio; the app at a port is not held to them, so leave out \
                        the port or the roots"
                .into());
        }
        if allow_streaming {
            return Err("--allow-streaming applies to the in-process studio; the app at a port streams as its menu item does".into());
        }
        if collection.is_some() {
            return Err("--collection opens a file in the in-process studio; with a port, open it with the collection_open tool".into());
        }
    }
    let roots = if read.is_empty() && write.is_empty() { None } else { Some(Roots::new(&read, &write).map_err(|e| e.to_string())?) };
    Ok(McpArgs { connect, options: ServerOptions { allow_streaming, roots }, collection })
}

/// Serve MCP on standard input and output until the input ends.
pub fn run(args: &[String]) -> Result<(), CliError> {
    let McpArgs { connect, options, collection } = parse_mcp_args(args).map_err(CliError::Usage)?;
    let failed = |e: McpError| CliError::Failed(e.to_string());
    let backend: Box<dyn Backend> = match connect {
        Some(target) => Box::new(Remote::from_env(&target).map_err(failed)?),
        None => {
            if let Some(roots) = &options.roots {
                eprintln!("studiocraft-cli: files are confined to {}", roots.describe());
            }
            let mut headless = Headless::with_options(Gpu::headless().ok(), options).map_err(failed)?;
            if let Some(file) = &collection {
                headless
                    .call("collection.open", json!({"path": file}))
                    .map_err(|e| CliError::Failed(format!("the scene collection {file} could not be opened: {e}")))?;
            }
            Box::new(headless)
        }
    };
    let mut server = Server::new(backend).map_err(failed)?;
    server.serve_idle(std::io::BufReader::new(std::io::stdin()), &mut std::io::stdout(), IDLE).map_err(failed)
}

/// How often the in-process studio's housekeeping runs while the MCP client sends nothing.
const IDLE: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    fn folders(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("studiocraft-cli-mcp-args-{tag}-{}", std::process::id()));
        for d in ["in", "in2", "out"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        dir
    }

    #[test]
    fn no_options_run_headless() {
        assert_eq!(parse_mcp_args(&[]).unwrap(), McpArgs { connect: None, options: ServerOptions::default(), collection: None });
    }

    #[test]
    fn connect_takes_a_port_or_a_loopback_address() {
        assert_eq!(parse_mcp_args(&strings(&["--connect", "9876"])).unwrap().connect.as_deref(), Some("9876"));
        assert_eq!(parse_mcp_args(&strings(&["--connect", "127.0.0.1:9876"])).unwrap().connect.as_deref(), Some("127.0.0.1:9876"));
    }

    #[test]
    fn a_bare_port_is_read_as_connect() {
        assert_eq!(parse_mcp_args(&strings(&["9876"])).unwrap(), parse_mcp_args(&strings(&["--connect", "9876"])).unwrap());
        assert_eq!(parse_mcp_args(&strings(&["127.0.0.1:9876"])).unwrap().connect.as_deref(), Some("127.0.0.1:9876"));
    }

    #[test]
    fn roots_streaming_and_a_collection_set_up_the_headless_studio() {
        let dir = folders("headless");
        let path = |d: &str| dir.join(d).to_string_lossy().into_owned();
        let a = parse_mcp_args(&strings(&[
            "--read-root",
            &path("in"),
            "--automation-read-root",
            &path("in2"),
            "--write-root",
            &path("out"),
            "--allow-streaming",
            "--collection",
            "show.json",
        ]))
        .unwrap();
        assert!(a.connect.is_none() && a.options.allow_streaming);
        assert_eq!(a.collection.as_deref(), Some("show.json"));
        let roots = a.options.roots.unwrap();
        assert_eq!((roots.read().len(), roots.write().len()), (2, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_options_are_refused() {
        assert!(parse_mcp_args(&strings(&["--allow-everything"])).unwrap_err().contains("unknown mcp option `--allow-everything`"));
        assert!(parse_mcp_args(&strings(&["--headless"])).unwrap_err().contains("unknown"));
    }

    #[test]
    fn a_second_connect_is_refused() {
        for args in [&["--connect", "9876", "--connect", "9877"][..], &["9876", "9877"], &["9876", "--connect", "9877"]] {
            assert!(parse_mcp_args(&strings(args)).unwrap_err().contains("give one port"), "{args:?}");
        }
    }

    #[test]
    fn headless_options_with_connect_are_refused() {
        let dir = folders("connect");
        let path = |d: &str| dir.join(d).to_string_lossy().into_owned();
        let e = parse_mcp_args(&strings(&["--connect", "9876", "--read-root", &path("in")])).unwrap_err();
        assert!(e.contains("--read-root"), "{e}");
        assert!(parse_mcp_args(&strings(&["9876", "--write-root", &path("out")])).is_err());
        assert!(parse_mcp_args(&strings(&["9876", "--allow-streaming"])).unwrap_err().contains("--allow-streaming"));
        assert!(parse_mcp_args(&strings(&["--connect", "9876", "--collection", "show.json"])).unwrap_err().contains("--collection"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ports_out_of_range_are_refused() {
        for bad in ["0", "65536", "70000", "127.0.0.1:0", "10.0.0.5:9876", "nine"] {
            assert!(parse_mcp_args(&strings(&["--connect", bad])).is_err(), "--connect {bad}");
            assert!(parse_mcp_args(&strings(&[bad])).is_err(), "{bad}");
        }
        assert!(parse_mcp_args(&strings(&["--connect", "65535"])).is_ok());
        assert!(parse_mcp_args(&strings(&["--connect", "1"])).is_ok());
    }

    #[test]
    fn missing_values_and_folders_are_refused() {
        for flag in ["--connect", "--read-root", "--write-root", "--collection"] {
            assert!(parse_mcp_args(&strings(&[flag])).unwrap_err().contains("needs a value"), "{flag}");
        }
        assert!(parse_mcp_args(&strings(&["--collection", "a.json", "--collection", "b.json"])).unwrap_err().contains("once"));
        let missing = std::env::temp_dir().join(format!("studiocraft-cli-no-such-folder-{}", std::process::id()));
        assert!(parse_mcp_args(&strings(&["--read-root", &missing.to_string_lossy()])).is_err());
    }
}
