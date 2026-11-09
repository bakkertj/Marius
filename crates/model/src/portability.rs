//! Whether a vault's filenames survive being opened on another platform.
//!
//! Requirements: KB-004 (portable UTF-8 files and readable directory
//! structures), SYNC-003/SYNC-005 (the same vault on several devices), AC-013.
//!
//! macOS and Linux permit filenames that Windows cannot open at all. A vault
//! authored on a Mac and then opened at work on Windows is a first-class use
//! case, so the mismatch has to be handled deliberately rather than discovered
//! as a mystery failure.
//!
//! Three different behaviours, because the three situations are different:
//!
//! * **Indexing an existing file: always allowed.** A note the user already has
//!   must never be hidden from them because of its name. If Windows cannot open
//!   it, that is worth reporting — but silently omitting someone's notes from
//!   their own search results would be far worse.
//! * **Creating or renaming: refused.** Marius will not *author* a name that
//!   breaks on a platform the user has said they use.
//! * **A vault-wide report:** `marius doctor` lists what would break, so a
//!   problem can be fixed before the file reaches the other machine rather
//!   than after.
//!
//! The rules are Windows' own, and they are checked on every platform — a check
//! that only ran on Windows would be useless, since the file is created on the
//! Mac.

use crate::document::VaultPath;

/// Why a path would not survive on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Portability {
    /// A path component matches an MS-DOS device name, which Windows reserves
    /// at every directory level and with any extension — `aux.md` and
    /// `notes/con.md` are both unopenable.
    ReservedName {
        component: String,
        reserved: &'static str,
    },
    /// A character Windows forbids in a filename. `:` is the one that bites in
    /// practice, because it appears in email subjects and in times.
    IllegalCharacter { component: String, character: char },
    /// Windows silently strips trailing dots and spaces, so the file would be
    /// created under a different name than the one asked for.
    TrailingDotOrSpace { component: String },
    /// Long enough that the default 260-character limit is likely to be hit
    /// once a vault root is prepended.
    TooLong { length: usize },
}

impl Portability {
    /// A one-line explanation for a user.
    pub fn explain(&self) -> String {
        match self {
            Self::ReservedName { component, reserved } => format!(
                "{component:?} uses the reserved device name {reserved:?}, which Windows cannot open"
            ),
            Self::IllegalCharacter { component, character } => format!(
                "{component:?} contains {character:?}, which Windows does not allow in a filename"
            ),
            Self::TrailingDotOrSpace { component } => format!(
                "{component:?} ends in a dot or a space, which Windows silently removes"
            ),
            Self::TooLong { length } => format!(
                "the path is {length} characters, close to Windows' 260-character limit"
            ),
        }
    }

    /// A name that would work, for suggesting a rename.
    pub fn suggestion(&self) -> Option<String> {
        match self {
            // The suffix goes before the extension: `aux.md` becomes
            // `aux-note.md`, not `aux.md-note`.
            Self::ReservedName { component, .. } => Some(match component.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{stem}-note.{ext}"),
                _ => format!("{component}-note"),
            }),
            Self::IllegalCharacter { component, .. } => Some(sanitize(component)),
            Self::TrailingDotOrSpace { component } => {
                Some(component.trim_end_matches(['.', ' ']).to_owned())
            }
            Self::TooLong { .. } => None,
        }
    }
}

/// Characters Windows forbids in a filename.
///
/// `/` is excluded because a `VaultPath` has already split on it.
const ILLEGAL: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\\'];

/// MS-DOS device names, reserved at every directory level.
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Conservative length budget for the vault-relative part of a path.
///
/// Windows' classic limit is 260 characters for the *whole* path, and a vault
/// root like `C:\Users\someone\Documents\notes\` eats 40 of them before any
/// note is named. Long paths can be enabled on Windows 10 and later, but not
/// every tool that touches the vault honours them.
pub const WINDOWS_SAFE_RELATIVE_LENGTH: usize = 200;

/// Every reason `path` would not survive on Windows. Empty means it travels.
pub fn windows_issues(path: &VaultPath) -> Vec<Portability> {
    let mut issues = Vec::new();

    for component in path.components() {
        let stem = component.split('.').next().unwrap_or(component);
        if let Some(reserved) = RESERVED.iter().find(|r| stem.eq_ignore_ascii_case(r)) {
            issues.push(Portability::ReservedName {
                component: component.to_owned(),
                reserved,
            });
        }
        if let Some(character) = component
            .chars()
            .find(|c| ILLEGAL.contains(c) || c.is_control())
        {
            issues.push(Portability::IllegalCharacter {
                component: component.to_owned(),
                character,
            });
        }
        if component.ends_with('.') || component.ends_with(' ') {
            issues.push(Portability::TrailingDotOrSpace {
                component: component.to_owned(),
            });
        }
    }

    let length = path.as_str().chars().count();
    if length > WINDOWS_SAFE_RELATIVE_LENGTH {
        issues.push(Portability::TooLong { length });
    }
    issues
}

/// True when `path` opens on every platform Marius targets.
pub fn travels_everywhere(path: &VaultPath) -> bool {
    windows_issues(path).is_empty()
}

/// Replace the characters Windows forbids with a hyphen.
///
/// Used for suggestions and for deriving a filename from a title — an email
/// subject like `Re: harbour survey` becomes `Re- harbour survey`, which is
/// legible and opens anywhere.
///
/// This takes a **single filename component**, not a path, which is why `/` is
/// replaced here while [`windows_issues`] ignores it: a `VaultPath` has already
/// split on `/`, but a title that is about to become a filename has not.
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ILLEGAL.contains(&ch) || ch == '/' || ch.is_control() {
            // Collapse a run of illegal characters into one hyphen.
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim_matches(['.', ' ', '-']).trim();
    // A name made only of separators carries nothing to keep.
    if trimmed.is_empty() {
        return "untitled".to_owned();
    }
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return format!("{trimmed}-note");
    }
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> VaultPath {
        VaultPath::new(s).unwrap()
    }

    #[test]
    fn ordinary_paths_travel() {
        for ok in [
            "notes/alexandria.md",
            "research/Marius Michel Pasha.md",
            "daily/2026-11-09.md",
            "灯台/notes.md",
            "a-b_c.d.md",
        ] {
            assert!(travels_everywhere(&path(ok)), "{ok} should travel");
        }
    }

    #[test]
    fn a_colon_is_caught_because_email_subjects_contain_them() {
        // The case that actually bites: AC-011 captures email, and a subject
        // line routinely contains a colon.
        let issues = windows_issues(&path("inbox/Re: harbour survey.md"));
        assert_eq!(
            issues,
            vec![Portability::IllegalCharacter {
                component: "Re: harbour survey.md".into(),
                character: ':',
            }]
        );
        assert!(issues[0].explain().contains("Windows does not allow"));
        assert_eq!(
            issues[0].suggestion().as_deref(),
            Some("Re- harbour survey.md")
        );
    }

    #[test]
    fn every_illegal_windows_character_is_caught() {
        for character in ['<', '>', ':', '"', '|', '?', '*'] {
            let name = format!("notes/bad{character}name.md");
            // `\` is normalised to a separator by VaultPath, so it cannot
            // appear inside a component and is tested separately below.
            let Ok(candidate) = VaultPath::new(&name) else {
                continue;
            };
            let issues = windows_issues(&candidate);
            assert!(
                issues
                    .iter()
                    .any(|i| matches!(i, Portability::IllegalCharacter { character: c, .. } if *c == character)),
                "{character:?} was not caught in {name}"
            );
        }
    }

    #[test]
    fn reserved_device_names_are_caught_with_any_extension_at_any_depth() {
        for name in [
            "aux.md",
            "notes/con.md",
            "NUL.md",
            "deep/folder/com1.txt",
            "prn",
        ] {
            let issues = windows_issues(&path(name));
            assert!(
                issues
                    .iter()
                    .any(|i| matches!(i, Portability::ReservedName { .. })),
                "{name} should be flagged"
            );
        }
    }

    #[test]
    fn a_name_merely_containing_a_reserved_word_is_fine() {
        // `console.md` and `auxiliary.md` are perfectly legal.
        for name in [
            "console.md",
            "auxiliary.md",
            "notes/nullable.md",
            "com10.md",
        ] {
            assert!(travels_everywhere(&path(name)), "{name} should be allowed");
        }
    }

    #[test]
    fn trailing_dots_and_spaces_are_caught() {
        // Windows strips them, so the file would not be where Marius put it.
        for name in ["notes/draft..md", "notes/trailing.", "notes/space .md"] {
            let Ok(candidate) = VaultPath::new(name) else {
                continue;
            };
            let issues = windows_issues(&candidate);
            // `VaultPath` trims whole components, so only an interior trailing
            // dot survives to be flagged here.
            if name.ends_with('.') {
                assert!(
                    issues
                        .iter()
                        .any(|i| matches!(i, Portability::TrailingDotOrSpace { .. })),
                    "{name} should be flagged, got {issues:?}"
                );
            }
        }
    }

    #[test]
    fn an_over_long_path_is_flagged() {
        let long = format!("notes/{}.md", "a".repeat(220));
        let issues = windows_issues(&path(&long));
        assert!(issues
            .iter()
            .any(|i| matches!(i, Portability::TooLong { .. })));
        assert!(issues[0].explain().contains("260"));
    }

    #[test]
    fn sanitizing_produces_a_name_that_travels() {
        for raw in [
            "Re: harbour survey",
            "Q1/Q2 review",
            "what? really!",
            "aux",
            "CON",
            "trailing dots...",
            "///",
            "-",
            "a<b>c:d\"e|f?g*h",
        ] {
            let clean = sanitize(raw);
            let candidate = VaultPath::new(format!("{clean}.md"))
                .unwrap_or_else(|e| panic!("{raw:?} -> {clean:?}: {e}"));
            assert!(
                travels_everywhere(&candidate),
                "{raw:?} sanitised to {clean:?}, which still does not travel: {:?}",
                windows_issues(&candidate)
            );
        }
    }

    #[test]
    fn sanitizing_keeps_the_name_legible() {
        assert_eq!(sanitize("Re: harbour survey"), "Re- harbour survey");
        assert_eq!(sanitize("Q1/Q2 review"), "Q1-Q2 review");
        assert_eq!(sanitize("The Pharos"), "The Pharos");
        assert_eq!(sanitize("灯台"), "灯台");
    }

    #[test]
    fn sanitizing_never_returns_an_empty_name() {
        assert_eq!(sanitize("***"), "untitled");
        assert_eq!(sanitize("   "), "untitled");
        assert_eq!(sanitize(""), "untitled");
    }

    #[test]
    fn a_reserved_name_is_sanitised_rather_than_rejected() {
        assert_eq!(sanitize("aux"), "aux-note");
        assert_eq!(sanitize("COM1"), "COM1-note");
    }

    #[test]
    fn a_suggestion_keeps_the_extension_where_it_belongs() {
        let issues = windows_issues(&path("aux.md"));
        assert_eq!(issues[0].suggestion().as_deref(), Some("aux-note.md"));

        let issues = windows_issues(&path("notes/com1.txt"));
        assert_eq!(issues[0].suggestion().as_deref(), Some("com1-note.txt"));

        // And every suggestion actually travels.
        for name in ["aux.md", "notes/con.md", "inbox/Re: x.md", "nul"] {
            let candidate = path(name);
            let suggestion = windows_issues(&candidate)[0]
                .suggestion()
                .expect("a suggestion");
            let fixed = match candidate.parent() {
                Some(folder) => VaultPath::new(format!("{folder}/{suggestion}")).unwrap(),
                None => VaultPath::new(&suggestion).unwrap(),
            };
            assert!(
                travels_everywhere(&fixed),
                "{name} -> {suggestion} still does not travel: {:?}",
                windows_issues(&fixed)
            );
        }
    }
}
