//! Software updates: Help › Check for Updates, the check at start and the update prompt.
//!
//! [`Runtime`] holds the updater. It reaches the network only after the desktop app turns update
//! checks on ([`StudioApp::set_update_checks`]); tests, `--no-prefs` and scripted runs leave them
//! off, and `app.check_updates` then fails with a message.

use std::sync::Arc;

use craft_update::http::{Fetched, Http};
use craft_update::updater::{Action, CheckSource, Status, Updater};
use craft_update::{CheckResultDialog, CheckResultResponse, Release};
use serde_json::{Value, json};

use crate::i18n::Msg;
use crate::{StudioApp, UPDATE_APP};

/// The version this build reports.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The update prompt and dialogs in the interface language.
fn translate(text: &str, args: &[(&str, &str)]) -> String {
    craft_i18n::fmt(tl!(text), args)
}

/// The release to offer a copy running `running`, from the answer to a request for the release
/// list. A missing list (the repository is private, renamed or gone) and a network error are
/// reported as errors, never as "up to date". A copy running a prerelease (`0.1.0-alpha.1`) is
/// offered the newest release, prereleases included; a copy running `X.Y.Z` is offered only
/// releases without a prerelease part.
pub(crate) fn offered(fetched: Result<Fetched, String>, running: &str) -> Result<Option<Release>, String> {
    let unreachable = |why: &str| format!("could not reach the release list at {} ({why})", UPDATE_APP.releases_url());
    match fetched.map_err(|e| unreachable(&e))? {
        Fetched::Body { text, .. } => {
            let running = craft_update::parse_version(running);
            let prereleases = running.as_ref().is_some_and(|v| !craft_update::version::is_stable(v));
            let latest = craft_update::parse_github_release_list(&text, UPDATE_APP.repo, prereleases)?;
            Ok(latest.filter(|r| r.version.as_ref().zip(running.as_ref()).is_some_and(|(l, c)| craft_update::version::offers_update(l, c))))
        }
        Fetched::NotModified => Ok(None),
        Fetched::NotFound => Err(unreachable("not found")),
        Fetched::RateLimited { .. } => Err(unreachable("too many requests; try again later")),
    }
}

/// The check the desktop app runs: one request for the release list.
fn network_source() -> CheckSource {
    Arc::new(|| {
        let running = craft_update::self_update::current_version(&UPDATE_APP, VERSION).map_or_else(|| VERSION.to_string(), |v| v.to_string());
        let http = Http::new(&format!("{}/Update", UPDATE_APP.name))?;
        let url = format!("https://api.github.com/repos/{}/releases?per_page=30", UPDATE_APP.repo);
        offered(http.get_api(&url, None), &running)
    })
}

pub(crate) struct Runtime {
    pub(crate) updater: Updater,
    /// Whether this session may look for updates on the network.
    online: bool,
    /// Whether the result of Help › Check for Updates is in view.
    pub(crate) window_open: bool,
    /// The failed check the status bar has reported, so that it is reported once.
    reported: Option<String>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new(Updater::new(&UPDATE_APP, VERSION).with_source(network_source()))
    }
}

impl Runtime {
    pub(crate) fn new(updater: Updater) -> Self {
        Self { updater: updater.with_translator(translate), online: false, window_open: false, reported: None }
    }
}

impl StudioApp {
    /// Let this session look for updates: Help › Check for Updates, and the check at start when
    /// the "Check for updates at start" setting is on. Off until the desktop app turns it on.
    pub fn set_update_checks(&mut self, on: bool) {
        self.updates.online = on;
    }
}

/// `app.check_updates`: start a check whose result the window shows.
pub(crate) fn check(app: &mut StudioApp, ctx: &egui::Context) -> Result<Value, Msg> {
    let rt = &mut app.updates;
    if !rt.online {
        // Why `app.check_updates` does nothing in this session.
        return Err(tmsg!("update checks are off in this session (--no-prefs, a scripted run or a test)"));
    }
    rt.window_open = true;
    rt.updater.check_now(ctx);
    Ok(json!({"checking": true}))
}

/// Start the check at start once (when `at_start` allows it) and pick up finished work. Returns
/// the error of a requested check that has just failed, for the status bar.
pub(crate) fn tick(rt: &mut Runtime, ctx: &egui::Context, at_start: bool) -> Option<String> {
    if !rt.online {
        return None;
    }
    if at_start {
        rt.updater.check_at_start(ctx);
    }
    rt.updater.tick(ctx);
    match rt.updater.status() {
        Status::Failed(e) if rt.reported.as_ref() != Some(e) => {
            rt.reported = Some(e.clone());
            Some(e.clone())
        }
        Status::Failed(_) => None,
        _ => {
            rt.reported = None;
            None
        }
    }
}

/// What the update windows asked for this frame.
pub(crate) enum Asked {
    Nothing,
    OpenUrl(String),
    /// Close the app so that a ready update installs.
    Close,
}

/// Draw the update prompt and the result of Help › Check for Updates. `can_start` is false while
/// an update must not start (a recording or stream is running).
pub(crate) fn show(rt: &mut Runtime, ctx: &egui::Context, can_start: bool) -> Asked {
    let asked = match rt.updater.show_if(ctx, can_start) {
        Action::OpenUrl(url) => Asked::OpenUrl(url),
        Action::Close => Asked::Close,
        Action::None => Asked::Nothing,
    };
    // An available update goes to the prompt, which replaces the result dialog.
    if rt.window_open && rt.updater.prompt_open() {
        rt.window_open = false;
    }
    if rt.window_open {
        match CheckResultDialog::new(&UPDATE_APP, VERSION).with_translator(translate).show(ctx, rt.updater.status()) {
            CheckResultResponse::None => {}
            CheckResultResponse::Close => rt.window_open = false,
            CheckResultResponse::CheckAgain => rt.updater.check_now(ctx),
            CheckResultResponse::RestartToUpdate => rt.updater.restart_to_update(),
        }
        // Closing the dialog dismisses the outcome it showed.
        if !rt.window_open {
            rt.updater.dismiss_status();
        }
    }
    asked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An updater whose check answers with `answer` and never reads the launcher's version file.
    fn updater(answer: Result<Option<Release>, String>) -> Updater {
        let no_pins = std::env::temp_dir().join("studiocraft-update-tests-no-such-dir").join("versions.json");
        Updater::new(&UPDATE_APP, VERSION).with_source(Arc::new(move || answer.clone())).with_pins_path(no_pins)
    }

    /// An app whose update checks are on and answered by `answer`.
    fn app(answer: Result<Option<Release>, String>) -> StudioApp {
        let mut a = StudioApp::boot(crate::store::Store::memory(), None, Vec::new());
        a.updates = Runtime::new(updater(answer));
        a.set_update_checks(true);
        a
    }

    /// Picks up the running check's answer. The check's thread always sends an answer or closes
    /// its channel, and the updater's `tick` turns either into a status other than `Checking`.
    fn settle(a: &mut StudioApp, ctx: &egui::Context) {
        while matches!(a.updates.updater.status(), Status::Checking) {
            a.updates.updater.tick(ctx);
            std::thread::yield_now();
        }
    }

    fn frame(a: &mut StudioApp, ctx: &egui::Context) {
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| a.frame(ui));
        out.textures_delta.clear();
    }

    fn list(tags: &[&str]) -> String {
        let items: Vec<String> = tags.iter().map(|t| format!(r#"{{"tag_name":"{t}","prerelease":false,"draft":false,"assets":[]}}"#)).collect();
        format!("[{}]", items.join(","))
    }

    fn body(text: String) -> Result<Fetched, String> {
        Ok(Fetched::Body { text, etag: None })
    }

    /// A release list as GitHub sends it, with each release's `prerelease` flag.
    fn list_with_flags(tags: &[(&str, bool)]) -> String {
        let items: Vec<String> =
            tags.iter().map(|(t, pre)| format!(r#"{{"tag_name":"{t}","prerelease":{pre},"draft":false,"assets":[]}}"#)).collect();
        format!("[{}]", items.join(","))
    }

    #[test]
    fn a_prerelease_copy_is_offered_the_next_prerelease() {
        let releases = list_with_flags(&[("v0.1.0-alpha.2", true), ("v0.1.0-alpha.1", true)]);
        let release = offered(body(releases.clone()), "0.1.0-alpha.1").unwrap().expect("alpha.2 is offered");
        assert_eq!(release.tag, "v0.1.0-alpha.2");
        assert_eq!(offered(body(releases.clone()), "0.1.0-alpha.2"), Ok(None));
        // A stable release after the alphas replaces them.
        let with_stable = list_with_flags(&[("v0.1.0", false), ("v0.1.0-alpha.2", true)]);
        assert_eq!(offered(body(with_stable), "0.1.0-alpha.2").unwrap().map(|r| r.tag), Some("v0.1.0".to_string()));
        // A copy running a release without a prerelease part is not offered an alpha.
        assert_eq!(offered(body(releases), "0.0.9"), Ok(None));
        let mixed = list_with_flags(&[("v0.2.0-alpha.1", true), ("v0.1.1", false)]);
        assert_eq!(offered(body(mixed), "0.1.0").unwrap().map(|r| r.tag), Some("v0.1.1".to_string()));
    }

    #[test]
    fn a_newer_release_opens_the_update_prompt() {
        let release = offered(body(list(&["v99.0.0"])), VERSION).unwrap().expect("99.0.0 is offered");
        assert_eq!(release.tag, "v99.0.0");
        let mut a = app(Ok(Some(release)));
        let ctx = egui::Context::default();
        a.perform(&ctx, &craft_keymap::CommandRef::new("app.check_updates")).unwrap();
        assert!(a.updates.window_open);
        settle(&mut a, &ctx);
        assert!(a.updates.updater.prompt_open());
        frame(&mut a, &ctx);
        assert!(a.updates.updater.prompt_open());
        assert!(!a.updates.window_open, "the prompt replaces the result dialog");
    }

    #[test]
    fn the_running_version_is_up_to_date() {
        let current = format!("v{VERSION}");
        assert_eq!(offered(body(list(&[&current])), VERSION), Ok(None));
        assert_eq!(offered(body(list(&["v0.0.1"])), VERSION), Ok(None));
        assert_eq!(offered(Ok(Fetched::NotModified), VERSION), Ok(None));
        let mut a = app(Ok(None));
        let ctx = egui::Context::default();
        a.perform(&ctx, &craft_keymap::CommandRef::new("app.check_updates")).unwrap();
        settle(&mut a, &ctx);
        assert_eq!(a.updates.updater.status(), &Status::UpToDate);
        frame(&mut a, &ctx);
        assert!(a.updates.window_open, "the result dialog says so");
        assert!(!a.updates.updater.prompt_open());
    }

    #[test]
    fn a_failed_request_becomes_a_status_message() {
        for (fetched, why) in [
            (Err("connection refused".to_string()), "connection refused"),
            (Ok(Fetched::NotFound), "not found"),
            (Ok(Fetched::RateLimited { reset: None }), "too many requests"),
        ] {
            let e = offered(fetched, VERSION).unwrap_err();
            assert!(e.starts_with("could not reach the release list at https://github.com/"), "{e}");
            assert!(e.contains(why), "{e}");
        }
        assert!(offered(body("not json".into()), VERSION).is_err());

        let error = offered(Ok(Fetched::NotFound), VERSION).unwrap_err();
        let mut a = app(Err(error.clone()));
        let ctx = egui::Context::default();
        a.perform(&ctx, &craft_keymap::CommandRef::new("app.check_updates")).unwrap();
        settle(&mut a, &ctx);
        assert_eq!(a.updates.updater.status(), &Status::Failed(error.clone()));
        // The next frame puts it in the status bar once, and draws the result dialog.
        frame(&mut a, &ctx);
        frame(&mut a, &ctx);
        let notices: Vec<_> = a.notices.iter().filter(|(n, _)| n.error && n.msg.english().contains("could not reach the release list")).collect();
        assert_eq!(notices.len(), 1, "{:?}", a.hud()["ui"]["notices"]);
        assert!(a.updates.window_open);
    }

    #[test]
    fn scripted_and_test_sessions_never_check() {
        // A new app, as tests, --no-prefs and scripted runs have it: checks are off, and neither
        // the menu command nor the check at start reaches the network.
        let mut a = StudioApp::boot(crate::store::Store::memory(), None, Vec::new());
        assert!(a.store().prefs().check_updates, "the check at start is on by default");
        let ctx = egui::Context::default();
        let e = a.perform(&ctx, &craft_keymap::CommandRef::new("app.check_updates")).unwrap_err();
        assert_eq!(e, "update checks are off in this session (--no-prefs, a scripted run or a test)");
        for _ in 0..3 {
            frame(&mut a, &ctx);
        }
        assert_eq!(a.updates.updater.status(), &Status::Idle);
        assert!(!a.updates.window_open);
        assert!(!a.updates.updater.prompt_open());
    }

    #[test]
    fn the_app_is_described_for_the_update_prompt() {
        assert_eq!(UPDATE_APP.id, crate::APP_ID);
        assert_eq!(UPDATE_APP.name, crate::APP_NAME);
        assert!(UPDATE_APP.accent.is_some());
        assert!(UPDATE_APP.releases_url().starts_with("https://github.com/"));
        assert_eq!(UPDATE_APP.bundle_id(), crate::BUNDLE_ID, "the launcher and the updater find the app by this id");
    }

    /// Every release file name `docs/releasing.md` lists is one the updater reads as a
    /// StudioCraft package for its platform.
    #[test]
    fn the_release_file_names_in_the_docs_are_ones_the_updater_picks() {
        let doc = include_str!("../../../docs/releasing.md");
        let names: Vec<&str> = doc.split('`').filter(|s| s.starts_with("studiocraft-0.") && !s.contains(' ')).collect();
        assert!(names.len() >= 8, "{names:?}");
        let stems: Vec<&str> = UPDATE_APP.stems().collect();
        for name in &names {
            let parsed = craft_update::platform::parse_asset_name(name, &stems);
            assert!(parsed.is_some_and(|p| p.stem == crate::APP_ID), "{name} is not read as a StudioCraft package");
        }
        let release = craft_update::Release {
            assets: names.iter().map(|n| craft_update::Asset { name: (*n).into(), size: 1, url: format!("https://x/{n}"), sha256: None }).collect(),
            ..craft_update::Release::default()
        };
        for (os, arch) in [
            (craft_update::Os::Windows, craft_update::Arch::X64),
            (craft_update::Os::MacOs, craft_update::Arch::Arm64),
            (craft_update::Os::Linux, craft_update::Arch::X64),
            (craft_update::Os::FreeBsd, craft_update::Arch::X64),
        ] {
            let platform = craft_update::Platform { os, arch };
            assert!(craft_update::pick_asset(&release, &stems, platform, platform.default_kinds()).is_some(), "{os:?} {arch:?}");
        }
    }
}
