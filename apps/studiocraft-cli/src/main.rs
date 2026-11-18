//! `studiocraft-cli`: talk to a running StudioCraft, serve it to MCP clients, check scene
//! collection files, and time the compositor.

mod client;
mod mcp;
mod tracks;

use std::process::ExitCode;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::json;
use studiocraft_model::{Profile, SceneCollection};
use studiocraft_render::Gpu;
use studiocraft_studio::files::read_collection_file;
use studiocraft_studio::{Studio, builtin_registry};

const USAGE: &str = "\
usage:
  studiocraft-cli send <port> <method> [<params as JSON>]
                     send one control request to the app and print the reply. The methods are
                     engine.execute, engine.commands, document.inspect, render.preview,
                     ui.inspect, ui.menu.list, ui.menu.invoke, ui.click, ui.drag, ui.key, ui.type,
                     ui.screenshot and app.quit
  studiocraft-cli mcp [--read-root <dir>]... [--write-root <dir>]... [--allow-streaming]
                      [--collection <file>]
                     an MCP server on standard input and output with its own studio and no window.
                     --collection opens <file> before serving; the roots keep its file reads and
                     writes inside those folders (each flag may be given more than once);
                     --allow-streaming lets it start a stream
  studiocraft-cli mcp --connect <port>
                     an MCP server on standard input and output that controls the app started
                     with `studiocraft --control <port>`. `mcp <port>` is read as
                     `mcp --connect <port>`
  studiocraft-cli check <collection.json>...
                     report problems in scene collection files
  studiocraft-cli bench [<seconds>]
                     run the compositor without a window for 1 to 600 seconds (5 by
                     default) and print its frame times
  studiocraft-cli bench audio-tracks [--seconds <s>] [--stall <s>] [--stall-at <s>]
                                     [--buffer <MiB>] [--tracks <n>] [--folder <dir>]
                     feed <n> audio tracks (9 by default) at the mixer's pace for 60 seconds
                     while the disk takes nothing for 40 seconds from second 5, then print
                     the seconds lost and whether every file has the full length. The files
                     go to a temporary folder that is deleted, or to <dir>, which is kept
  studiocraft-cli audio-tracks repair <folder>
                     repair the audio tracks of takes that stopped without finishing their
                     files, in <folder> and the folders in it, and print each file's length
  studiocraft-cli --version
                     print `studiocraft-cli <version>`

<port> is a port from 1 to 65535 or 127.0.0.1:<port>. `send` and `mcp --connect` read the session
token from STUDIOCRAFT_CONTROL_TOKEN, as the app does. A command line that can't be read exits
with code 2; a command that fails exits with code 1.
";

/// Why a command did not finish.
#[derive(Debug, PartialEq)]
pub enum CliError {
    /// The command line can't be read: an unknown command or option, or a missing or bad value.
    Usage(String),
    /// The command ran and failed.
    Failed(String),
}

impl CliError {
    fn exit_code(&self) -> ExitCode {
        match self {
            CliError::Usage(_) => ExitCode::from(2),
            CliError::Failed(_) => ExitCode::FAILURE,
        }
    }
}

fn check(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("name at least one file".into());
    }
    let ffmpeg = Arc::new(RwLock::new(String::new()));
    let registry = builtin_registry(&ffmpeg).map_err(|e| e.to_string())?;
    let mut failed = false;
    for p in paths {
        // The file's size is checked before it is read.
        let c = match read_collection_file(std::path::Path::new(p)) {
            Ok(c) => c,
            Err(e) => {
                println!("{p}: {e}");
                failed = true;
                continue;
            }
        };
        let mut problems = c.problems();
        for s in &c.sources {
            if registry.source(&s.kind).is_err() {
                problems.push(format!("source `{}` is of a type this build lacks: {}", s.name, s.kind));
            }
            for f in &s.filters {
                if registry.filter(&f.kind).is_err() {
                    problems.push(format!("filter `{}` on `{}` is of a type this build lacks: {}", f.name, s.name, f.kind));
                }
            }
        }
        if registry.transition(&c.transition.kind).is_err() {
            problems.push(format!("the transition is of a type this build lacks: {}", c.transition.kind));
        }
        if problems.is_empty() {
            println!("{p}: {} scenes, {} sources, no problems", c.scenes.len(), c.sources.len());
        } else {
            failed = true;
            for pr in problems {
                println!("{p}: {pr}");
            }
        }
    }
    if failed { Err("problems were found".into()) } else { Ok(()) }
}

/// A 1080p scene with a background, four overlapping pictures and a text.
fn bench_collection() -> SceneCollection {
    let mut c = SceneCollection::new("Bench", "Scene");
    for (i, (color, w, h, x, y)) in [
        ("#202830ff", 1920, 1080, 0.0, 0.0),
        ("#d04040c0", 800, 600, 100.0, 100.0),
        ("#40d040c0", 800, 600, 500.0, 300.0),
        ("#4040d0c0", 800, 600, 900.0, 200.0),
        ("#e0e0e080", 400, 400, 1400.0, 600.0),
    ]
    .iter()
    .enumerate()
    {
        let id = studiocraft_model::SourceId(c.alloc_id());
        let settings = studiocraft_model::Settings::new().with("color", *color).with("width", *w).with("height", *h);
        c.sources.push(studiocraft_model::SourceConfig {
            id,
            kind: "color".into(),
            kind_version: 1,
            name: format!("Box {i}"),
            settings,
            filters: vec![],
            audio: Default::default(),
        });
        let item = studiocraft_model::ItemId(c.alloc_id());
        let transform = studiocraft_model::Transform {
            position: studiocraft_model::Vec2::new(*x, *y),
            rotation: if i == 3 { 15.0 } else { 0.0 },
            ..Default::default()
        };
        c.scenes[0].items.push(studiocraft_model::SceneItem { id: item, source: id, transform, visible: true, locked: false });
    }
    c
}

fn bench(seconds: f64) -> Result<(), String> {
    let gpu = Gpu::headless().map_err(|e| format!("no graphics device: {e}"))?;
    let ffmpeg = Arc::new(RwLock::new(String::new()));
    let registry = Arc::new(builtin_registry(&ffmpeg).map_err(|e| e.to_string())?);
    let mut profile = Profile::default();
    profile.video.fps_num = 60;
    let mut studio = Studio::new(Some(gpu), bench_collection(), profile, registry, ffmpeg);
    let until = Instant::now() + Duration::try_from_secs_f64(seconds).map_err(|e| e.to_string())?;
    while Instant::now() < until {
        studio.tick();
        std::thread::sleep(Duration::from_millis(16));
    }
    let s = studio.stats();
    for n in studio.take_notices() {
        eprintln!("{}", n.text());
    }
    let report = json!({
        "canvas": "1920x1080", "fps": 60, "seconds": seconds,
        "frames": s.frames, "lagged": s.lagged, "frame_ms_average": s.render_avg_ms, "frame_ms_last": s.render_ms,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    Ok(())
}

/// The length of a `bench` run: 5 seconds when none is given, otherwise from 1 to 600.
fn bench_seconds(arg: Option<&str>) -> Result<f64, CliError> {
    let Some(arg) = arg else { return Ok(5.0) };
    match arg.parse::<f64>() {
        Ok(s) if (1.0..=600.0).contains(&s) => Ok(s),
        _ => Err(CliError::Usage(format!("`{arg}` is not a number of seconds from 1 to 600"))),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = match std::env::args_os().skip(1).map(std::ffi::OsString::into_string).collect() {
        Ok(args) => args,
        Err(arg) => {
            eprintln!("studiocraft-cli: `{}` is not valid Unicode", arg.to_string_lossy());
            return ExitCode::from(2);
        }
    };
    let result = match args.first().map(String::as_str) {
        Some("send") => client::send(&args[1..]),
        Some("mcp") => mcp::run(&args[1..]),
        Some("check") if args.len() < 2 => Err(CliError::Usage("name at least one file".into())),
        Some("check") => check(&args[1..]).map_err(CliError::Failed),
        Some("bench") if args.get(1).is_some_and(|a| a == "audio-tracks") => {
            tracks::bench_args(&args[2..]).and_then(|b| tracks::bench(&b).map_err(CliError::Failed))
        }
        Some("audio-tracks") => tracks::run(&args[1..]),
        Some("bench") => bench_seconds(args.get(1).map(String::as_str)).and_then(|s| bench(s).map_err(CliError::Failed)),
        Some("-V" | "--version" | "version") => {
            println!("studiocraft-cli {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(CliError::Usage(format!("unknown command `{other}`\n\n{USAGE}"))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let code = e.exit_code();
            let (CliError::Usage(text) | CliError::Failed(text)) = e;
            eprintln!("studiocraft-cli: {text}");
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_lengths_outside_the_range_are_refused() {
        assert_eq!(bench_seconds(None).ok(), Some(5.0));
        assert_eq!(bench_seconds(Some("12.5")).ok(), Some(12.5));
        for bad in ["NaN", "inf", "0.5", "601", "abc", ""] {
            assert!(matches!(bench_seconds(Some(bad)), Err(CliError::Usage(_))), "{bad}");
        }
    }

    #[test]
    fn the_bench_collection_is_valid() {
        let c = bench_collection();
        assert!(c.problems().is_empty(), "{:?}", c.problems());
        assert_eq!(c.scenes[0].items.len(), 5);
    }

    #[test]
    fn check_reports_unknown_plugins() {
        let dir = std::env::temp_dir().join(format!("studiocraft-cli-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = bench_collection();
        let good = dir.join("good.json");
        std::fs::write(&good, studiocraft_model::format::write_collection(&c).unwrap()).unwrap();
        assert!(check(&[good.to_string_lossy().into_owned()]).is_ok());
        c.sources[0].kind = "hologram".into();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, studiocraft_model::format::write_collection(&c).unwrap()).unwrap();
        assert!(check(&[bad.to_string_lossy().into_owned()]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_usage_names_the_namespaced_methods_and_the_mcp_options() {
        for method in ["engine.execute", "engine.commands", "document.inspect", "render.preview", "ui.inspect", "ui.screenshot", "app.quit"] {
            assert!(USAGE.contains(method), "{method}");
        }
        for option in ["--connect <port>", "--read-root <dir>", "--write-root <dir>", "--allow-streaming", "--collection <file>", "`mcp <port>`"] {
            assert!(USAGE.contains(option), "{option}");
        }
    }
}
