//! The SQLite schema, as an ordered list of migrations.
//!
//! Requirements: ARCH-006 (SQLite for metadata, relations, indexes and sync
//! state), KB-006/KB-007, GRAPH-009, SYNC-010, SYNC-012, SEC-006, NFR-004,
//! NFR-007, integrity rules 1–8.
//!
//! ## Schema decisions worth knowing
//!
//! * **`document.seq` is an explicit `INTEGER PRIMARY KEY`** alongside the
//!   stable `id BLOB`. FTS5 keys on an integer rowid, and relying on SQLite's
//!   implicit rowid would couple the search index to a value the engine is free
//!   to renumber.
//! * **Relationship endpoints carry no foreign key.** A cross-vault edge must
//!   survive its far vault being detached or removed (SYNC-012, AC-009), which
//!   a `REFERENCES document(id)` would make impossible. Dangling endpoints are
//!   therefore normal and are resolved at read time.
//! * **Rejections are their own tables, keyed by what was rejected** rather
//!   than by the row that was deleted. A later inference pass proposing the
//!   same tag or edge with a fresh ID is still recognised and suppressed
//!   (TAG-012, GRAPH-011, AC-006, integrity rule 7).
//! * **Derived data is segregated.** `document_fts`, `chunk` and
//!   `chunk_embedding` are rebuildable from the source files and the durable
//!   tables, which is what makes NFR-007 and integrity rule 3 achievable; they
//!   are also the only tables a rebuild is allowed to drop.
//! * **Every mutation appends to `sync_operation`,** whether or not a backend
//!   is configured. The spec requires sync-ready change tracking from phase one;
//!   with no backend the journal doubles as the undo and audit trail (SEC-006).

/// One schema step. Applied in order, exactly once, inside a transaction.
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

/// The schema's current version.
pub const CURRENT_VERSION: u32 = 1;

/// Tables holding data that can be recomputed from source files and durable
/// metadata. `rebuild_indexes` truncates exactly these (NFR-007).
pub const DERIVED_TABLES: &[&str] = &["document_fts", "chunk_embedding", "chunk"];

/// All migrations, oldest first.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial",
    sql: INITIAL,
}];

const INITIAL: &str = r#"
-- ---------------------------------------------------------------- vaults
CREATE TABLE vault (
    id                       BLOB    PRIMARY KEY,
    name                     TEXT    NOT NULL,
    -- Device-local absolute path. Never replicated: the same vault lives at a
    -- different place on each machine (SYNC-002).
    root                     TEXT    NOT NULL,
    sync_backend             TEXT    NOT NULL,
    created_at               TEXT    NOT NULL,
    available                INTEGER NOT NULL DEFAULT 1,
    write_ids_to_frontmatter INTEGER NOT NULL DEFAULT 0,
    -- VaultPolicy, denormalised so the policy engine needs one read (AI-005).
    default_sensitivity      TEXT    NOT NULL DEFAULT 'personal',
    disabled_tasks           TEXT    NOT NULL DEFAULT '[]',
    allowed_providers        TEXT,
    denied_providers         TEXT    NOT NULL DEFAULT '[]',
    auto_classify            INTEGER NOT NULL DEFAULT 0,
    revision                 INTEGER NOT NULL DEFAULT 1
) STRICT;

CREATE UNIQUE INDEX vault_root ON vault(root);

-- ------------------------------------------------------------- documents
CREATE TABLE document (
    -- Integer rowid for FTS5 to key on. Local to this device.
    seq           INTEGER PRIMARY KEY,
    -- The stable, replicated identity (KB-006, ARCH-011).
    id            BLOB    NOT NULL UNIQUE,
    vault_id      BLOB    NOT NULL REFERENCES vault(id) ON DELETE CASCADE,
    path          TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    kind          TEXT    NOT NULL,
    authorship    TEXT    NOT NULL,
    content_hash  BLOB    NOT NULL,
    size_bytes    INTEGER NOT NULL,
    created_at    TEXT    NOT NULL,
    modified_at   TEXT    NOT NULL,
    indexed_at    TEXT    NOT NULL,
    archived_at   TEXT,
    sensitivity   TEXT,
    frontmatter   TEXT    NOT NULL DEFAULT '{}',
    external      INTEGER NOT NULL DEFAULT 0,
    revision      INTEGER NOT NULL DEFAULT 1,
    UNIQUE (vault_id, path)
) STRICT;

-- Duplicate detection on import (IM-003) and re-identifying a moved file.
CREATE INDEX document_hash ON document(content_hash);
CREATE INDEX document_modified ON document(vault_id, modified_at);
CREATE INDEX document_kind ON document(vault_id, kind);
CREATE INDEX document_authorship ON document(authorship);

CREATE TABLE document_alias (
    document_id BLOB NOT NULL REFERENCES document(id) ON DELETE CASCADE,
    vault_id    BLOB NOT NULL,
    alias       TEXT NOT NULL,
    -- Case- and punctuation-folded form used for link resolution (MD-006).
    alias_key   TEXT NOT NULL,
    PRIMARY KEY (document_id, alias_key)
) STRICT;

CREATE INDEX document_alias_lookup ON document_alias(vault_id, alias_key);

-- Folded title for resolving `[[Wiki Links]]` by name (MD-006).
CREATE TABLE document_name (
    document_id BLOB NOT NULL REFERENCES document(id) ON DELETE CASCADE,
    vault_id    BLOB NOT NULL,
    name_key    TEXT NOT NULL,
    PRIMARY KEY (document_id, name_key)
) STRICT;

CREATE INDEX document_name_lookup ON document_name(vault_id, name_key);

-- ------------------------------------------------------------------ tags
CREATE TABLE tag (
    id             BLOB PRIMARY KEY,
    -- The nil UUID means "global to every vault"; see convert::vault_or_nil
    -- for why this is a sentinel rather than NULL.
    vault_id       BLOB NOT NULL,
    canonical_name TEXT NOT NULL,
    display_name   TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    revision       INTEGER NOT NULL DEFAULT 1,
    UNIQUE (vault_id, canonical_name)
) STRICT;

CREATE TABLE tag_alias (
    tag_id BLOB NOT NULL REFERENCES tag(id) ON DELETE CASCADE,
    alias  TEXT NOT NULL,
    PRIMARY KEY (tag_id, alias)
) STRICT;

-- One assignment per (document, tag). Provenance records the strongest claim,
-- and a user-authored claim is never replaced by a machine one (TAG-006).
CREATE TABLE document_tag (
    document_id BLOB NOT NULL REFERENCES document(id) ON DELETE CASCADE,
    tag_id      BLOB NOT NULL REFERENCES tag(id) ON DELETE CASCADE,
    provenance  TEXT NOT NULL,
    review      TEXT NOT NULL,
    source      TEXT NOT NULL,
    confidence  REAL,
    model       TEXT,
    assigned_at TEXT NOT NULL,
    PRIMARY KEY (document_id, tag_id)
) STRICT;

CREATE INDEX document_tag_by_tag ON document_tag(tag_id);
CREATE INDEX document_tag_by_provenance ON document_tag(provenance, review);

-- Survives the assignment's deletion, reindexing and sync (integrity rule 7).
CREATE TABLE rejected_tag (
    document_id BLOB NOT NULL REFERENCES document(id) ON DELETE CASCADE,
    tag_id      BLOB NOT NULL REFERENCES tag(id) ON DELETE CASCADE,
    rejected_at TEXT NOT NULL,
    reason      TEXT,
    PRIMARY KEY (document_id, tag_id)
) STRICT;

-- --------------------------------------------------------- relationships
CREATE TABLE relationship (
    id              BLOB PRIMARY KEY,
    -- No REFERENCES: a cross-vault edge outlives a detached vault (SYNC-012).
    source_vault    BLOB NOT NULL,
    source_document BLOB NOT NULL,
    target_vault    BLOB NOT NULL,
    target_document BLOB NOT NULL,
    kind            TEXT NOT NULL,
    provenance      TEXT NOT NULL,
    review          TEXT NOT NULL,
    confidence      REAL,
    model           TEXT,
    rationale       TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    revision        INTEGER NOT NULL DEFAULT 1,
    UNIQUE (source_document, target_document, kind)
) STRICT;

CREATE INDEX relationship_source ON relationship(source_document);
CREATE INDEX relationship_target ON relationship(target_document);
CREATE INDEX relationship_kind ON relationship(kind, review);

CREATE TABLE rejected_relationship (
    source_vault    BLOB NOT NULL,
    source_document BLOB NOT NULL,
    target_vault    BLOB NOT NULL,
    target_document BLOB NOT NULL,
    kind            TEXT NOT NULL,
    rejected_at     TEXT NOT NULL,
    reason          TEXT,
    PRIMARY KEY (source_document, target_document, kind)
) STRICT;

-- An unresolved `[[link]]` whose target does not exist yet. Kept so that
-- creating the target later completes the edge without a full reindex, and so
-- the UI can list broken links (MD-007, KB-013).
CREATE TABLE pending_link (
    document_id BLOB NOT NULL REFERENCES document(id) ON DELETE CASCADE,
    vault_id    BLOB NOT NULL,
    target_key  TEXT NOT NULL,
    raw_target  TEXT NOT NULL,
    kind        TEXT NOT NULL,
    PRIMARY KEY (document_id, target_key, kind)
) STRICT;

CREATE INDEX pending_link_target ON pending_link(vault_id, target_key);

-- ------------------------------------------------------------- revisions
CREATE TABLE document_revision (
    id           BLOB PRIMARY KEY,
    document_id  BLOB NOT NULL REFERENCES document(id) ON DELETE CASCADE,
    sequence     INTEGER NOT NULL,
    cause        TEXT NOT NULL,
    content_hash BLOB NOT NULL,
    size_bytes   INTEGER NOT NULL,
    path         TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    note         TEXT,
    UNIQUE (document_id, sequence)
) STRICT;

-- Body snapshots, kept separately so history can be pruned without losing the
-- revision metadata that proves what happened (KB-011).
CREATE TABLE revision_body (
    revision_id BLOB PRIMARY KEY REFERENCES document_revision(id) ON DELETE CASCADE,
    body        TEXT NOT NULL
) STRICT;

-- --------------------------------------------------- generated knowledge
CREATE TABLE generated_note_provenance (
    document_id  BLOB PRIMARY KEY REFERENCES document(id) ON DELETE CASCADE,
    revision_id  BLOB NOT NULL,
    inference_id BLOB NOT NULL,
    task         TEXT NOT NULL,
    provider     TEXT NOT NULL,
    model        TEXT NOT NULL,
    prompt_id    TEXT,
    sources      TEXT NOT NULL DEFAULT '[]',
    generated_at TEXT NOT NULL,
    -- Once true, regeneration may not overwrite the body (integrity rule 6).
    user_edited  INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Written for every request the policy engine considers, including refusals:
-- a denial is the evidence a user needs that a vault stayed local (AC-016).
CREATE TABLE inference_record (
    id                BLOB PRIMARY KEY,
    task              TEXT NOT NULL,
    provider          TEXT,
    model             TEXT,
    locality          TEXT,
    sensitivity       TEXT NOT NULL,
    documents         TEXT NOT NULL DEFAULT '[]',
    outcome           TEXT NOT NULL,
    reason            TEXT NOT NULL,
    prompt_tokens     INTEGER,
    completion_tokens INTEGER,
    cost              REAL,
    currency          TEXT,
    started_at        TEXT NOT NULL,
    duration_ms       INTEGER
) STRICT;

CREATE INDEX inference_record_time ON inference_record(started_at);
CREATE INDEX inference_record_outcome ON inference_record(outcome, started_at);

-- --------------------------------------------------------------- policy
CREATE TABLE document_policy (
    document_id BLOB PRIMARY KEY REFERENCES document(id) ON DELETE CASCADE,
    sensitivity TEXT NOT NULL
) STRICT;

-- ----------------------------------------------------------------- sync
-- This device's identity and logical clock (SYNC-010).
CREATE TABLE device (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    id        BLOB NOT NULL,
    name      TEXT NOT NULL DEFAULT '',
    counter   INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE sync_operation (
    id          BLOB PRIMARY KEY,
    vault_id    BLOB NOT NULL,
    entity_kind TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    change      TEXT NOT NULL,
    revision    INTEGER NOT NULL,
    counter     INTEGER NOT NULL,
    device      BLOB NOT NULL,
    state       TEXT NOT NULL,
    backend     TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    last_error  TEXT
) STRICT;

CREATE INDEX sync_operation_outstanding ON sync_operation(state, counter);
CREATE INDEX sync_operation_entity ON sync_operation(entity_kind, entity_id, revision);
CREATE INDEX sync_operation_vault ON sync_operation(vault_id, counter);

-- -------------------------------------------------------------- plugins
CREATE TABLE plugin_installation (
    id           BLOB PRIMARY KEY,
    plugin_id    TEXT NOT NULL UNIQUE,
    manifest     TEXT NOT NULL,
    install_path TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 0,
    granted      TEXT NOT NULL DEFAULT '[]',
    diagnostics  TEXT NOT NULL DEFAULT '[]',
    installed_at TEXT NOT NULL
) STRICT;

-- ----------------------------------------------- derived: search index
-- Rebuildable (integrity rule 3). `body` is a cache of the file's text, held
-- here so that snippet() and highlight() can serve SRCH-003 without re-reading
-- every candidate file from disk.
CREATE VIRTUAL TABLE document_fts USING fts5(
    title,
    body,
    tags,
    path,
    aliases,
    tokenize = "unicode61 remove_diacritics 2",
    prefix = '2 3 4'
);

-- ----------------------------------------------- derived: chunks/vectors
CREATE TABLE chunk (
    id          BLOB PRIMARY KEY,
    document_id BLOB NOT NULL REFERENCES document(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    byte_start  INTEGER NOT NULL,
    byte_end    INTEGER NOT NULL,
    heading_path TEXT NOT NULL DEFAULT '[]',
    text        TEXT NOT NULL,
    UNIQUE (document_id, ordinal)
) STRICT;

CREATE TABLE chunk_embedding (
    chunk_id    BLOB NOT NULL REFERENCES chunk(id) ON DELETE CASCADE,
    document_id BLOB NOT NULL,
    -- Model name, version and width, so vectors from different embedding
    -- spaces are never compared (open question 5).
    space_key   TEXT NOT NULL,
    model       TEXT NOT NULL,
    dimensions  INTEGER NOT NULL,
    -- The document hash the vector was computed from; a mismatch means stale.
    source_hash BLOB NOT NULL,
    vector      BLOB NOT NULL,
    computed_at TEXT NOT NULL,
    PRIMARY KEY (chunk_id, space_key)
) STRICT;

CREATE INDEX chunk_embedding_space ON chunk_embedding(space_key, document_id);

-- -------------------------------------------------------- saved queries
CREATE TABLE saved_search (
    name       TEXT PRIMARY KEY,
    query      TEXT NOT NULL,
    created_at TEXT NOT NULL
) STRICT;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_versions_are_consecutive_from_one() {
        for (i, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                migration.version,
                i as u32 + 1,
                "migration {:?} is out of order",
                migration.name
            );
        }
    }

    #[test]
    fn the_current_version_matches_the_last_migration() {
        assert_eq!(
            CURRENT_VERSION,
            MIGRATIONS.last().expect("at least one migration").version
        );
    }

    #[test]
    fn derived_tables_are_dropped_children_first() {
        // chunk_embedding references chunk, so the order matters for a rebuild.
        let chunk = DERIVED_TABLES.iter().position(|t| *t == "chunk");
        let embeddings = DERIVED_TABLES.iter().position(|t| *t == "chunk_embedding");
        assert!(embeddings < chunk, "{DERIVED_TABLES:?}");
    }
}
