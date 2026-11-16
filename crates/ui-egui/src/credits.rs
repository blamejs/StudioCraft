//! The Credits section of the About window: the contributor list in the name style the viewer
//! picks, and the license texts.

use studiocraft_engine::credits::Contributor;

/// The MIT license text, from the repository root.
pub const LICENSE_MIT: &str = include_str!("../../../LICENSE-MIT");
/// The Apache License 2.0 text, from the repository root.
pub const LICENSE_APACHE: &str = include_str!("../../../LICENSE-APACHE");
/// The SIL Open Font License 1.1 text that ships with the embedded Inter faces.
pub const LICENSE_INTER: &str = include_str!("../../../assets/fonts/OFL-Inter.txt");
/// The SIL Open Font License 1.1 text that ships with the embedded JetBrains Mono face.
pub const LICENSE_JETBRAINS_MONO: &str = include_str!("../../../assets/fonts/OFL-JetBrainsMono.txt");

/// Which name the Credits list shows for each person.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NameMode {
    /// The GitHub username.
    #[default]
    Username,
    /// The display name from the person's entry, or the username.
    Display,
    /// The real name from the person's entry, or the username.
    Real,
}

impl NameMode {
    pub const ALL: [NameMode; 3] = [NameMode::Username, NameMode::Display, NameMode::Real];

    pub fn label(self) -> &'static str {
        match self {
            NameMode::Username => "Username",
            NameMode::Display => "Display name",
            NameMode::Real => "Real name",
        }
    }
}

/// The name `c` is shown by in `mode`: the chosen name when the person gave one, else the
/// username.
pub fn name(c: &Contributor, mode: NameMode) -> &'static str {
    match mode {
        NameMode::Username => None,
        NameMode::Display => c.display_name,
        NameMode::Real => c.real_name,
    }
    .unwrap_or(c.login)
}

/// `list` sorted by the name shown in `mode` (without case), then by username.
pub fn sorted(list: &[Contributor], mode: NameMode) -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&'static str, &'static str)> = list.iter().map(|c| (name(c, mode), c.login)).collect();
    rows.sort_by_cached_key(|(shown, login)| (shown.to_lowercase(), login.to_ascii_lowercase()));
    rows
}

/// Which license the Licenses window shows: one of StudioCraft's two licenses, or the license
/// of an interface font built into the app.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum License {
    #[default]
    Mit,
    Apache,
    Inter,
    JetBrainsMono,
}

impl License {
    /// Every license the Licenses window offers, in the order it lists them.
    pub const ALL: [License; 4] = [License::Mit, License::Apache, License::Inter, License::JetBrainsMono];

    pub fn text(self) -> &'static str {
        match self {
            License::Mit => LICENSE_MIT,
            License::Apache => LICENSE_APACHE,
            License::Inter => LICENSE_INTER,
            License::JetBrainsMono => LICENSE_JETBRAINS_MONO,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            License::Mit => "MIT",
            License::Apache => "Apache 2.0",
            License::Inter => "Inter (SIL OFL 1.1)",
            License::JetBrainsMono => "JetBrains Mono (SIL OFL 1.1)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: [Contributor; 4] = [
        Contributor { login: "zed", real_name: Some("Adam Zed"), display_name: None },
        Contributor { login: "amy", real_name: None, display_name: Some("Moon") },
        Contributor { login: "Bob", real_name: Some("bob Real"), display_name: Some("bobby") },
        Contributor { login: "carl", real_name: None, display_name: None },
    ];

    #[test]
    fn names_fall_back_to_the_username() {
        assert_eq!(name(&LIST[0], NameMode::Username), "zed");
        assert_eq!(name(&LIST[0], NameMode::Real), "Adam Zed");
        assert_eq!(name(&LIST[0], NameMode::Display), "zed");
        assert_eq!(name(&LIST[1], NameMode::Real), "amy");
        assert_eq!(name(&LIST[1], NameMode::Display), "Moon");
        assert_eq!(name(&LIST[3], NameMode::Real), "carl");
    }

    #[test]
    fn lists_sort_by_the_shown_name_without_case() {
        let shown = |m| sorted(&LIST, m).into_iter().map(|(s, _)| s).collect::<Vec<_>>();
        assert_eq!(shown(NameMode::Username), ["amy", "Bob", "carl", "zed"]);
        assert_eq!(shown(NameMode::Real), ["Adam Zed", "amy", "bob Real", "carl"]);
        assert_eq!(shown(NameMode::Display), ["bobby", "carl", "Moon", "zed"]);
        assert!(sorted(&[], NameMode::Real).is_empty());
        let same = [
            Contributor { login: "b", real_name: Some("Sam"), display_name: None },
            Contributor { login: "a", real_name: Some("sam"), display_name: None },
        ];
        assert_eq!(sorted(&same, NameMode::Real), [("sam", "a"), ("Sam", "b")], "ties go by username");
    }

    #[test]
    fn license_texts_are_the_repository_files() {
        assert!(License::Mit.text().contains("MIT License") || License::Mit.text().contains("Permission is hereby granted"));
        assert!(License::Apache.text().contains("Apache License"));
        for font in [License::Inter, License::JetBrainsMono] {
            assert!(font.text().contains("SIL OPEN FONT LICENSE"), "{font:?}");
        }
        assert!(License::Inter.text().contains("The Inter Project Authors"));
        assert!(License::JetBrainsMono.text().contains("The JetBrains Mono Project Authors"));
        assert_eq!(License::ALL.map(License::label), ["MIT", "Apache 2.0", "Inter (SIL OFL 1.1)", "JetBrains Mono (SIL OFL 1.1)"]);
        assert_eq!(NameMode::ALL.map(NameMode::label), ["Username", "Display name", "Real name"]);
    }
}
