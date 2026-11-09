//! Document revision history.
//!
//! Requirements: KB-011 (maintain revision history and recover earlier
//! revisions), AI-013 (regeneration creates a new revision rather than
//! replacing a user's edits), NFR-001, integrity rule 6.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{ContentHash, DocumentId, RevisionId};

/// What caused a revision to be recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionCause {
    /// A person saved the document in Marius.
    Edit,
    /// The file changed on disk outside the application (KB-005). Captured so
    /// an external tool's edit can still be rolled back.
    ExternalChange,
    /// The document was created, or first seen by the indexer.
    Created,
    /// Content arrived through an importer (IM-001 .. IM-011).
    Import,
    /// A model wrote this revision of a derived note (AI-011, AI-013).
    Generation,
    /// The user rolled back to an earlier revision; recorded as a new revision
    /// rather than by deleting history.
    Restore,
    /// A rename or move. The body is unchanged, but the path is not, and
    /// AC-003 requires the identity trail to show it.
    Relocation,
}

impl RevisionCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::ExternalChange => "external_change",
            Self::Created => "created",
            Self::Import => "import",
            Self::Generation => "generation",
            Self::Restore => "restore",
            Self::Relocation => "relocation",
        }
    }

    /// True when a person authored the content of this revision. Regeneration
    /// must never discard such a revision (integrity rule 6).
    pub const fn is_user_authored(self) -> bool {
        matches!(self, Self::Edit | Self::ExternalChange | Self::Restore)
    }

    pub const ALL: [RevisionCause; 7] = [
        Self::Edit,
        Self::ExternalChange,
        Self::Created,
        Self::Import,
        Self::Generation,
        Self::Restore,
        Self::Relocation,
    ];
}

impl std::fmt::Display for RevisionCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RevisionCause {
    type Err = UnknownRevisionCause;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "edit" => Ok(Self::Edit),
            "external_change" => Ok(Self::ExternalChange),
            "created" => Ok(Self::Created),
            "import" => Ok(Self::Import),
            "generation" => Ok(Self::Generation),
            "restore" => Ok(Self::Restore),
            "relocation" => Ok(Self::Relocation),
            other => Err(UnknownRevisionCause(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown revision cause {0:?}")]
pub struct UnknownRevisionCause(pub String);

/// One recorded state of a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentRevision {
    pub id: RevisionId,
    pub document: DocumentId,
    /// Monotonic per document, starting at 1. Used as the sync revision
    /// counter too, so conflict detection and history share one clock
    /// (SYNC-009, SYNC-010).
    pub sequence: u64,
    pub cause: RevisionCause,
    pub content_hash: ContentHash,
    pub size_bytes: u64,
    /// Path at the time of the revision, so a restore can also restore location.
    pub path: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Short description, e.g. the model and prompt for a generated revision.
    pub note: Option<String>,
}

impl DocumentRevision {
    pub fn new(
        document: DocumentId,
        sequence: u64,
        cause: RevisionCause,
        content_hash: ContentHash,
        size_bytes: u64,
        path: impl Into<String>,
    ) -> Self {
        Self {
            id: RevisionId::new(),
            document,
            sequence,
            cause,
            content_hash,
            size_bytes,
            path: path.into(),
            created_at: OffsetDateTime::now_utc(),
            note: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_authored_causes_are_identified() {
        // Integrity rule 6: regeneration keys off this predicate.
        assert!(RevisionCause::Edit.is_user_authored());
        assert!(RevisionCause::ExternalChange.is_user_authored());
        assert!(RevisionCause::Restore.is_user_authored());
        assert!(!RevisionCause::Generation.is_user_authored());
        assert!(!RevisionCause::Import.is_user_authored());
    }

    #[test]
    fn revisions_carry_the_path_they_were_taken_at() {
        let rev = DocumentRevision::new(
            DocumentId::new(),
            1,
            RevisionCause::Created,
            ContentHash::of(b"x"),
            1,
            "notes/a.md",
        );
        assert_eq!(rev.path, "notes/a.md");
        assert_eq!(rev.sequence, 1);
    }

    #[test]
    fn cause_tokens_round_trip() {
        for c in RevisionCause::ALL {
            assert_eq!(c, c.as_str().parse().unwrap());
        }
    }
}
