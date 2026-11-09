//! Where a piece of metadata came from, and who is allowed to change it.
//!
//! Requirements: TAG-006/TAG-007 (preserve manual tags; record provenance),
//! GRAPH-009/GRAPH-013 (relationship origin), AI-009 (AI may change derived
//! metadata only), ARCH-012, integrity rule 4.
//!
//! This module holds the single rule that protects user intent across the whole
//! application: automatic classification may only overwrite metadata it owns
//! itself. Every writer that touches tags or relationships consults
//! [`Provenance::is_machine_writable`] rather than re-deriving the policy.

use serde::{Deserialize, Serialize};

/// How a metadata assertion came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// A person asserted it directly in the application.
    Manual,
    /// It arrived with imported content — an Obsidian vault, frontmatter, an
    /// email header. Treated as user-authored: the user put it there, even if
    /// indirectly, so automatic classification must not remove it (IM-004).
    Imported,
    /// A deterministic, non-AI rule produced it (for example an inline `#tag`
    /// found in the Markdown body). Re-derived from source on every index pass,
    /// so the indexer owns it.
    Derived,
    /// A model proposed it and nobody has reviewed it (TAG-005, AI-010).
    AiGenerated,
    /// A model proposed it and a person accepted it. Now as durable as
    /// [`Provenance::Manual`] (TAG-007).
    AiConfirmed,
}

impl Provenance {
    /// True when automatic classification may replace or delete this assertion.
    ///
    /// This is the enforcement point for TAG-006, AI-009 and integrity rule 4:
    /// anything a person authored, imported or confirmed is off-limits to the
    /// classifier, while the classifier's own unreviewed output and the
    /// indexer's derived output can be recomputed freely.
    pub const fn is_machine_writable(self) -> bool {
        matches!(self, Self::Derived | Self::AiGenerated)
    }

    /// True when a person stands behind this assertion.
    pub const fn is_user_authored(self) -> bool {
        matches!(self, Self::Manual | Self::Imported | Self::AiConfirmed)
    }

    /// True when a model produced the assertion, confirmed or not. Drives the
    /// provenance badge in the UI (UI-010).
    pub const fn is_ai_originated(self) -> bool {
        matches!(self, Self::AiGenerated | Self::AiConfirmed)
    }

    /// Stable lower-case token used in storage, queries and the CLI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Imported => "imported",
            Self::Derived => "derived",
            Self::AiGenerated => "ai_generated",
            Self::AiConfirmed => "ai_confirmed",
        }
    }

    /// Every variant, for exhaustive UI filters and round-trip tests.
    pub const ALL: [Provenance; 5] = [
        Self::Manual,
        Self::Imported,
        Self::Derived,
        Self::AiGenerated,
        Self::AiConfirmed,
    ];
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Provenance {
    type Err = UnknownProvenance;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "manual" => Ok(Self::Manual),
            "imported" => Ok(Self::Imported),
            "derived" => Ok(Self::Derived),
            "ai_generated" => Ok(Self::AiGenerated),
            "ai_confirmed" => Ok(Self::AiConfirmed),
            other => Err(UnknownProvenance(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown provenance {0:?}")]
pub struct UnknownProvenance(pub String);

/// A person's explicit decision about a machine proposal.
///
/// Requirements: TAG-011 (track user approval), TAG-012 (rejection and locking
/// rules), GRAPH-010/GRAPH-011, AC-006, integrity rule 7.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    /// Nobody has looked at it yet.
    #[default]
    Unreviewed,
    /// Accepted. Promotes an AI assertion to [`Provenance::AiConfirmed`].
    Approved,
    /// Rejected. The assertion is removed *and* suppressed, so reindexing or a
    /// sync round cannot bring it back (integrity rule 7, GRAPH-011).
    Rejected,
    /// Pinned by the user. Automatic classification may not touch it even if the
    /// assertion originated from a model (TAG-012).
    Locked,
}

impl ReviewState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unreviewed => "unreviewed",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Locked => "locked",
        }
    }

    /// True when this review decision alone forbids machine writes, regardless
    /// of the underlying [`Provenance`].
    pub const fn pins_assertion(self) -> bool {
        matches!(self, Self::Approved | Self::Locked)
    }

    pub const ALL: [ReviewState; 4] = [
        Self::Unreviewed,
        Self::Approved,
        Self::Rejected,
        Self::Locked,
    ];
}

impl std::fmt::Display for ReviewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReviewState {
    type Err = UnknownReviewState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unreviewed" => Ok(Self::Unreviewed),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            "locked" => Ok(Self::Locked),
            other => Err(UnknownReviewState(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown review state {0:?}")]
pub struct UnknownReviewState(pub String);

/// Confidence reported by a model, clamped to `0.0..=1.0`.
///
/// Requirements: TAG-011, GRAPH-009 (optional confidence score).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Confidence(f32);

impl Confidence {
    pub const CERTAIN: Self = Self(1.0);

    /// Clamp an arbitrary score into range. Providers are not trusted to stay
    /// inside `0..1`, and a NaN would poison every later comparison and sort.
    pub fn new(value: f32) -> Self {
        if value.is_nan() {
            Self(0.0)
        } else {
            Self(value.clamp(0.0, 1.0))
        }
    }

    pub const fn get(self) -> f32 {
        self.0
    }
}

impl Eq for Confidence {}

#[allow(clippy::derive_ord_xor_partial_ord)]
impl Ord for Confidence {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Safe: `new` rejects NaN, so every stored value is totally ordered.
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.2}", self.0)
    }
}

/// Whether a document's body was written by a person or generated by a model.
///
/// Requirements: ARCH-013 and AI-012 (generated notes are first-class but
/// visibly identified and filterable), AI-014 (promotion to user-maintained).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Authorship {
    /// A person's document. Its body is never modified by the application
    /// without an explicit, user-initiated action (ARCH-012, AI-009).
    #[default]
    Source,
    /// A summary or synthesis note produced by a model (AI-011).
    Derived,
    /// Began as a derived note and was promoted by the user; from now on it is
    /// protected exactly like a source document (AI-014).
    Promoted,
}

impl Authorship {
    /// True when the application must not rewrite the body automatically.
    ///
    /// Derived notes are the only bodies regeneration may replace, and even then
    /// only when the user has not edited them (AI-013, integrity rule 6).
    pub const fn is_source_protected(self) -> bool {
        matches!(self, Self::Source | Self::Promoted)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Derived => "derived",
            Self::Promoted => "promoted",
        }
    }

    pub const ALL: [Authorship; 3] = [Self::Source, Self::Derived, Self::Promoted];
}

impl std::fmt::Display for Authorship {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Authorship {
    type Err = UnknownAuthorship;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "source" => Ok(Self::Source),
            "derived" => Ok(Self::Derived),
            "promoted" => Ok(Self::Promoted),
            other => Err(UnknownAuthorship(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown authorship {0:?}")]
pub struct UnknownAuthorship(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_machine_owned_provenance_is_machine_writable() {
        // TAG-006, AI-009, integrity rule 4.
        for p in Provenance::ALL {
            assert_eq!(
                p.is_machine_writable(),
                matches!(p, Provenance::Derived | Provenance::AiGenerated),
                "{p} classified wrongly",
            );
            assert_ne!(p.is_machine_writable(), p.is_user_authored());
        }
    }

    #[test]
    fn imported_metadata_is_protected_like_manual() {
        // IM-004: tags that arrived with an import belong to the user.
        assert!(!Provenance::Imported.is_machine_writable());
        assert!(Provenance::Imported.is_user_authored());
    }

    #[test]
    fn approval_and_locking_pin_an_assertion() {
        assert!(ReviewState::Approved.pins_assertion());
        assert!(ReviewState::Locked.pins_assertion());
        assert!(!ReviewState::Unreviewed.pins_assertion());
        assert!(!ReviewState::Rejected.pins_assertion());
    }

    #[test]
    fn source_and_promoted_bodies_are_protected() {
        // ARCH-012 and AI-014.
        assert!(Authorship::Source.is_source_protected());
        assert!(Authorship::Promoted.is_source_protected());
        assert!(!Authorship::Derived.is_source_protected());
    }

    #[test]
    fn confidence_clamps_and_rejects_nan() {
        assert_eq!(Confidence::new(1.5).get(), 1.0);
        assert_eq!(Confidence::new(-3.0).get(), 0.0);
        assert_eq!(Confidence::new(f32::NAN).get(), 0.0);
        // Total ordering must hold so result ranking is deterministic.
        let mut scores = [
            Confidence::new(0.9),
            Confidence::new(0.1),
            Confidence::new(f32::NAN),
        ];
        scores.sort();
        assert_eq!(scores.first().unwrap().get(), 0.0);
    }

    #[test]
    fn enum_tokens_round_trip_through_storage_strings() {
        for p in Provenance::ALL {
            assert_eq!(p, p.as_str().parse().unwrap());
        }
        for r in ReviewState::ALL {
            assert_eq!(r, r.as_str().parse().unwrap());
        }
        for a in Authorship::ALL {
            assert_eq!(a, a.as_str().parse().unwrap());
        }
    }
}
