//! The people credited in the About window and counted by `app.info`.
//!
//! `build.rs` builds [`CONTRIBUTORS`] from `contributors/people.toml` and the optional
//! `contributors/contributors.json` when the crate is compiled. Each entry is sorted by username
//! without case.

/// One contributor: a GitHub username and the names the person chose in their own
/// `contributors/people.toml` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contributor {
    pub login: &'static str,
    pub real_name: Option<&'static str>,
    pub display_name: Option<&'static str>,
}

include!(concat!(env!("OUT_DIR"), "/credits.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_by_username_and_names_are_not_empty() {
        let keys: Vec<String> = CONTRIBUTORS.iter().map(|c| c.login.to_ascii_lowercase()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(keys, sorted);
        for c in CONTRIBUTORS {
            assert!(!c.login.is_empty());
            assert!(c.real_name.is_none_or(|n| !n.trim().is_empty()), "{c:?}");
            assert!(c.display_name.is_none_or(|n| !n.trim().is_empty()), "{c:?}");
        }
    }
}
