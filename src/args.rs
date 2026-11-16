//! The command line.

use std::path::PathBuf;

#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub config_dir: Option<PathBuf>,
    pub no_prefs: bool,
    pub control: Option<u16>,
    pub screenshot: Option<PathBuf>,
    pub self_test: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Run(Args),
    Version,
    Help,
}

fn value(name: &str, v: Option<String>) -> Result<String, String> {
    match v {
        Some(v) if !v.is_empty() && !v.starts_with("--") => Ok(v),
        Some(v) => Err(format!("{name} needs a value, not `{v}`")),
        None => Err(format!("{name} needs a value")),
    }
}

fn once<T>(slot: &mut Option<T>, name: &str, v: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{name} given twice"));
    }
    *slot = Some(v);
    Ok(())
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Action, String> {
    let mut a = Args::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut next = |name: &str| value(name, inline.clone().or_else(|| it.next()));
        match name.as_str() {
            "--version" | "-V" => return Ok(Action::Version),
            "--help" | "-h" => return Ok(Action::Help),
            "--no-prefs" => a.no_prefs = true,
            "--config-dir" => {
                let v = next("--config-dir")?;
                once(&mut a.config_dir, "--config-dir", PathBuf::from(v))?;
            }
            "--control" => {
                let v = next("--control")?;
                let port = v.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| format!("--control needs a port from 1 to 65535, not `{v}`"))?;
                once(&mut a.control, "--control", port)?;
            }
            "--screenshot" => {
                let v = next("--screenshot")?;
                once(&mut a.screenshot, "--screenshot", PathBuf::from(v))?;
            }
            "--self-test" => {
                let v = next("--self-test")?;
                once(&mut a.self_test, "--self-test", PathBuf::from(v))?;
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    if a.no_prefs && a.config_dir.is_some() {
        return Err("--no-prefs and --config-dir can't be used together".into());
    }
    Ok(Action::Run(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Action, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn reads_every_option() {
        assert_eq!(p(&[]), Ok(Action::Run(Args::default())));
        assert_eq!(p(&["--version"]), Ok(Action::Version));
        assert_eq!(p(&["-h"]), Ok(Action::Help));
        let Ok(Action::Run(a)) = p(&["--control", "7070", "--screenshot=shot.png", "--config-dir", "cfg"]) else { panic!() };
        assert_eq!(a.control, Some(7070));
        assert_eq!(a.screenshot, Some(PathBuf::from("shot.png")));
        assert_eq!(a.config_dir, Some(PathBuf::from("cfg")));
    }

    #[test]
    fn refuses_bad_input() {
        assert!(p(&["--control", "0"]).is_err());
        assert!(p(&["--control", "70000"]).is_err());
        assert!(p(&["--control"]).is_err());
        assert!(p(&["--screenshot", "--self-test"]).is_err());
        assert!(p(&["--self-test", "a", "--self-test", "b"]).is_err());
        assert!(p(&["--no-prefs", "--config-dir", "x"]).is_err());
        assert!(p(&["--frobnicate"]).is_err());
    }
}
