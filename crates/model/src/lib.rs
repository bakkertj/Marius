//! # Marius domain model
//!
//! The vocabulary every other crate shares: stable identities, documents,
//! tags, relationships, revisions, embeddings, policy, replication and
//! plugins. It has no I/O, no database and no async runtime, so the rules that
//! protect the user's content can be tested in isolation and reused unchanged
//! on desktop, on iOS and in the sync service (ARCH-001, UI-008).
//!
//! ## The four invariants this crate exists to enforce
//!
//! 1. **Identity is permanent.** A [`ids::DocumentId`] is independent of
//!    filename, folder and vault, so renaming or moving a document cannot break
//!    a link or a graph edge (KB-006, ARCH-011, integrity rules 1–2).
//! 2. **Source content belongs to the user.** [`provenance::Authorship`] marks
//!    which bodies the application may never rewrite on its own (ARCH-012,
//!    AI-009).
//! 3. **User intent outranks automation.** [`provenance::Provenance`] and
//!    [`provenance::ReviewState`] decide, in one place, what automatic
//!    classification is allowed to touch (TAG-006, GRAPH-012, integrity rule 4)
//!    — and rejections are stored as their own records so reindexing and sync
//!    cannot resurrect them (integrity rule 7).
//! 4. **Privacy restrictions only tighten.** [`policy::Sensitivity`] is ordered
//!    and combined by taking the maximum, so an unknown document is treated
//!    conservatively and one confidential source restricts a whole request
//!    (AI-007, AI-008).
//!
//! Named for Marius Michel Pasha, who spent his life putting lighthouses where
//! ships kept being lost.

#![forbid(unsafe_code)]
#![warn(clippy::all)]
// Every module, type, function and non-obvious field here carries a doc comment
// naming the requirement it serves. `missing_docs` is deliberately off: it would
// demand prose on `pub id: DocumentId`, and filler of that kind makes the
// comments that do carry meaning harder to find. Revisit before publishing the
// crate to a registry, where the rendered API surface stands alone.

pub mod document;
pub mod embedding;
pub mod ids;
pub mod inference;
pub mod plugin;
pub mod policy;
pub mod portability;
pub mod provenance;
pub mod relationship;
pub mod revision;
pub mod sync;
pub mod tag;

pub use document::{
    Document, DocumentAlias, DocumentKind, MetadataValue, PathError, SyncBackend, Vault, VaultPath,
};
pub use embedding::{cosine_similarity, Chunk, ChunkEmbedding, EmbeddingModel};
pub use ids::{
    ChunkId, ContentHash, DeviceId, DocumentId, GlobalDocumentId, IdParseError, InferenceId,
    OperationId, PluginInstallationId, RelationshipId, RevisionId, TagId, VaultId,
};
pub use inference::{
    Citation, Claim, ClaimSupport, GeneratedNoteProvenance, InferenceOutcome, InferenceRecord,
};
pub use plugin::{
    Capability, CompatibilityDiagnostic, DiagnosticSeverity, PluginInstallation, PluginManifest,
};
pub use policy::{DocumentPolicy, InferenceTask, Locality, Sensitivity, VaultPolicy};
pub use portability::{sanitize, travels_everywhere, windows_issues, Portability};
pub use provenance::{Authorship, Confidence, Provenance, ReviewState};
pub use relationship::{Endpoints, RejectedRelationship, Relationship, RelationshipKind};
pub use revision::{DocumentRevision, RevisionCause};
pub use sync::{
    ChangeKind, EntityKind, LogicalTimestamp, PullResponse, PushConflict, PushRequest,
    PushResponse, SyncEnvelope, SyncOperation, SyncPayload, SyncState,
};
pub use tag::{DocumentTag, RejectedTag, Tag, TagName, TagNameError, TagSource};

/// The application's name, used in file headers, user agents and frontmatter keys.
pub const APP_NAME: &str = "Marius";

/// Frontmatter key that holds a document's stable ID when a vault opts into
/// writing identity into its files (see [`Vault::write_ids_to_frontmatter`]).
pub const FRONTMATTER_ID_KEY: &str = "marius-id";
