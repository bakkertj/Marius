//! Graph edges: explicit links, tag kinship, and persisted inferences.
//!
//! Requirements: GRAPH-002, GRAPH-003, GRAPH-007 .. GRAPH-013, AC-006, AC-009,
//! integrity rules 1, 2 and 7.
//!
//! An important modelling decision lives here. Shared-tag kinship (GRAPH-003)
//! is *computed* at query time — storing every pairwise edge for a popular tag
//! would be quadratic for no benefit — while inferred and user-asserted edges
//! (GRAPH-007, GRAPH-010) are *stored* individually because they cannot be
//! recomputed and must survive reindexing.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{GlobalDocumentId, RelationshipId};
use crate::provenance::{Confidence, Provenance, ReviewState};

/// What an edge asserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    /// A `[[wiki link]]` or Markdown link in the source text (GRAPH-002).
    /// Always directed from the linking document to its target.
    Link,
    /// A `![[embed]]` — a link that also includes the target's content (MD-006).
    Embed,
    /// Documents that share at least one tag (GRAPH-003). Computed, not stored.
    SharedTag,
    /// A model judged the documents related without an explicit link or shared
    /// tag (GRAPH-007). Stored, with confidence, and reversible by the user.
    Semantic,
    /// The user drew the edge themselves (GRAPH-010).
    Asserted,
    /// Provenance edge from a generated note to a source it was built from
    /// (AI-011). Never removed automatically, or citations would rot.
    DerivedFrom,
}

impl RelationshipKind {
    /// True when the edge is recomputed from content on every index pass and so
    /// is never stored as a row.
    pub const fn is_computed(self) -> bool {
        matches!(self, Self::SharedTag)
    }

    /// True when direction carries meaning. Undirected kinds are stored once,
    /// with endpoints in a canonical order, so A–B and B–A cannot both exist.
    pub const fn is_directed(self) -> bool {
        matches!(self, Self::Link | Self::Embed | Self::DerivedFrom)
    }

    /// The provenance an edge of this kind is created with.
    pub const fn default_provenance(self) -> Provenance {
        match self {
            Self::Link | Self::Embed | Self::SharedTag => Provenance::Derived,
            Self::Semantic => Provenance::AiGenerated,
            Self::Asserted | Self::DerivedFrom => Provenance::Manual,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Embed => "embed",
            Self::SharedTag => "shared_tag",
            Self::Semantic => "semantic",
            Self::Asserted => "asserted",
            Self::DerivedFrom => "derived_from",
        }
    }

    pub const ALL: [RelationshipKind; 6] = [
        Self::Link,
        Self::Embed,
        Self::SharedTag,
        Self::Semantic,
        Self::Asserted,
        Self::DerivedFrom,
    ];
}

impl std::fmt::Display for RelationshipKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RelationshipKind {
    type Err = UnknownRelationshipKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "link" => Ok(Self::Link),
            "embed" => Ok(Self::Embed),
            "shared_tag" => Ok(Self::SharedTag),
            "semantic" => Ok(Self::Semantic),
            "asserted" => Ok(Self::Asserted),
            "derived_from" => Ok(Self::DerivedFrom),
            other => Err(UnknownRelationshipKind(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown relationship kind {0:?}")]
pub struct UnknownRelationshipKind(pub String);

/// The two ends of an edge.
///
/// Endpoints are [`GlobalDocumentId`]s so a cross-vault edge keeps both halves
/// identifiable while one vault is detached (GRAPH-008, SYNC-012, AC-009), and
/// so no edge ever depends on a path (integrity rule 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Endpoints {
    pub source: GlobalDocumentId,
    pub target: GlobalDocumentId,
}

impl Endpoints {
    pub const fn new(source: GlobalDocumentId, target: GlobalDocumentId) -> Self {
        Self { source, target }
    }

    /// Endpoints in a canonical order, so an undirected edge has exactly one
    /// representation and the database's uniqueness constraint can catch
    /// duplicates (integrity rule 7).
    pub fn canonical(self) -> Self {
        if self.target < self.source {
            Self {
                source: self.target,
                target: self.source,
            }
        } else {
            self
        }
    }

    /// Normalised for storage: directed kinds keep their direction, undirected
    /// kinds are canonicalised.
    pub fn normalized_for(self, kind: RelationshipKind) -> Self {
        if kind.is_directed() {
            self
        } else {
            self.canonical()
        }
    }

    pub fn reversed(self) -> Self {
        Self {
            source: self.target,
            target: self.source,
        }
    }

    /// True when both ends are the same document.
    pub fn is_self_loop(self) -> bool {
        self.source == self.target
    }

    /// True when the edge crosses a vault boundary (GRAPH-015).
    pub fn is_cross_vault(self) -> bool {
        self.source.vault != self.target.vault
    }

    /// The end that is not `document`, if `document` is an endpoint.
    pub fn other(self, document: GlobalDocumentId) -> Option<GlobalDocumentId> {
        if self.source == document {
            Some(self.target)
        } else if self.target == document {
            Some(self.source)
        } else {
            None
        }
    }
}

/// A stored graph edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    pub id: RelationshipId,
    pub endpoints: Endpoints,
    pub kind: RelationshipKind,
    pub provenance: Provenance,
    pub review: ReviewState,
    pub confidence: Option<Confidence>,
    /// Model that proposed the edge, for provenance display (UI-010, AI-015).
    pub model: Option<String>,
    /// Short human-readable justification, shown on hover for inferred edges.
    pub rationale: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl Relationship {
    /// An edge of `kind` between `endpoints`, with the kind's default
    /// provenance and normalised endpoint order.
    pub fn new(endpoints: Endpoints, kind: RelationshipKind) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id: RelationshipId::new(),
            endpoints: endpoints.normalized_for(kind),
            kind,
            provenance: kind.default_provenance(),
            review: if kind.default_provenance() == Provenance::Manual {
                ReviewState::Approved
            } else {
                ReviewState::Unreviewed
            },
            confidence: None,
            model: None,
            rationale: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// A model-proposed semantic edge (GRAPH-007).
    pub fn semantic(
        endpoints: Endpoints,
        model: impl Into<String>,
        confidence: Confidence,
        rationale: Option<String>,
    ) -> Self {
        Self {
            confidence: Some(confidence),
            model: Some(model.into()),
            rationale,
            ..Self::new(endpoints, RelationshipKind::Semantic)
        }
    }

    /// True when re-inference may overwrite or delete this edge.
    ///
    /// GRAPH-012 requires reevaluation of inferred edges *without* discarding
    /// confirmed ones, which is exactly this predicate.
    pub fn is_machine_writable(&self) -> bool {
        self.provenance.is_machine_writable() && !self.review.pins_assertion()
    }

    /// Record user confirmation (GRAPH-010).
    pub fn confirm(&mut self) {
        self.review = ReviewState::Approved;
        if self.provenance == Provenance::AiGenerated {
            self.provenance = Provenance::AiConfirmed;
        }
        self.updated_at = OffsetDateTime::now_utc();
    }

    /// The suppression record to store when the user rejects this edge.
    pub fn rejection(&self, reason: Option<String>) -> RejectedRelationship {
        RejectedRelationship {
            endpoints: self.endpoints.normalized_for(self.kind),
            kind: self.kind,
            rejected_at: OffsetDateTime::now_utc(),
            reason,
        }
    }
}

/// A relationship the user removed and does not want back.
///
/// Kept as its own record, keyed by endpoints and kind rather than by the
/// deleted edge's ID, so that a later inference pass proposing the *same*
/// relationship with a *new* ID is still recognised and suppressed (GRAPH-011,
/// AC-006, integrity rule 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedRelationship {
    pub endpoints: Endpoints,
    pub kind: RelationshipKind,
    #[serde(with = "time::serde::rfc3339")]
    pub rejected_at: OffsetDateTime,
    pub reason: Option<String>,
}

impl RejectedRelationship {
    /// True when this suppression blocks `candidate`.
    ///
    /// Direction is ignored for undirected kinds, so rejecting A–B also blocks
    /// a later proposal of B–A.
    pub fn suppresses(&self, candidate: &Relationship) -> bool {
        self.kind == candidate.kind
            && self.endpoints == candidate.endpoints.normalized_for(candidate.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{DocumentId, VaultId};

    fn doc(vault: VaultId) -> GlobalDocumentId {
        GlobalDocumentId::new(vault, DocumentId::new())
    }

    fn pair() -> (GlobalDocumentId, GlobalDocumentId) {
        let v = VaultId::new();
        (doc(v), doc(v))
    }

    #[test]
    fn shared_tag_edges_are_computed_not_stored() {
        // GRAPH-003 plus the spec's graph-interpretation note.
        assert!(RelationshipKind::SharedTag.is_computed());
        for kind in RelationshipKind::ALL {
            if kind != RelationshipKind::SharedTag {
                assert!(!kind.is_computed(), "{kind} should be stored");
            }
        }
    }

    #[test]
    fn links_are_directed_and_semantic_edges_are_not() {
        assert!(RelationshipKind::Link.is_directed());
        assert!(RelationshipKind::Embed.is_directed());
        assert!(RelationshipKind::DerivedFrom.is_directed());
        assert!(!RelationshipKind::Semantic.is_directed());
        assert!(!RelationshipKind::Asserted.is_directed());
    }

    #[test]
    fn undirected_edges_get_one_canonical_representation() {
        let (a, b) = pair();
        let forward = Relationship::new(Endpoints::new(a, b), RelationshipKind::Semantic);
        let backward = Relationship::new(Endpoints::new(b, a), RelationshipKind::Semantic);
        assert_eq!(forward.endpoints, backward.endpoints);
    }

    #[test]
    fn directed_edges_keep_their_direction() {
        let (a, b) = pair();
        let forward = Relationship::new(Endpoints::new(a, b), RelationshipKind::Link);
        let backward = Relationship::new(Endpoints::new(b, a), RelationshipKind::Link);
        assert_ne!(forward.endpoints, backward.endpoints);
        assert_eq!(forward.endpoints.reversed(), backward.endpoints);
    }

    #[test]
    fn rejecting_an_edge_suppresses_the_same_edge_in_either_direction() {
        // AC-006 and integrity rule 7: the rejection must outlive the edge and
        // match a freshly minted proposal with a different ID.
        let (a, b) = pair();
        let edge =
            Relationship::semantic(Endpoints::new(a, b), "llama3.2", Confidence::new(0.7), None);
        let rejection = edge.rejection(Some("unrelated".into()));

        let reproposed = Relationship::semantic(
            Endpoints::new(b, a),
            "some-other-model",
            Confidence::new(0.9),
            None,
        );
        assert_ne!(edge.id, reproposed.id);
        assert!(rejection.suppresses(&reproposed));
    }

    #[test]
    fn rejection_does_not_leak_across_relationship_kinds() {
        let (a, b) = pair();
        let semantic = Relationship::new(Endpoints::new(a, b), RelationshipKind::Semantic);
        let rejection = semantic.rejection(None);
        let explicit = Relationship::new(Endpoints::new(a, b), RelationshipKind::Link);
        assert!(
            !rejection.suppresses(&explicit),
            "an explicit link the user wrote must stand"
        );
    }

    #[test]
    fn confirmed_edges_survive_reinference() {
        // GRAPH-012.
        let (a, b) = pair();
        let mut edge = Relationship::semantic(Endpoints::new(a, b), "m", Confidence::CERTAIN, None);
        assert!(edge.is_machine_writable());
        edge.confirm();
        assert_eq!(edge.provenance, Provenance::AiConfirmed);
        assert!(!edge.is_machine_writable());
    }

    #[test]
    fn user_asserted_and_citation_edges_are_never_machine_writable() {
        let (a, b) = pair();
        for kind in [RelationshipKind::Asserted, RelationshipKind::DerivedFrom] {
            let edge = Relationship::new(Endpoints::new(a, b), kind);
            assert!(!edge.is_machine_writable(), "{kind}");
            assert_eq!(edge.review, ReviewState::Approved);
        }
    }

    #[test]
    fn cross_vault_edges_are_recognised_and_traversable() {
        // AC-009.
        let left = doc(VaultId::new());
        let right = doc(VaultId::new());
        let endpoints = Endpoints::new(left, right);
        assert!(endpoints.is_cross_vault());
        assert_eq!(endpoints.other(left), Some(right));
        assert_eq!(endpoints.other(right), Some(left));
        assert_eq!(endpoints.other(doc(VaultId::new())), None);
        assert!(!endpoints.is_self_loop());
    }

    #[test]
    fn kind_tokens_round_trip() {
        for kind in RelationshipKind::ALL {
            assert_eq!(kind, kind.as_str().parse().unwrap());
        }
    }
}
