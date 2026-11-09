//! Tags: canonical names, hierarchy, aliases and assignments.
//!
//! Requirements: TAG-001 .. TAG-016, AC-004, AC-005, AC-006, integrity rule 4.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{DocumentId, TagId, VaultId};
use crate::provenance::{Confidence, Provenance, ReviewState};

/// A normalised tag name.
///
/// Tags arrive from inline `#tags`, YAML frontmatter, the UI and models, in
/// inconsistent case and with inconsistent separators. Normalising at the type
/// boundary is what makes TAG-008 ("prefer existing canonical tags over
/// near-duplicate AI tags") work: two spellings of the same idea collapse
/// before they ever reach storage.
///
/// Rules: lower-cased; `/` separates hierarchy levels (TAG-010); runs of
/// whitespace, `_` and `-` inside a level become a single `-`; empty levels are
/// dropped.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TagName(String);

impl TagName {
    pub fn new(raw: impl AsRef<str>) -> Result<Self, TagNameError> {
        let raw = raw.as_ref();
        let mut levels: Vec<String> = Vec::new();
        for level in raw.split('/') {
            let mut out = String::with_capacity(level.len());
            let mut pending_sep = false;
            for ch in level.chars() {
                if ch.is_whitespace() || ch == '_' || ch == '-' {
                    // Collapse separator runs, and never emit a leading one.
                    pending_sep = !out.is_empty();
                } else if ch.is_alphanumeric() || matches!(ch, '+' | '#' | '.' | '&') {
                    if pending_sep {
                        out.push('-');
                        pending_sep = false;
                    }
                    out.extend(ch.to_lowercase());
                } else {
                    // Anything else (punctuation, symbols) acts as a separator
                    // rather than being silently deleted, so "C#/.NET" and
                    // "read:later" both stay legible.
                    pending_sep = !out.is_empty();
                }
            }
            if !out.is_empty() {
                levels.push(out);
            }
        }
        if levels.is_empty() {
            return Err(TagNameError::Empty(raw.to_owned()));
        }
        Ok(Self(levels.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Hierarchy levels, outermost first (TAG-010).
    pub fn levels(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The leaf level, which is what the UI shows in a compact chip.
    pub fn leaf(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The parent tag, or `None` at the top level.
    pub fn parent(&self) -> Option<Self> {
        self.0
            .rsplit_once('/')
            .map(|(head, _)| Self(head.to_owned()))
    }

    /// Every ancestor of this tag, nearest first.
    ///
    /// Used so that tagging a note `project/marius/ui` also makes it findable
    /// under `project` without storing three assignments (TAG-010).
    pub fn ancestors(&self) -> Vec<Self> {
        let mut out = Vec::new();
        let mut current = self.parent();
        while let Some(tag) = current {
            current = tag.parent();
            out.push(tag);
        }
        out
    }

    /// True when `self` is `other` or lies beneath it.
    pub fn is_within(&self, other: &Self) -> bool {
        self == other
            || (self.0.starts_with(&other.0) && self.0.as_bytes().get(other.0.len()) == Some(&b'/'))
    }

    /// Depth in the hierarchy, starting at 1.
    pub fn depth(&self) -> usize {
        self.levels().count()
    }
}

impl std::fmt::Display for TagName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for TagName {
    type Err = TagNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagNameError {
    #[error("tag name {0:?} normalises to nothing usable")]
    Empty(String),
}

/// A tag's identity record.
///
/// The ID rather than the name is what assignments reference, so a global
/// rename or merge (TAG-010) is a single update and cannot orphan assignments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: TagId,
    /// `None` for a tag shared across every vault; set for a vault-scoped tag.
    pub vault: Option<VaultId>,
    pub canonical_name: TagName,
    /// Name as the user prefers to see it, preserving their capitalisation.
    pub display_name: String,
    /// Alternate spellings that resolve to this tag (TAG-010). AI-proposed tags
    /// are matched against these before a new tag is created (TAG-008).
    pub aliases: Vec<TagName>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl Tag {
    pub fn new(vault: Option<VaultId>, name: TagName) -> Self {
        Self {
            id: TagId::new(),
            vault,
            display_name: name.as_str().to_owned(),
            canonical_name: name,
            aliases: Vec::new(),
            created_at: OffsetDateTime::now_utc(),
        }
    }

    /// True when `name` is this tag's canonical name or one of its aliases.
    pub fn matches(&self, name: &TagName) -> bool {
        &self.canonical_name == name || self.aliases.contains(name)
    }
}

/// Where in a document a derived tag was found.
///
/// Only meaningful for [`Provenance::Derived`] assignments: it lets the indexer
/// remove an inline tag's assignment when the user deletes the `#tag` from the
/// body, without touching assignments from other sources (TAG-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagSource {
    /// An inline `#tag` in the Markdown body.
    Inline,
    /// A `tags:` entry in YAML frontmatter.
    Frontmatter,
    /// Held only in the database (TAG-009: the default for AI tags, so the
    /// source file is left untouched).
    Index,
}

impl TagSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Frontmatter => "frontmatter",
            Self::Index => "index",
        }
    }

    /// True when the assignment is a reflection of text in the file, and so is
    /// recomputed from the file on every index pass.
    pub const fn is_in_source_file(self) -> bool {
        matches!(self, Self::Inline | Self::Frontmatter)
    }

    pub const ALL: [TagSource; 3] = [Self::Inline, Self::Frontmatter, Self::Index];
}

impl std::fmt::Display for TagSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TagSource {
    type Err = UnknownTagSource;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "inline" => Ok(Self::Inline),
            "frontmatter" => Ok(Self::Frontmatter),
            "index" => Ok(Self::Index),
            other => Err(UnknownTagSource(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tag source {0:?}")]
pub struct UnknownTagSource(pub String);

/// One tag applied to one document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentTag {
    pub document: DocumentId,
    pub tag: TagId,
    pub provenance: Provenance,
    pub review: ReviewState,
    pub source: TagSource,
    /// Model confidence, present only for AI assignments (TAG-011).
    pub confidence: Option<Confidence>,
    /// Which model proposed it, for the audit trail (TAG-011, AI-015).
    pub model: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub assigned_at: OffsetDateTime,
}

impl DocumentTag {
    /// A tag the user applied by hand (TAG-001).
    pub fn manual(document: DocumentId, tag: TagId) -> Self {
        Self {
            document,
            tag,
            provenance: Provenance::Manual,
            review: ReviewState::Approved,
            source: TagSource::Index,
            confidence: None,
            model: None,
            assigned_at: OffsetDateTime::now_utc(),
        }
    }

    /// A tag the indexer read out of the file itself (TAG-004).
    pub fn derived(document: DocumentId, tag: TagId, source: TagSource) -> Self {
        Self {
            document,
            tag,
            provenance: Provenance::Derived,
            review: ReviewState::Unreviewed,
            source,
            confidence: None,
            model: None,
            assigned_at: OffsetDateTime::now_utc(),
        }
    }

    /// A tag a model proposed (TAG-005, TAG-009: `source` is `Index`, so the
    /// Markdown file is not modified).
    pub fn ai_generated(
        document: DocumentId,
        tag: TagId,
        model: impl Into<String>,
        confidence: Confidence,
    ) -> Self {
        Self {
            document,
            tag,
            provenance: Provenance::AiGenerated,
            review: ReviewState::Unreviewed,
            source: TagSource::Index,
            confidence: Some(confidence),
            model: Some(model.into()),
            assigned_at: OffsetDateTime::now_utc(),
        }
    }

    /// True when automatic classification may replace or remove this assignment.
    ///
    /// The single gate for TAG-006 and TAG-012: a review decision can pin an
    /// assignment even when its provenance would otherwise allow a rewrite.
    pub fn is_machine_writable(&self) -> bool {
        self.provenance.is_machine_writable() && !self.review.pins_assertion()
    }

    /// Record the user's acceptance, promoting an AI tag to confirmed (TAG-007).
    pub fn approve(&mut self) {
        self.review = ReviewState::Approved;
        if self.provenance == Provenance::AiGenerated {
            self.provenance = Provenance::AiConfirmed;
        }
    }
}

/// A tag the user rejected for a document.
///
/// Stored separately from the assignment so that the rejection outlives the
/// assignment's deletion and survives reindexing and sync (TAG-012, AC-006,
/// integrity rule 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedTag {
    pub document: DocumentId,
    pub tag: TagId,
    #[serde(with = "time::serde::rfc3339")]
    pub rejected_at: OffsetDateTime,
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(s: &str) -> TagName {
        TagName::new(s).unwrap()
    }

    #[test]
    fn names_normalise_case_and_separators() {
        assert_eq!(tag("Lighthouse").as_str(), "lighthouse");
        assert_eq!(tag("Knowledge Management").as_str(), "knowledge-management");
        assert_eq!(tag("knowledge_management").as_str(), "knowledge-management");
        assert_eq!(
            tag("knowledge---management").as_str(),
            "knowledge-management"
        );
        assert_eq!(tag("#lighthouse").as_str(), "#lighthouse");
    }

    #[test]
    fn differing_spellings_collapse_to_one_canonical_name() {
        // TAG-008: this is what stops a model inventing a near-duplicate tag.
        let variants = [
            "Knowledge Management",
            "knowledge management",
            "KNOWLEDGE_MANAGEMENT",
            "Knowledge-Management",
        ];
        let canonical: Vec<_> = variants.iter().map(|v| tag(v)).collect();
        assert!(canonical.windows(2).all(|w| w[0] == w[1]), "{canonical:?}");
    }

    #[test]
    fn hierarchy_is_preserved_and_navigable() {
        let t = tag("Project/Marius/UI Shell");
        assert_eq!(t.as_str(), "project/marius/ui-shell");
        assert_eq!(t.leaf(), "ui-shell");
        assert_eq!(t.depth(), 3);
        assert_eq!(t.parent().unwrap().as_str(), "project/marius");
        assert_eq!(
            t.ancestors()
                .iter()
                .map(TagName::as_str)
                .collect::<Vec<_>>(),
            vec!["project/marius", "project"]
        );
        assert!(t.is_within(&tag("project")));
        assert!(t.is_within(&t));
        assert!(!t.is_within(&tag("proj")));
    }

    #[test]
    fn empty_levels_collapse_rather_than_producing_blank_tags() {
        assert_eq!(tag("a//b/").as_str(), "a/b");
        assert_eq!(tag("/leading").as_str(), "leading");
        assert!(TagName::new("///").is_err());
        assert!(TagName::new("  ").is_err());
    }

    #[test]
    fn unusual_but_meaningful_characters_survive() {
        assert_eq!(tag("C#").as_str(), "c#");
        assert_eq!(tag("C++").as_str(), "c++");
        assert_eq!(tag("read:later").as_str(), "read-later");
        assert_eq!(tag(".NET/Core").as_str(), ".net/core");
    }

    #[test]
    fn non_latin_names_are_kept() {
        assert_eq!(tag("Leuchtturm").as_str(), "leuchtturm");
        assert_eq!(tag("灯台").as_str(), "灯台");
        assert_eq!(tag("Φάρος").as_str(), "φάρος");
    }

    #[test]
    fn aliases_resolve_to_the_same_tag() {
        let mut t = Tag::new(None, tag("knowledge-management"));
        t.aliases.push(tag("pkm"));
        assert!(t.matches(&tag("PKM")));
        assert!(t.matches(&tag("Knowledge Management")));
        assert!(!t.matches(&tag("lighthouse")));
    }

    #[test]
    fn manual_tags_are_never_machine_writable() {
        // TAG-006 / AC-005.
        let m = DocumentTag::manual(DocumentId::new(), TagId::new());
        assert!(!m.is_machine_writable());
        assert_eq!(m.provenance, Provenance::Manual);
    }

    #[test]
    fn unreviewed_ai_tags_are_rewritable_but_pinned_ones_are_not() {
        let mut t = DocumentTag::ai_generated(
            DocumentId::new(),
            TagId::new(),
            "llama3.2",
            Confidence::new(0.8),
        );
        assert!(t.is_machine_writable());

        t.review = ReviewState::Locked;
        assert!(
            !t.is_machine_writable(),
            "TAG-012: locking must pin the tag"
        );
    }

    #[test]
    fn approving_an_ai_tag_promotes_its_provenance() {
        // TAG-007: a confirmed AI tag is as durable as a manual one.
        let mut t = DocumentTag::ai_generated(
            DocumentId::new(),
            TagId::new(),
            "llama3.2",
            Confidence::new(0.6),
        );
        t.approve();
        assert_eq!(t.provenance, Provenance::AiConfirmed);
        assert!(!t.is_machine_writable());
    }

    #[test]
    fn ai_tags_default_to_index_storage_leaving_source_files_alone() {
        // TAG-009 / AC-005.
        let t = DocumentTag::ai_generated(
            DocumentId::new(),
            TagId::new(),
            "llama3.2",
            Confidence::CERTAIN,
        );
        assert_eq!(t.source, TagSource::Index);
        assert!(!t.source.is_in_source_file());
    }

    #[test]
    fn inline_and_frontmatter_sources_are_recomputed_from_the_file() {
        assert!(TagSource::Inline.is_in_source_file());
        assert!(TagSource::Frontmatter.is_in_source_file());
        for s in TagSource::ALL {
            assert_eq!(s, s.as_str().parse().unwrap());
        }
    }
}
