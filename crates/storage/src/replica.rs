//! Writers for *replicated* state (SYNC-005, SYNC-010).
//!
//! Applying what another device already journaled must not journal again —
//! that would echo every operation straight back — and must store the
//! entity's *remote* revision verbatim, or the per-entity counters diverge
//! and conflict detection starts lying. So replication gets its own
//! writers, the same pattern as the indexer/classifier provenance split:
//! each writer owns exactly the rows it may touch.
//!
//! What these writers do *not* bypass: the protection gates. A durable
//! rejection suppresses a synced proposal exactly as it suppresses a
//! reindexed one, via [`crate::tags::assign_replica`].

use marius_model::{
    Document, DocumentId, Relationship, RelationshipId, Tag, TagId, TagName, VaultId,
};
use rusqlite::{params, OptionalExtension};

use crate::convert::{bool_to_sql, hash_to_sql, json_to_sql, time_to_sql, IdSql};
use crate::{Connection, Result};

// -------------------------------------------------------------- cursor

/// Where this vault's pull left off against `backend`. Zero for a backend
/// the vault has never pulled from — including after a migration to a
/// different backend (SYNC-008), which is the point of keying on both.
pub fn cursor(conn: &Connection, vault: VaultId, backend: &str) -> Result<u64> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT backend, last_seq FROM sync_cursor WHERE vault_id = ?1",
            params![vault.to_sql_blob()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((stored, seq)) if stored == backend => seq as u64,
        _ => 0,
    })
}

pub fn set_cursor(conn: &Connection, vault: VaultId, backend: &str, seq: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_cursor (vault_id, backend, last_seq) VALUES (?1, ?2, ?3)
         ON CONFLICT(vault_id) DO UPDATE SET backend = excluded.backend,
                                             last_seq = excluded.last_seq",
        params![vault.to_sql_blob(), backend, seq as i64],
    )?;
    Ok(())
}

// ------------------------------------------------------------ document

/// Store a replicated document row exactly as received — remote revision
/// included — with no journal entry. The caller owns writing the file and
/// refreshing the derived rows.
pub fn upsert_document(conn: &Connection, document: &Document, revision: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO document
            (id, vault_id, path, title, kind, authorship, content_hash, size_bytes,
             created_at, modified_at, indexed_at, archived_at, sensitivity, frontmatter,
             external, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(id) DO UPDATE SET
            vault_id = excluded.vault_id,
            path = excluded.path,
            title = excluded.title,
            kind = excluded.kind,
            authorship = excluded.authorship,
            content_hash = excluded.content_hash,
            size_bytes = excluded.size_bytes,
            modified_at = excluded.modified_at,
            indexed_at = excluded.indexed_at,
            archived_at = excluded.archived_at,
            sensitivity = excluded.sensitivity,
            frontmatter = excluded.frontmatter,
            external = excluded.external,
            revision = excluded.revision",
        params![
            document.id.to_sql_blob(),
            document.vault.to_sql_blob(),
            document.path.as_str(),
            document.title,
            document.kind.as_str(),
            document.authorship.as_str(),
            hash_to_sql(&document.content_hash),
            document.size_bytes as i64,
            time_to_sql(document.created_at),
            time_to_sql(document.modified_at),
            time_to_sql(document.indexed_at),
            document.archived_at.map(time_to_sql),
            document.sensitivity.map(|s| s.as_str()),
            json_to_sql(&document.frontmatter)?,
            bool_to_sql(document.external),
            revision as i64,
        ],
    )?;
    crate::documents::refresh_names(conn, document)?;
    Ok(())
}

/// Remove a replicated-away document's row, FTS entry included. Silent:
/// the tombstone we are applying is already in the other device's journal.
pub fn delete_document(conn: &Connection, id: DocumentId) -> Result<bool> {
    if let Some(seq) = crate::documents::seq_of(conn, id)? {
        crate::search_index::remove(conn, seq)?;
    }
    let removed = conn.execute(
        "DELETE FROM document WHERE id = ?1",
        params![id.to_sql_blob()],
    )?;
    Ok(removed > 0)
}

/// The document's current revision counter, for materialising envelopes.
pub fn document_revision(conn: &Connection, id: DocumentId) -> Result<Option<u64>> {
    let revision: Option<i64> = conn
        .query_row(
            "SELECT revision FROM document WHERE id = ?1",
            params![id.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(revision.map(|r| r as u64))
}

// ----------------------------------------------------------------- tag

pub fn upsert_tag(conn: &Connection, tag: &Tag, revision: u64) -> Result<()> {
    let pretender = find_pretender(conn, tag.vault, &tag.canonical_name, tag.id)?;
    if let Some(pretender) = pretender {
        // Free the name's uniqueness slot; references migrate after the
        // authoritative row exists, or their foreign keys have no target.
        conn.execute(
            "UPDATE tag SET canonical_name = canonical_name || ':replaced' WHERE id = ?1",
            params![pretender.to_sql_blob()],
        )?;
    }
    conn.execute(
        "INSERT INTO tag (id, vault_id, canonical_name, display_name, created_at, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
            canonical_name = excluded.canonical_name,
            display_name = excluded.display_name,
            revision = excluded.revision",
        params![
            tag.id.to_sql_blob(),
            tag.vault
                .map(|v| v.to_sql_blob())
                .unwrap_or_else(|| VaultId::nil().to_sql_blob()),
            tag.canonical_name.as_str(),
            tag.display_name,
            time_to_sql(tag.created_at),
            revision as i64,
        ],
    )?;
    conn.execute(
        "DELETE FROM tag_alias WHERE tag_id = ?1",
        params![tag.id.to_sql_blob()],
    )?;
    for alias in &tag.aliases {
        conn.execute(
            "INSERT OR IGNORE INTO tag_alias (tag_id, alias) VALUES (?1, ?2)",
            params![tag.id.to_sql_blob(), alias.as_str()],
        )?;
    }
    if let Some(pretender) = pretender {
        migrate_tag_references(conn, pretender, tag.id)?;
    }
    Ok(())
}

/// A locally minted tag occupying the replicated tag's name.
///
/// Applying a replicated document re-derives its inline tags *before* the
/// tag's own operation arrives, which mints a local id for the same
/// canonical name. The replicated id is the shared one (KB-006 for tags);
/// the pretender's references are rewritten onto it and the pretender
/// removed — otherwise the authoritative insert trips the name-uniqueness
/// constraint and devices never converge on one identity.
fn find_pretender(
    conn: &Connection,
    vault: Option<VaultId>,
    name: &TagName,
    replicated: TagId,
) -> Result<Option<TagId>> {
    let local: Option<Vec<u8>> = conn
        .query_row(
            "SELECT id FROM tag WHERE vault_id = ?1 AND canonical_name = ?2",
            params![
                vault
                    .map(|v| v.to_sql_blob())
                    .unwrap_or_else(|| VaultId::nil().to_sql_blob()),
                name.as_str()
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(local) = local else {
        return Ok(None);
    };
    let local = TagId::from_sql_blob(&local)?;
    Ok((local != replicated).then_some(local))
}

/// Move every reference from the pretender onto the replicated id, then
/// drop the pretender. `UPDATE OR IGNORE` first, because the replicated
/// id may already carry the same (document, tag) pair.
fn migrate_tag_references(conn: &Connection, from: TagId, to: TagId) -> Result<()> {
    conn.execute(
        "UPDATE OR IGNORE document_tag SET tag_id = ?2 WHERE tag_id = ?1",
        params![from.to_sql_blob(), to.to_sql_blob()],
    )?;
    conn.execute(
        "DELETE FROM document_tag WHERE tag_id = ?1",
        params![from.to_sql_blob()],
    )?;
    conn.execute(
        "UPDATE OR IGNORE rejected_tag SET tag_id = ?2 WHERE tag_id = ?1",
        params![from.to_sql_blob(), to.to_sql_blob()],
    )?;
    conn.execute(
        "DELETE FROM rejected_tag WHERE tag_id = ?1",
        params![from.to_sql_blob()],
    )?;
    conn.execute("DELETE FROM tag WHERE id = ?1", params![from.to_sql_blob()])?;
    Ok(())
}

pub fn delete_tag(conn: &Connection, id: TagId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM tag WHERE id = ?1", params![id.to_sql_blob()])? > 0)
}

pub fn tag_revision(conn: &Connection, id: TagId) -> Result<Option<u64>> {
    let revision: Option<i64> = conn
        .query_row(
            "SELECT revision FROM tag WHERE id = ?1",
            params![id.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(revision.map(|r| r as u64))
}

/// Make sure a tag row with this exact id exists, for assignment and
/// rejection payloads that arrive before (or without) their Tag operation.
/// A later Tag op overwrites the placeholder with authoritative state.
pub fn ensure_tag_id(
    conn: &Connection,
    id: TagId,
    vault: Option<VaultId>,
    name: &TagName,
) -> Result<()> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM tag WHERE id = ?1",
            params![id.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Ok(());
    }
    let tag = Tag {
        id,
        vault,
        canonical_name: name.clone(),
        display_name: name.to_string(),
        aliases: Vec::new(),
        created_at: time::OffsetDateTime::now_utc(),
    };
    // upsert_tag resolves any name collision in the replicated id's favour.
    upsert_tag(conn, &tag, 1)
}

/// [`crate::tags::ensure`] without the journal entry, for replica index
/// passes: a tag derived from replicated content will be (or already was)
/// named authoritatively by its own Tag operation, so the local placeholder
/// must not announce itself to the backend.
pub fn ensure_tag(conn: &Connection, vault: Option<VaultId>, name: &TagName) -> Result<Tag> {
    if let Some(existing) = crate::tags::by_name(conn, vault, name)? {
        return Ok(existing);
    }
    let tag = Tag::new(vault, name.clone());
    upsert_tag(conn, &tag, 1)?;
    Ok(tag)
}

/// Silent unassignment, for replicated removals.
pub fn unassign_tag(conn: &Connection, document: DocumentId, tag: TagId) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM document_tag WHERE document_id = ?1 AND tag_id = ?2",
        params![document.to_sql_blob(), tag.to_sql_blob()],
    )? > 0)
}

/// A replicated rejection: durable here exactly as it was on the device
/// that made it (integrity rule 7), and it removes any live machine
/// assignment the same way a local rejection would.
pub fn reject_tag(
    conn: &Connection,
    document: DocumentId,
    tag: TagId,
    reason: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO rejected_tag (document_id, tag_id, rejected_at, reason)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            document.to_sql_blob(),
            tag.to_sql_blob(),
            time_to_sql(time::OffsetDateTime::now_utc()),
            reason,
        ],
    )?;
    conn.execute(
        "DELETE FROM document_tag
         WHERE document_id = ?1 AND tag_id = ?2 AND provenance IN ('ai_generated', 'derived')",
        params![document.to_sql_blob(), tag.to_sql_blob()],
    )?;
    Ok(())
}

// -------------------------------------------------------- relationship

pub fn upsert_relationship(
    conn: &Connection,
    relationship: &Relationship,
    revision: u64,
) -> Result<()> {
    // A replicated edge the user rejected here stays dead (integrity 7).
    let rejected: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM rejected_relationship
             WHERE source_document = ?1 AND target_document = ?2 AND kind = ?3",
            params![
                relationship.endpoints.source.document.to_sql_blob(),
                relationship.endpoints.target.document.to_sql_blob(),
                relationship.kind.as_str(),
            ],
            |row| row.get(0),
        )
        .optional()?;
    if rejected.is_some() && !relationship.provenance.is_user_authored() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO relationship
            (id, source_vault, source_document, target_vault, target_document, kind,
             provenance, review, confidence, model, rationale, created_at, updated_at,
             revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(source_document, target_document, kind) DO UPDATE SET
            provenance = excluded.provenance,
            review = excluded.review,
            confidence = excluded.confidence,
            model = excluded.model,
            rationale = excluded.rationale,
            updated_at = excluded.updated_at,
            revision = excluded.revision",
        params![
            relationship.id.to_sql_blob(),
            relationship.endpoints.source.vault.to_sql_blob(),
            relationship.endpoints.source.document.to_sql_blob(),
            relationship.endpoints.target.vault.to_sql_blob(),
            relationship.endpoints.target.document.to_sql_blob(),
            relationship.kind.as_str(),
            relationship.provenance.as_str(),
            relationship.review.as_str(),
            relationship.confidence.map(|c| c.get() as f64),
            relationship.model,
            relationship.rationale,
            time_to_sql(relationship.created_at),
            time_to_sql(relationship.updated_at),
            revision as i64,
        ],
    )?;
    Ok(())
}

pub fn delete_relationship(conn: &Connection, id: RelationshipId) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM relationship WHERE id = ?1",
        params![id.to_sql_blob()],
    )? > 0)
}

pub fn relationship_revision(conn: &Connection, id: RelationshipId) -> Result<Option<u64>> {
    let revision: Option<i64> = conn
        .query_row(
            "SELECT revision FROM relationship WHERE id = ?1",
            params![id.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(revision.map(|r| r as u64))
}

/// Resolve a replicated id to its document, if this device has it.
pub fn document_exists(conn: &Connection, id: DocumentId) -> Result<bool> {
    let row: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM document WHERE id = ?1",
            params![id.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(row.is_some())
}

// --------------------------------------------- getters for materialising

/// Raw tag row: vault blob, canonical, display, created, revision.
type TagRow = (Option<Vec<u8>>, String, String, String, i64);
/// Raw assignment row: provenance, review, source, confidence, model, at.
type AssignmentRow = (String, String, String, Option<f64>, Option<String>, String);

/// A tag with its aliases, by exact id.
pub fn get_tag(conn: &Connection, id: TagId) -> Result<Option<(Tag, u64)>> {
    let row: Option<TagRow> = conn
        .query_row(
            "SELECT vault_id, canonical_name, display_name, created_at, revision
             FROM tag WHERE id = ?1",
            params![id.to_sql_blob()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((vault, canonical, display, created, revision)) = row else {
        return Ok(None);
    };
    let vault = vault
        .map(|v| VaultId::from_sql_blob(&v))
        .transpose()?
        .filter(|v| *v != VaultId::nil());
    let mut aliases = Vec::new();
    let mut stmt = conn.prepare("SELECT alias FROM tag_alias WHERE tag_id = ?1 ORDER BY alias")?;
    for alias in stmt.query_map(params![id.to_sql_blob()], |row| row.get::<_, String>(0))? {
        if let Ok(name) = TagName::new(alias?) {
            aliases.push(name);
        }
    }
    Ok(Some((
        Tag {
            id,
            vault,
            canonical_name: TagName::new(&canonical).map_err(|e| {
                crate::StorageError::Corrupt(format!("tag name {canonical:?}: {e}"))
            })?,
            display_name: display,
            aliases,
            created_at: crate::convert::time_from_sql(&created)?,
        },
        revision as u64,
    )))
}

/// One assignment row, by its pair.
pub fn get_assignment(
    conn: &Connection,
    document: DocumentId,
    tag: TagId,
) -> Result<Option<marius_model::DocumentTag>> {
    use marius_model::{Confidence, DocumentTag};
    let row: Option<AssignmentRow> = conn
        .query_row(
            "SELECT provenance, review, source, confidence, model, assigned_at
             FROM document_tag WHERE document_id = ?1 AND tag_id = ?2",
            params![document.to_sql_blob(), tag.to_sql_blob()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((provenance, review, source, confidence, model, assigned_at)) = row else {
        return Ok(None);
    };
    let corrupt = |what: &str| crate::StorageError::Corrupt(format!("document_tag {what}"));
    Ok(Some(DocumentTag {
        document,
        tag,
        provenance: provenance.parse().map_err(|_| corrupt("provenance"))?,
        review: review.parse().map_err(|_| corrupt("review"))?,
        source: source.parse().map_err(|_| corrupt("source"))?,
        confidence: confidence.map(|c| Confidence::new(c as f32)),
        model,
        assigned_at: crate::convert::time_from_sql(&assigned_at)?,
    }))
}

/// A stored tag rejection's reason, if the rejection exists.
pub fn get_rejected_tag(
    conn: &Connection,
    document: DocumentId,
    tag: TagId,
) -> Result<Option<Option<String>>> {
    Ok(conn
        .query_row(
            "SELECT reason FROM rejected_tag WHERE document_id = ?1 AND tag_id = ?2",
            params![document.to_sql_blob(), tag.to_sql_blob()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?)
}

/// Store a replicated relationship rejection, silently, and remove any
/// machine edge it suppresses — mirroring the local rejection path.
pub fn reject_relationship(
    conn: &Connection,
    rejection: &marius_model::RejectedRelationship,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO rejected_relationship
            (source_vault, source_document, target_vault, target_document, kind,
             rejected_at, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            rejection.endpoints.source.vault.to_sql_blob(),
            rejection.endpoints.source.document.to_sql_blob(),
            rejection.endpoints.target.vault.to_sql_blob(),
            rejection.endpoints.target.document.to_sql_blob(),
            rejection.kind.as_str(),
            time_to_sql(rejection.rejected_at),
            rejection.reason,
        ],
    )?;
    conn.execute(
        "DELETE FROM relationship
         WHERE source_document = ?1 AND target_document = ?2 AND kind = ?3
           AND provenance IN ('ai_generated', 'derived')",
        params![
            rejection.endpoints.source.document.to_sql_blob(),
            rejection.endpoints.target.document.to_sql_blob(),
            rejection.kind.as_str(),
        ],
    )?;
    Ok(())
}

/// Give an existing document a different id — the migration half of
/// KB-014, when an export's identity must survive into a re-indexed
/// vault. Children move with it; foreign keys are deferred to commit.
pub fn adopt_document_id(conn: &Connection, from: DocumentId, to: DocumentId) -> Result<()> {
    if from == to {
        return Ok(());
    }
    conn.execute_batch("PRAGMA defer_foreign_keys = ON")?;
    let old = from.to_sql_blob();
    let new = to.to_sql_blob();
    conn.execute(
        "UPDATE document SET id = ?2 WHERE id = ?1",
        params![old, new],
    )?;
    for sql in [
        "UPDATE document_tag SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE rejected_tag SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE document_name SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE document_alias SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE chunk SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE chunk_embedding SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE document_revision SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE pending_link SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE generated_note_provenance SET document_id = ?2 WHERE document_id = ?1",
        "UPDATE relationship SET source_document = ?2 WHERE source_document = ?1",
        "UPDATE relationship SET target_document = ?2 WHERE target_document = ?1",
        "UPDATE rejected_relationship SET source_document = ?2 WHERE source_document = ?1",
        "UPDATE rejected_relationship SET target_document = ?2 WHERE target_document = ?1",
    ] {
        conn.execute(sql, params![old, new])?;
    }
    Ok(())
}
