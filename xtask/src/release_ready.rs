//! `cargo xtask release-ready` counts the pull requests merged into `main` since the previous alpha
//! tag and exits 0 only when there are at least [`MIN_PULL_REQUESTS`] of them.
//!
//! The previous alpha tag is the newest `v*-alpha.*` tag that is an ancestor of the counted
//! revision. With no such tag, every pull request merged into `main` counts. A pull request counts
//! when its merge commit is in `<tag>..<revision>` (`git rev-list`). The list comes from every page
//! of `gh api repos/{owner}/{repo}/pulls?state=closed&base=main`, so `gh` must be signed in, or
//! `GH_TOKEN` set.
//!
//! ```text
//! cargo xtask release-ready                          # counts up to origin/main
//! cargo xtask release-ready --ref HEAD --tag v0.1.0-alpha.2 --notes notes.md
//! ```
//!
//! `--ref` names the revision to count up to (default `origin/main`). `--tag` names the tag being
//! released, which is never taken as the previous one. `--notes` writes the release notes from
//! `packaging/release-notes.md`. The exit code is 0 when ready, 1 when fewer than
//! [`MIN_PULL_REQUESTS`] have merged, and 2 on an error.

use std::collections::HashSet;
use std::process::{Command, ExitCode};

use crate::{root, scan, version};

/// The pull requests an alpha release needs since the previous alpha tag.
pub const MIN_PULL_REQUESTS: usize = 50;

/// Every closed pull request into `main`, one page after another, as one JSON object per line for
/// each merged one.
const PULLS_ENDPOINT: &str = "repos/{owner}/{repo}/pulls?state=closed&base=main&per_page=100";
const PULLS_JQ: &str = ".[] | select(.merged_at != null) | {number, title, author: .user.login, merge_commit: .merge_commit_sha}";

/// The notes template, relative to the workspace root.
const NOTES_TEMPLATE: &str = "packaging/release-notes.md";

const USAGE: &str = "usage: cargo xtask release-ready [--ref <revision>] [--tag <tag being released>] [--notes <file>]";

/// A pull request merged into `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub merge_commit: String,
}

struct Options {
    revision: String,
    tag: Option<String>,
    notes: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options { revision: "origin/main".into(), tag: None, notes: None };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{arg} needs a value\n{USAGE}"));
        match arg.as_str() {
            "--ref" => options.revision = value()?,
            "--tag" => options.tag = Some(value()?),
            "--notes" => options.notes = Some(value()?),
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    Ok(options)
}

/// Runs the command and returns its exit code.
pub fn main(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<bool, String> {
    let options = parse_args(args)?;
    let revision = output("git", &["rev-parse", "--verify", &format!("{}^{{commit}}", options.revision)])?.trim().to_string();
    let tags = output("git", &["tag", "--list", "v*-alpha.*", "--sort=-v:refname"])?;
    let previous = previous_tag(&tags, options.tag.as_deref(), |tag| {
        Command::new("git").current_dir(root()).args(["merge-base", "--is-ancestor", tag, &revision]).status().is_ok_and(|s| s.success())
    });
    let range = match &previous {
        Some(tag) => format!("{tag}..{revision}"),
        None => revision.clone(),
    };
    let commits: HashSet<String> = output("git", &["rev-list", &range])?.lines().map(str::to_string).collect();
    let lines = output("gh", &["api", "--paginate", PULLS_ENDPOINT, "--jq", PULLS_JQ])?;
    let merged = merged_in_range(parse_pull_requests(&lines)?, &commits);

    let since = previous.as_deref().unwrap_or("the start of the repository");
    println!("{} pull requests merged into main since {since} (an alpha needs {MIN_PULL_REQUESTS}):", merged.len());
    print!("{}", pull_request_list(&merged));
    let ready = is_ready(merged.len());
    println!("{}", if ready { "ready for an alpha release" } else { "not ready for an alpha release" });

    if let Some(path) = &options.notes {
        let template = std::fs::read_to_string(root().join(NOTES_TEMPLATE)).map_err(|e| format!("{NOTES_TEMPLATE}: {e}"))?;
        let manifest = std::fs::read_to_string(root().join("Cargo.toml")).map_err(|e| format!("Cargo.toml: {e}"))?;
        let version = version::read(&manifest)?;
        let tag = options.tag.clone().unwrap_or_else(|| format!("v{version}"));
        let repo = match std::env::var("GH_REPO") {
            Ok(repo) if !repo.is_empty() => repo,
            _ => output("gh", &["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"])?.trim().to_string(),
        };
        let notes = render_notes(&template, &version, &tag, previous.as_deref(), &repo, &merged);
        let named = scan::brand_hits(&notes);
        if !named.is_empty() {
            let lines: Vec<String> = named.iter().map(|(line, name)| format!("line {line}: {name}")).collect();
            return Err(format!("the release notes name other products:\n  {}", lines.join("\n  ")));
        }
        std::fs::write(path, notes).map_err(|e| format!("{path}: {e}"))?;
        println!("wrote the release notes to {path}");
    }
    Ok(ready)
}

/// The standard output of `program args` run in the workspace root, which must exit successfully.
fn output(program: &str, args: &[&str]) -> Result<String, String> {
    let what = format!("{program} {}", args.join(" "));
    let out = Command::new(program).current_dir(root()).args(args).output().map_err(|e| format!("{what}: could not start: {e}"))?;
    if !out.status.success() {
        return Err(format!("{what}: exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{what}: output is not UTF-8: {e}"))
}

/// Whether `count` merged pull requests are enough for an alpha release.
pub fn is_ready(count: usize) -> bool {
    count >= MIN_PULL_REQUESTS
}

/// The first alpha tag in `tags` (one per line, newest first) other than `current` for which
/// `is_ancestor` holds.
pub fn previous_tag(tags: &str, current: Option<&str>, is_ancestor: impl Fn(&str) -> bool) -> Option<String> {
    tags.lines()
        .map(str::trim)
        .filter(|t| t.starts_with('v') && t.contains("-alpha."))
        .filter(|t| Some(*t) != current)
        .find(|t| is_ancestor(t))
        .map(str::to_string)
}

/// The pull requests in the output of [`PULLS_JQ`]: one JSON object per line with `number`,
/// `title`, `author` and `merge_commit`.
pub fn parse_pull_requests(lines: &str) -> Result<Vec<PullRequest>, String> {
    lines
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let item: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("gh api pulls: {e}: {line}"))?;
            let number = item["number"].as_u64().ok_or_else(|| format!("gh api pulls: no pull request number in {line}"))?;
            let title = item["title"].as_str().unwrap_or_default().to_string();
            let author = item["author"].as_str().unwrap_or_default().to_string();
            let merge_commit = item["merge_commit"].as_str().ok_or_else(|| format!("gh api pulls: #{number} has no merge commit"))?.to_string();
            Ok(PullRequest { number, title, author, merge_commit })
        })
        .collect()
}

/// The pull requests whose merge commit is in `commits`, by number.
pub fn merged_in_range(prs: Vec<PullRequest>, commits: &HashSet<String>) -> Vec<PullRequest> {
    let mut merged: Vec<PullRequest> = prs.into_iter().filter(|pr| commits.contains(&pr.merge_commit)).collect();
    merged.sort_by_key(|pr| pr.number);
    merged
}

/// One ``- #N `title` by @author`` line for each pull request, with the title written by
/// [`code_span`].
pub fn pull_request_list(prs: &[PullRequest]) -> String {
    prs.iter()
        .map(|pr| {
            let mut line = format!("- #{}", pr.number);
            let title = code_span(&pr.title);
            if !title.is_empty() {
                line.push(' ');
                line.push_str(&title);
            }
            if !pr.author.is_empty() {
                line.push_str(" by @");
                line.push_str(&pr.author);
            }
            line.push('\n');
            line
        })
        .collect()
}

/// `text` as a Markdown code span on one line, so GitHub shows it as written: no emphasis, links,
/// HTML, entities, mentions or references. Each control character, line breaks included, becomes
/// a space. The fence is one backtick longer than the longest run of backticks in `text`, with a
/// space inside each end when `text` starts or ends with a backtick or a space. Text that is empty
/// or only spaces gives an empty string.
pub fn code_span(text: &str) -> String {
    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if text.trim_matches(' ').is_empty() {
        return String::new();
    }
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with(['`', ' ']) || text.ends_with(['`', ' ']) { " " } else { "" };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// `template` with `{{version}}`, `{{count}}`, `{{since}}`, `{{pull_requests}}` and `{{changes}}`
/// filled in. `{{changes}}` is the compare link from the previous tag, or the commit list of `tag`
/// for the first alpha. `{{pull_requests}}` is filled in last, so a placeholder in a title stays
/// as written.
pub fn render_notes(template: &str, version: &str, tag: &str, previous: Option<&str>, repo: &str, prs: &[PullRequest]) -> String {
    let since = previous.map_or_else(|| "the start of the repository".to_string(), |p| p.to_string());
    let changes = match previous {
        Some(p) => format!("https://github.com/{repo}/compare/{p}...{tag}"),
        None => format!("https://github.com/{repo}/commits/{tag}"),
    };
    template
        .replace("{{version}}", version)
        .replace("{{count}}", &prs.len().to_string())
        .replace("{{since}}", &since)
        .replace("{{changes}}", &changes)
        .replace("{{pull_requests}}", pull_request_list(prs).trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, commit: &str) -> PullRequest {
        PullRequest { number, title: format!("Change {number}"), author: "someone".into(), merge_commit: commit.into() }
    }

    fn commits(list: &[&str]) -> HashSet<String> {
        list.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn ready_at_fifty_and_above() {
        assert!(!is_ready(0));
        assert!(!is_ready(MIN_PULL_REQUESTS - 1));
        assert!(is_ready(MIN_PULL_REQUESTS));
        assert!(is_ready(MIN_PULL_REQUESTS + 18));
    }

    #[test]
    fn the_previous_tag_is_the_newest_alpha_tag_in_the_history() {
        let tags = "v0.2.0-alpha.1\nv0.1.0-alpha.10\nv0.1.0-alpha.9\nv0.1.0\n";
        assert_eq!(previous_tag(tags, None, |_| true).as_deref(), Some("v0.2.0-alpha.1"));
        // The tag being released is skipped, and so is a tag on another branch.
        assert_eq!(previous_tag(tags, Some("v0.2.0-alpha.1"), |_| true).as_deref(), Some("v0.1.0-alpha.10"));
        assert_eq!(previous_tag(tags, None, |t| t != "v0.2.0-alpha.1").as_deref(), Some("v0.1.0-alpha.10"));
        // A tag that is not an alpha tag is never taken.
        assert_eq!(previous_tag("v0.1.0\nv0.1.0-rc.1\n", None, |_| true), None);
        assert_eq!(previous_tag("", None, |_| true), None);
    }

    #[test]
    fn reads_one_pull_request_per_line() {
        // Two pages of `gh api --paginate --jq`, which print one object per line.
        let lines = "{\"number\":68,\"title\":\"Outputs: a change\",\"author\":\"a\",\"merge_commit\":\"b116\"}\n\
                     {\"number\":67,\"title\":\"Docs: another\",\"author\":\"b\",\"merge_commit\":\"8bb5\"}\n\n\
                     {\"number\":3,\"title\":\"Release: x\",\"author\":null,\"merge_commit\":\"0460\"}\n";
        let prs = parse_pull_requests(lines).unwrap();
        assert_eq!(prs.iter().map(|p| p.number).collect::<Vec<_>>(), vec![68, 67, 3]);
        assert_eq!(prs[0], PullRequest { number: 68, title: "Outputs: a change".into(), author: "a".into(), merge_commit: "b116".into() });
        assert_eq!(prs[2].author, "");
        assert!(parse_pull_requests("").unwrap().is_empty());
        assert!(parse_pull_requests(r#"{"number":1,"title":"x","author":"a","merge_commit":null}"#).is_err());
        assert!(parse_pull_requests(r#"{"title":"x"}"#).is_err());
        assert!(parse_pull_requests("not json").is_err());
    }

    #[test]
    fn counts_only_pull_requests_merged_in_the_range() {
        let prs = vec![pr(4, "d"), pr(3, "c"), pr(2, "b"), pr(1, "a")];
        let merged = merged_in_range(prs, &commits(&["d", "b", "x"]));
        assert_eq!(merged.iter().map(|p| p.number).collect::<Vec<_>>(), vec![2, 4]);
        assert!(merged_in_range(vec![pr(1, "a")], &commits(&[])).is_empty());
    }

    #[test]
    fn notes_fill_every_field() {
        let template = "{{version}}: {{count}} since {{since}}\n{{pull_requests}}\n{{changes}}\n";
        let prs = vec![pr(1, "a"), pr(2, "b")];
        let first = render_notes(template, "0.1.0-alpha.1", "v0.1.0-alpha.1", None, "o/r", &prs);
        assert_eq!(
            first,
            "0.1.0-alpha.1: 2 since the start of the repository\n- #1 `Change 1` by @someone\n- #2 `Change 2` by @someone\n\
             https://github.com/o/r/commits/v0.1.0-alpha.1\n"
        );
        let next = render_notes(template, "0.1.0-alpha.2", "v0.1.0-alpha.2", Some("v0.1.0-alpha.1"), "o/r", &[]);
        assert_eq!(next, "0.1.0-alpha.2: 0 since v0.1.0-alpha.1\n\nhttps://github.com/o/r/compare/v0.1.0-alpha.1...v0.1.0-alpha.2\n");
    }

    #[test]
    fn code_span_holds_any_title() {
        assert_eq!(code_span("Outputs: a change"), "`Outputs: a change`");
        assert_eq!(code_span("<b>&amp; @someone #12 https://example.com</b>"), "`<b>&amp; @someone #12 https://example.com</b>`");
        // The fence is longer than any run of backticks inside, padded when an end is a backtick.
        assert_eq!(code_span("a `b` c"), "``a `b` c``");
        assert_eq!(code_span("``x``"), "``` ``x`` ```");
        assert_eq!(code_span("`"), "`` ` ``");
        assert_eq!(code_span(" lead"), "`  lead `");
        // Control characters, line breaks included, become spaces.
        assert_eq!(code_span("a\nb\r\nc\td\u{1b}[31m"), "`a b  c d [31m`");
        assert_eq!(code_span("\n## heading"), "`  ## heading `");
        assert_eq!(code_span("Übersicht: 日本語"), "`Übersicht: 日本語`");
        assert_eq!(code_span(""), "");
        assert_eq!(code_span(" \n "), "");
    }

    #[test]
    fn titles_are_plain_text_in_the_notes() {
        let mut hostile = pr(7, "a");
        hostile.title = "<img src=x onerror=alert(1)> [link](https://example.com) **bold** `code` &amp; @someone\r\n## heading".into();
        let template = "{{pull_requests}}\n{{changes}}\n";
        let notes = render_notes(template, "0.1.0-alpha.1", "v0.1.0-alpha.1", None, "o/r", &[hostile]);
        assert_eq!(
            notes,
            "- #7 ``<img src=x onerror=alert(1)> [link](https://example.com) **bold** `code` &amp; @someone  ## heading`` by @someone\n\
             https://github.com/o/r/commits/v0.1.0-alpha.1\n"
        );
        // A title that holds a placeholder is not filled in.
        let mut placeholder = pr(8, "b");
        placeholder.title = "{{changes}} {{version}}".into();
        let notes = render_notes(template, "0.1.0-alpha.1", "v0.1.0-alpha.1", None, "o/r", &[placeholder]);
        assert_eq!(notes, "- #8 `{{changes}} {{version}}` by @someone\nhttps://github.com/o/r/commits/v0.1.0-alpha.1\n");
        // An empty title leaves no empty code span.
        let mut empty = pr(9, "c");
        empty.title = String::new();
        assert_eq!(pull_request_list(&[empty]), "- #9 by @someone\n");
    }

    #[test]
    fn the_notes_template_has_every_field() {
        let template = std::fs::read_to_string(root().join(NOTES_TEMPLATE)).unwrap();
        for field in ["{{version}}", "{{count}}", "{{since}}", "{{pull_requests}}", "{{changes}}"] {
            assert!(template.contains(field), "{NOTES_TEMPLATE} has no {field}");
        }
        assert_eq!(scan::brand_hits(&template), Vec::new());
    }

    #[test]
    fn arguments() {
        let args: Vec<String> = ["--ref", "HEAD", "--tag", "v1.0.0-alpha.1", "--notes", "n.md"].iter().map(|s| s.to_string()).collect();
        let o = parse_args(&args).unwrap();
        assert_eq!((o.revision.as_str(), o.tag.as_deref(), o.notes.as_deref()), ("HEAD", Some("v1.0.0-alpha.1"), Some("n.md")));
        assert_eq!(parse_args(&[]).unwrap().revision, "origin/main");
        assert!(parse_args(&["--ref".to_string()]).is_err());
        assert!(parse_args(&["--other".to_string()]).is_err());
    }
}
