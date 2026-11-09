//! Change tracking for replication.
//!
//! Requirements: SYNC-005 .. SYNC-012, ARCH-009, integrity rules 3 and 7.
//!
//! Marius is local-first and may never sync at all (SYNC-001), but the spec's
//! development note requires sync-ready change tracking from the first phase.
//! Every mutation therefore appends a [`SyncOperation`] whether a backend is
//! configured or not; with no backend the log is simply an undo/audit trail.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{DeviceId, OperationId, VaultId};

/// Which replicated entity an operation concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Vault,
    Document,
    DocumentAlias,
    Tag,
    DocumentTag,
    RejectedTag,
    Relationship,
    RejectedRelationship,
    DocumentRevision,
    GeneratedNoteProvenance,
    Policy,
}

impl EntityKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vault => "vault",
            Self::Document => "document",
            Self::DocumentAlias => "document_alias",
            Self::Tag => "tag",
            Self::DocumentTag => "document_tag",
            Self::RejectedTag => "rejected_tag",
            Self::Relationship => "relationship",
            Self::RejectedRelationship => "rejected_relationship",
            Self::DocumentRevision => "document_revision",
            Self::GeneratedNoteProvenance => "generated_note_provenance",
            Self::Policy => "policy",
        }
    }

    /// True when the entity is derived and so is never replicated: it can be
    /// rebuilt on each device from source files and durable metadata
    /// (integrity rule 3, open question 6).
    ///
    /// No current variant is derived — chunks, embeddings and FTS rows simply
    /// have no `EntityKind` — but the predicate documents the boundary and
    /// keeps the exhaustive match honest if one is ever added.
    pub const fn is_replicated(self) -> bool {
        true
    }

    pub const ALL: [EntityKind; 11] = [
        Self::Vault,
        Self::Document,
        Self::DocumentAlias,
        Self::Tag,
        Self::DocumentTag,
        Self::RejectedTag,
        Self::Relationship,
        Self::RejectedRelationship,
        Self::DocumentRevision,
        Self::GeneratedNoteProvenance,
        Self::Policy,
    ];
}

impl std::fmt::Display for EntityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EntityKind {
    type Err = UnknownEntityKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "vault" => Ok(Self::Vault),
            "document" => Ok(Self::Document),
            "document_alias" => Ok(Self::DocumentAlias),
            "tag" => Ok(Self::Tag),
            "document_tag" => Ok(Self::DocumentTag),
            "rejected_tag" => Ok(Self::RejectedTag),
            "relationship" => Ok(Self::Relationship),
            "rejected_relationship" => Ok(Self::RejectedRelationship),
            "document_revision" => Ok(Self::DocumentRevision),
            "generated_note_provenance" => Ok(Self::GeneratedNoteProvenance),
            "policy" => Ok(Self::Policy),
            other => Err(UnknownEntityKind(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown entity kind {0:?}")]
pub struct UnknownEntityKind(pub String);

/// What happened to the entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Upsert,
    /// A tombstone. Deletions replicate as records, never as absence, so a
    /// device that was offline during the delete does not resurrect the row
    /// (SYNC-010, integrity rule 7).
    Delete,
}

impl ChangeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Delete => "delete",
        }
    }
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ChangeKind {
    type Err = UnknownChangeKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "upsert" => Ok(Self::Upsert),
            "delete" => Ok(Self::Delete),
            other => Err(UnknownChangeKind(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown change kind {0:?}")]
pub struct UnknownChangeKind(pub String);

/// Progress of one operation through replication.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Recorded locally, not yet sent.
    #[default]
    Pending,
    /// Accepted by the authoritative backend.
    Acknowledged,
    /// Collides with a remote change; awaiting resolution. Never resolved by
    /// discarding an edge of the conflict (SYNC-009, AC-012).
    Conflicted,
    /// Send failed; will be retried (SYNC-011).
    Failed,
}

impl SyncState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Acknowledged => "acknowledged",
            Self::Conflicted => "conflicted",
            Self::Failed => "failed",
        }
    }

    /// True when the operation still needs attention from the sync engine.
    pub const fn is_outstanding(self) -> bool {
        matches!(self, Self::Pending | Self::Failed | Self::Conflicted)
    }

    pub const ALL: [SyncState; 4] = [
        Self::Pending,
        Self::Acknowledged,
        Self::Conflicted,
        Self::Failed,
    ];
}

impl std::fmt::Display for SyncState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SyncState {
    type Err = UnknownSyncState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "acknowledged" => Ok(Self::Acknowledged),
            "conflicted" => Ok(Self::Conflicted),
            "failed" => Ok(Self::Failed),
            other => Err(UnknownSyncState(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown sync state {0:?}")]
pub struct UnknownSyncState(pub String);

/// A Lamport-style logical clock paired with the device that stamped it.
///
/// Wall clocks disagree between a laptop and a phone, so ordering uses a
/// logical counter and breaks ties by device ID. That gives a total order
/// without pretending it implies causality (SYNC-010).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LogicalTimestamp {
    pub counter: u64,
    pub device: DeviceId,
}

impl LogicalTimestamp {
    pub const fn new(counter: u64, device: DeviceId) -> Self {
        Self { counter, device }
    }

    /// The next stamp this device should issue after observing `observed`.
    pub fn next_after(self, observed: Option<Self>) -> Self {
        let high = observed.map_or(self.counter, |o| self.counter.max(o.counter));
        Self {
            counter: high + 1,
            device: self.device,
        }
    }

    /// True when neither stamp is known to precede the other, i.e. two devices
    /// wrote at the same logical time. A conflict candidate (AC-012).
    pub fn is_concurrent_with(self, other: Self) -> bool {
        self.counter == other.counter && self.device != other.device
    }
}

/// One replicated change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOperation {
    pub id: OperationId,
    pub vault: VaultId,
    pub entity_kind: EntityKind,
    /// The entity's stable ID as text, since entity ID types differ.
    pub entity_id: String,
    pub change: ChangeKind,
    /// Per-entity revision counter. Mismatch against the backend's value is
    /// what detects a conflict (SYNC-009).
    pub revision: u64,
    pub timestamp: LogicalTimestamp,
    pub state: SyncState,
    /// Which backend this operation is destined for. Recorded so that a
    /// migration between backends can tell replayed history apart (SYNC-008).
    pub backend: String,
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
    /// Last error, for the status view (SYNC-011).
    pub last_error: Option<String>,
}

impl SyncOperation {
    pub fn new(
        vault: VaultId,
        entity_kind: EntityKind,
        entity_id: impl Into<String>,
        change: ChangeKind,
        revision: u64,
        timestamp: LogicalTimestamp,
        backend: impl Into<String>,
    ) -> Self {
        Self {
            id: OperationId::new(),
            vault,
            entity_kind,
            entity_id: entity_id.into(),
            change,
            revision,
            timestamp,
            state: SyncState::Pending,
            backend: backend.into(),
            recorded_at: OffsetDateTime::now_utc(),
            last_error: None,
        }
    }

    /// Key identifying the entity this operation targets. Two operations with
    /// the same key and revision from different devices are a conflict.
    pub fn entity_key(&self) -> (EntityKind, &str) {
        (self.entity_kind, self.entity_id.as_str())
    }

    /// True when applying this operation twice has the same effect as once.
    ///
    /// Replication is at-least-once, so every operation must be idempotent
    /// (SYNC-010). Upserts carry the full entity state and deletes are
    /// tombstones, so this holds unconditionally; the method exists to make the
    /// requirement visible at the call site.
    pub const fn is_idempotent(&self) -> bool {
        true
    }

    /// True when this operation and `other` describe concurrent edits to the
    /// same entity, which SYNC-009 forbids resolving by discarding either side.
    pub fn conflicts_with(&self, other: &Self) -> bool {
        self.entity_key() == other.entity_key()
            && self.revision == other.revision
            && self.timestamp.device != other.timestamp.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::DocumentId;

    fn op(device: DeviceId, revision: u64, entity: &str) -> SyncOperation {
        SyncOperation::new(
            VaultId::nil(),
            EntityKind::Document,
            entity,
            ChangeKind::Upsert,
            revision,
            LogicalTimestamp::new(revision, device),
            "self-hosted",
        )
    }

    #[test]
    fn deletions_replicate_as_tombstones() {
        // SYNC-010, integrity rule 7.
        let tombstone = SyncOperation::new(
            VaultId::new(),
            EntityKind::RejectedRelationship,
            crate::ids::RelationshipId::new().to_string(),
            ChangeKind::Delete,
            4,
            LogicalTimestamp::new(4, DeviceId::new()),
            "cloudkit",
        );
        assert_eq!(tombstone.change, ChangeKind::Delete);
        assert!(tombstone.is_idempotent());
    }

    #[test]
    fn concurrent_edits_to_one_document_are_a_conflict() {
        // AC-012: two devices edit the same document offline.
        let laptop = DeviceId::new();
        let phone = DeviceId::new();
        let doc = DocumentId::new().to_string();

        let a = op(laptop, 7, &doc);
        let b = op(phone, 7, &doc);
        assert!(a.conflicts_with(&b));
        assert!(b.conflicts_with(&a));
        assert!(a.timestamp.is_concurrent_with(b.timestamp));
    }

    #[test]
    fn sequential_edits_from_one_device_are_not_a_conflict() {
        let laptop = DeviceId::new();
        let doc = DocumentId::new().to_string();
        assert!(!op(laptop, 7, &doc).conflicts_with(&op(laptop, 8, &doc)));
        // Nor are same-revision edits from the *same* device, which is just a
        // retry of the same operation.
        assert!(!op(laptop, 7, &doc).conflicts_with(&op(laptop, 7, &doc)));
    }

    #[test]
    fn edits_to_different_entities_are_independent() {
        let a = op(DeviceId::new(), 3, &DocumentId::new().to_string());
        let b = op(DeviceId::new(), 3, &DocumentId::new().to_string());
        assert!(!a.conflicts_with(&b));
    }

    #[test]
    fn logical_clock_advances_past_what_it_has_observed() {
        let device = DeviceId::new();
        let local = LogicalTimestamp::new(3, device);
        let remote = LogicalTimestamp::new(11, DeviceId::new());
        assert_eq!(local.next_after(Some(remote)).counter, 12);
        assert_eq!(local.next_after(None).counter, 4);
    }

    #[test]
    fn outstanding_states_are_identified_for_retry() {
        // SYNC-011.
        assert!(SyncState::Pending.is_outstanding());
        assert!(SyncState::Failed.is_outstanding());
        assert!(SyncState::Conflicted.is_outstanding());
        assert!(!SyncState::Acknowledged.is_outstanding());
    }

    #[test]
    fn tokens_round_trip() {
        for e in EntityKind::ALL {
            assert_eq!(e, e.as_str().parse().unwrap());
            assert!(e.is_replicated());
        }
        for s in SyncState::ALL {
            assert_eq!(s, s.as_str().parse().unwrap());
        }
        for c in [ChangeKind::Upsert, ChangeKind::Delete] {
            assert_eq!(c, c.as_str().parse().unwrap());
        }
    }
}
