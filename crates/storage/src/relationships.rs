//! Graph edges: stored links and inferences, computed tag kinship.
//!
//! Requirements: GRAPH-002, GRAPH-003, GRAPH-007 .. GRAPH-013, GRAPH-015,
//! SYNC-012, AC-006, AC-009, integrity rules 2 and 7.
//!
//! Three decisions shape this module:
//!
//! * **Shared-tag kinship is computed, never stored.** A tag used by 2,000
//!   notes would imply two million edges. [`shared_tag_neighbours`] derives
//!   them on demand instead, exactly as the spec's graph-interpretation note
//!   calls for, while inferred and user-asserted edges are stored as rows
//!   because they cannot be recomputed.
//! * **Endpoints carry no foreign key.** A cross-vault edge has to keep working
//!   while the far vault is unmounted (SYNC-012, AC-009), so a dangling
//!   endpoint is a normal state that read paths resolve, not a violation the
//!   database forbids.
//! * **Rejections are keyed by what was rejected, not by the row that was
//!   deleted** — so a fresh proposal of the same edge, with a new ID from a
//!   different model, is still suppressed (GRAPH-011, integrity rule 7).

use marius_model::{
    ChangeKind, Confidence, DocumentId, Endpoints, EntityKind, GlobalDocumentId, Provenance,
    RejectedRelationship, Relationship, RelationshipId, RelationshipKind, ReviewState, VaultId,
};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use time::OffsetDateTime;

use crate::convert::{
    id_from_row, opt_confidence_from_row, opt_f32_to_sql, parsed_from_row, time_from_row,
    time_to_sql, IdSql,
};
use crate::{changelog, Result, StorageError};

const COLUMNS: &str = "id, source_vault, source_document, target_vault, target_document, kind, \
                       provenance, review, confidence, model, rationale, created_at, updated_at";

/// What [`upsert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upserted {
    Inserted,
    /// An existing machine-owned edge was refreshed.
    Updated,
    /// The user rejected this exact relationship (GRAPH-011).
    SkippedRejected,
    /// A user-asserted or confirmed edge already exists (GRAPH-012).
    SkippedProtected,
    /// Both endpoints were the same document.
    SkippedSelfLoop,
}

impl Upserted {
    pub fn changed(self) -> bool {
        matches!(self, Self::Inserted | Self::Updated)
    }
}

/// Store an edge, respecting suppressions and user ownership.
pub fn upsert(conn: &Connection, edge: &Relationship) -> Result<Upserted> {
    if edge.kind.is_computed() {
        return Err(StorageError::Corrupt(format!(
            "{} relationships are computed and must not be stored",
            edge.kind
        )));
    }
    let endpoints = edge.endpoints.normalized_for(edge.kind);
    if endpoints.is_self_loop() {
        return Ok(Upserted::SkippedSelfLoop);
    }

    if is_rejected(conn, endpoints, edge.kind)? {
        // A user-asserted edge is the user changing their mind, and clears the
        // suppression; a machine proposal is dropped.
        if !edge.provenance.is_user_authored() {
            return Ok(Upserted::SkippedRejected);
        }
        unreject(conn, endpoints, edge.kind)?;
    }

    let existing: Option<(Vec<u8>, String, String, i64)> = conn
        .query_row(
            "SELECT id, provenance, review, revision FROM relationship
             WHERE source_document = ?1 AND target_document = ?2 AND kind = ?3",
            params![
                endpoints.source.document.to_sql_blob(),
                endpoints.target.document.to_sql_blob(),
                edge.kind.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    if let Some((id, provenance, review, revision)) = existing {
        let provenance: Provenance =
            provenance
                .parse()
                .map_err(|e: marius_model::provenance::UnknownProvenance| {
                    StorageError::Corrupt(e.to_string())
                })?;
        let review: ReviewState =
            review
                .parse()
                .map_err(|e: marius_model::provenance::UnknownReviewState| {
                    StorageError::Corrupt(e.to_string())
                })?;
        if (!provenance.is_machine_writable() || review.pins_assertion())
            && !edge.provenance.is_user_authored()
        {
            return Ok(Upserted::SkippedProtected);
        }
        let existing_id = RelationshipId::from_sql_blob(&id)?;
        update(conn, existing_id, edge, revision as u64 + 1)?;
        return Ok(Upserted::Updated);
    }

    insert(conn, edge, endpoints)?;
    Ok(Upserted::Inserted)
}

fn insert(conn: &Connection, edge: &Relationship, endpoints: Endpoints) -> Result<()> {
    conn.execute(
        "INSERT INTO relationship
            (id, source_vault, source_document, target_vault, target_document, kind,
             provenance, review, confidence, model, rationale, created_at, updated_at, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1)",
        params![
            edge.id.to_sql_blob(),
            endpoints.source.vault.to_sql_blob(),
            endpoints.source.document.to_sql_blob(),
            endpoints.target.vault.to_sql_blob(),
            endpoints.target.document.to_sql_blob(),
            edge.kind.as_str(),
            edge.provenance.as_str(),
            edge.review.as_str(),
            opt_f32_to_sql(edge.confidence.map(Confidence::get)),
            edge.model,
            edge.rationale,
            time_to_sql(edge.created_at),
            time_to_sql(edge.updated_at),
        ],
    )?;
    journal(conn, endpoints, edge.id, ChangeKind::Upsert, 1)
}

fn update(conn: &Connection, id: RelationshipId, edge: &Relationship, revision: u64) -> Result<()> {
    conn.execute(
        "UPDATE relationship SET
            provenance = ?2, review = ?3, confidence = ?4, model = ?5, rationale = ?6,
            updated_at = ?7, revision = ?8
         WHERE id = ?1",
        params![
            id.to_sql_blob(),
            edge.provenance.as_str(),
            edge.review.as_str(),
            opt_f32_to_sql(edge.confidence.map(Confidence::get)),
            edge.model,
            edge.rationale,
            time_to_sql(OffsetDateTime::now_utc()),
            revision as i64,
        ],
    )?;
    journal(
        conn,
        edge.endpoints.normalized_for(edge.kind),
        id,
        ChangeKind::Upsert,
        revision,
    )
}

/// Journal against the source endpoint's vault; a cross-vault edge belongs to
/// the side that asserted it.
fn journal(
    conn: &Connection,
    endpoints: Endpoints,
    id: RelationshipId,
    change: ChangeKind,
    revision: u64,
) -> Result<()> {
    changelog::record(
        conn,
        endpoints.source.vault,
        EntityKind::Relationship,
        &id.to_string(),
        change,
        revision,
    )?;
    Ok(())
}

/// Fetch one edge.
pub fn get(conn: &Connection, id: RelationshipId) -> Result<Option<Relationship>> {
    let sql = format!("SELECT {COLUMNS} FROM relationship WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![id.to_sql_blob()], row_to_relationship)
        .optional()?)
}

/// Fetch an edge by what it connects.
pub fn by_endpoints(
    conn: &Connection,
    endpoints: Endpoints,
    kind: RelationshipKind,
) -> Result<Option<Relationship>> {
    let endpoints = endpoints.normalized_for(kind);
    let sql = format!(
        "SELECT {COLUMNS} FROM relationship
         WHERE source_document = ?1 AND target_document = ?2 AND kind = ?3"
    );
    Ok(conn
        .query_row(
            &sql,
            params![
                endpoints.source.document.to_sql_blob(),
                endpoints.target.document.to_sql_blob(),
                kind.as_str()
            ],
            row_to_relationship,
        )
        .optional()?)
}

/// Edges where `document` is the source — its outgoing links (GRAPH-002).
pub fn outgoing(conn: &Connection, document: DocumentId) -> Result<Vec<Relationship>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM relationship WHERE source_document = ?1
         ORDER BY kind, created_at"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], row_to_relationship)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Edges where `document` is the target — its backlinks (GRAPH-002).
pub fn incoming(conn: &Connection, document: DocumentId) -> Result<Vec<Relationship>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM relationship WHERE target_document = ?1
         ORDER BY kind, created_at"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], row_to_relationship)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every stored edge touching `document`, in either direction (GRAPH-005).
pub fn touching(conn: &Connection, document: DocumentId) -> Result<Vec<Relationship>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM relationship
         WHERE source_document = ?1 OR target_document = ?1
         ORDER BY kind, created_at"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], row_to_relationship)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Edges of given kinds within a set of vaults, for the graph view.
pub fn in_vaults(
    conn: &Connection,
    vaults: &[VaultId],
    kinds: &[RelationshipKind],
    limit: Option<usize>,
) -> Result<Vec<Relationship>> {
    let mut sql = format!("SELECT {COLUMNS} FROM relationship WHERE 1 = 1");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if !vaults.is_empty() {
        let holes = vec!["?"; vaults.len()].join(", ");
        // Either endpoint being in scope keeps a cross-vault edge visible
        // from the vault the user is looking at (GRAPH-008).
        sql.push_str(&format!(
            " AND (source_vault IN ({holes}) OR target_vault IN ({holes}))"
        ));
        for _ in 0..2 {
            args.extend(vaults.iter().map(|v| v.to_sql_blob().into()));
        }
    }
    if !kinds.is_empty() {
        let holes = vec!["?"; kinds.len()].join(", ");
        sql.push_str(&format!(" AND kind IN ({holes})"));
        args.extend(kinds.iter().map(|k| k.as_str().to_owned().into()));
    }
    sql.push_str(" ORDER BY created_at");
    if let Some(limit) = limit {
        sql.push_str(" LIMIT ?");
        args.push((limit as i64).into());
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args), row_to_relationship)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Record user confirmation (GRAPH-010), which also protects the edge from
/// re-inference (GRAPH-012).
pub fn confirm(conn: &Connection, id: RelationshipId) -> Result<bool> {
    let row: Option<(i64, Vec<u8>)> = conn
        .query_row(
            "UPDATE relationship SET
                review = 'approved',
                provenance = CASE provenance WHEN 'ai_generated' THEN 'ai_confirmed'
                                             ELSE provenance END,
                updated_at = ?2,
                revision = revision + 1
             WHERE id = ?1
             RETURNING revision, source_vault",
            params![id.to_sql_blob(), time_to_sql(OffsetDateTime::now_utc())],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((revision, vault)) = row else {
        return Ok(false);
    };
    changelog::record(
        conn,
        VaultId::from_sql_blob(&vault)?,
        EntityKind::Relationship,
        &id.to_string(),
        ChangeKind::Upsert,
        revision as u64,
    )?;
    Ok(true)
}

/// Delete an edge without suppressing it. Re-inference may propose it again.
pub fn delete(conn: &Connection, id: RelationshipId) -> Result<bool> {
    let Some(edge) = get(conn, id)? else {
        return Ok(false);
    };
    conn.execute(
        "DELETE FROM relationship WHERE id = ?1",
        params![id.to_sql_blob()],
    )?;
    journal(conn, edge.endpoints, id, ChangeKind::Delete, 0)?;
    Ok(true)
}

/// Reject an edge: delete it and remember the decision (GRAPH-010, GRAPH-011).
pub fn reject(conn: &Connection, id: RelationshipId, reason: Option<&str>) -> Result<bool> {
    let Some(edge) = get(conn, id)? else {
        return Ok(false);
    };
    let rejection = edge.rejection(reason.map(str::to_owned));
    conn.execute(
        "DELETE FROM relationship WHERE id = ?1",
        params![id.to_sql_blob()],
    )?;
    store_rejection(conn, &rejection)?;
    journal(conn, edge.endpoints, id, ChangeKind::Delete, 0)?;
    Ok(true)
}

fn store_rejection(conn: &Connection, rejection: &RejectedRelationship) -> Result<()> {
    conn.execute(
        "INSERT INTO rejected_relationship
            (source_vault, source_document, target_vault, target_document, kind,
             rejected_at, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(source_document, target_document, kind) DO UPDATE SET
            rejected_at = excluded.rejected_at, reason = excluded.reason",
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
    changelog::record(
        conn,
        rejection.endpoints.source.vault,
        EntityKind::RejectedRelationship,
        &format!(
            "{}/{}/{}",
            rejection.endpoints.source.document,
            rejection.endpoints.target.document,
            rejection.kind
        ),
        ChangeKind::Upsert,
        1,
    )?;
    Ok(())
}

fn is_rejected(conn: &Connection, endpoints: Endpoints, kind: RelationshipKind) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM rejected_relationship
             WHERE source_document = ?1 AND target_document = ?2 AND kind = ?3",
            params![
                endpoints.source.document.to_sql_blob(),
                endpoints.target.document.to_sql_blob(),
                kind.as_str()
            ],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

/// Withdraw a suppression, so the relationship may be proposed again
/// (GRAPH-011's "unless requested").
pub fn unreject(conn: &Connection, endpoints: Endpoints, kind: RelationshipKind) -> Result<bool> {
    let endpoints = endpoints.normalized_for(kind);
    let removed = conn.execute(
        "DELETE FROM rejected_relationship
         WHERE source_document = ?1 AND target_document = ?2 AND kind = ?3",
        params![
            endpoints.source.document.to_sql_blob(),
            endpoints.target.document.to_sql_blob(),
            kind.as_str()
        ],
    )?;
    Ok(removed > 0)
}

/// Suppressions touching a document, so the UI can show what was dismissed.
pub fn rejections(conn: &Connection, document: DocumentId) -> Result<Vec<RejectedRelationship>> {
    let mut stmt = conn.prepare(
        "SELECT source_vault, source_document, target_vault, target_document, kind,
                rejected_at, reason
         FROM rejected_relationship
         WHERE source_document = ?1 OR target_document = ?1",
    )?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], |row| {
            Ok(RejectedRelationship {
                endpoints: Endpoints::new(
                    GlobalDocumentId::new(id_from_row(row, 0)?, id_from_row(row, 1)?),
                    GlobalDocumentId::new(id_from_row(row, 2)?, id_from_row(row, 3)?),
                ),
                kind: parsed_from_row(row, 4)?,
                rejected_at: time_from_row(row, 5)?,
                reason: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Replace the edges the *indexer* owns — those read out of the file's own
/// links.
///
/// Distinct from [`replace_machine_edges`] for the same reason the tag module
/// separates its two writers: re-reading a file must not delete a model's
/// inferred edges, and re-inference must not delete the `[[links]]` the user
/// wrote (GRAPH-002 alongside GRAPH-012).
pub fn replace_link_edges(
    conn: &Connection,
    document: DocumentId,
    proposed: &[Relationship],
) -> Result<Vec<Upserted>> {
    replace_edges(
        conn,
        document,
        &[RelationshipKind::Link, RelationshipKind::Embed],
        &["derived"],
        proposed,
    )
}

/// Replace the machine-owned edges of one kind originating at `document`.
///
/// The re-inference entry point (GRAPH-012): confirmed, locked and
/// user-asserted edges survive, suppressed ones stay suppressed, and stale
/// machine edges are removed.
pub fn replace_machine_edges(
    conn: &Connection,
    document: DocumentId,
    kind: RelationshipKind,
    proposed: &[Relationship],
) -> Result<Vec<Upserted>> {
    // Only `ai_generated`: a `derived` edge is one the indexer read out of the
    // file's own links, and re-inference does not own those.
    replace_edges(conn, document, &[kind], &["ai_generated"], proposed)
}

fn replace_edges(
    conn: &Connection,
    document: DocumentId,
    kinds: &[RelationshipKind],
    provenances: &[&str],
    proposed: &[Relationship],
) -> Result<Vec<Upserted>> {
    // The far endpoint of each proposed edge: the rows to keep.
    let self_ref = GlobalDocumentId::new(VaultId::nil(), document);
    let keep: Vec<Vec<u8>> = proposed
        .iter()
        .map(|p| {
            let endpoints = p.endpoints.normalized_for(p.kind);
            // `other` compares whole global ids, so fall back to matching on
            // the document alone — the vault half may differ.
            let far = if endpoints.source.document == document {
                endpoints.target.document
            } else if endpoints.target.document == document {
                endpoints.source.document
            } else {
                let _ = self_ref;
                endpoints.target.document
            };
            far.to_sql_blob()
        })
        .collect();

    let kind_holes = vec!["?"; kinds.len()].join(", ");
    let provenance_holes = vec!["?"; provenances.len()].join(", ");
    // A directed edge belongs to the document it starts from: `a.md` saying
    // `[[b]]` is a fact about `a.md`'s file, so re-reading `b.md` must not
    // delete it. An undirected edge has no owning side, so either endpoint
    // matches.
    let owner = if kinds.iter().all(|k| k.is_directed()) {
        "source_document = ?1"
    } else {
        "(source_document = ?1 OR target_document = ?1)"
    };
    // `NOT IN (NULL)` is NULL rather than true, so the clause is omitted when
    // there is nothing to keep.
    let exclusion = if keep.is_empty() {
        String::new()
    } else {
        let holes = vec!["?"; keep.len()].join(", ");
        format!(" AND target_document NOT IN ({holes}) AND source_document NOT IN ({holes})")
    };
    // `+kind`: keep the planner on the source/target_document indexes —
    // the (kind, review) index's entries grow with the vault, and
    // scanning them per document is the O(n²) that sank 100k-note
    // indexing in tags before the same hint.
    let sql = format!(
        "DELETE FROM relationship
         WHERE {owner}
           AND +kind IN ({kind_holes})
           AND provenance IN ({provenance_holes})
           AND review NOT IN ('approved', 'locked'){exclusion}"
    );
    let mut args: Vec<rusqlite::types::Value> = vec![document.to_sql_blob().into()];
    args.extend(kinds.iter().map(|k| k.as_str().to_owned().into()));
    args.extend(provenances.iter().map(|p| (*p).to_owned().into()));
    for _ in 0..if keep.is_empty() { 0 } else { 2 } {
        args.extend(keep.iter().map(|k| k.clone().into()));
    }
    conn.execute(&sql, params_from_iter(args))?;

    proposed.iter().map(|p| upsert(conn, p)).collect()
}

/// A document related to another by a shared tag, with how many they share.
///
/// Computed on demand rather than stored — see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagKinship {
    pub document: GlobalDocumentId,
    pub shared_tags: u64,
}

/// Documents sharing at least one tag with `document` (GRAPH-003).
///
/// Ordered by the number of tags in common, so the strongest kinship comes
/// first, and limited because a popular tag can relate thousands of notes.
pub fn shared_tag_neighbours(
    conn: &Connection,
    document: DocumentId,
    vaults: &[VaultId],
    limit: usize,
) -> Result<Vec<TagKinship>> {
    let mut sql = String::from(
        "SELECT d.vault_id, d.id, COUNT(DISTINCT mine.tag_id) AS shared
         FROM document_tag mine
         JOIN document_tag theirs ON theirs.tag_id = mine.tag_id
         JOIN document d ON d.id = theirs.document_id
         WHERE mine.document_id = ?1 AND theirs.document_id <> ?1",
    );
    let mut args: Vec<rusqlite::types::Value> = vec![document.to_sql_blob().into()];
    if !vaults.is_empty() {
        let holes = vec!["?"; vaults.len()].join(", ");
        sql.push_str(&format!(" AND d.vault_id IN ({holes})"));
        args.extend(vaults.iter().map(|v| v.to_sql_blob().into()));
    }
    sql.push_str(" GROUP BY d.id ORDER BY shared DESC, d.path ASC LIMIT ?");
    args.push((limit as i64).into());

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args), |row| {
            Ok(TagKinship {
                document: GlobalDocumentId::new(id_from_row(row, 0)?, id_from_row(row, 1)?),
                shared_tags: row.get::<_, i64>(2)? as u64,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Pairs of documents in `vault` that share at least one tag, for the
/// global graph view (GRAPH-003). Computed on demand, never stored.
///
/// A tag carried by more than `per_tag_cap` documents is skipped: a broad
/// topic tag on hundreds of notes produces a quadratic blow-up of edges that
/// say nothing about any particular pair (GRAPH-014). Pairs are ranked by how
/// many tags they share, capped at `limit`.
pub fn co_tagged_in_vault(
    conn: &Connection,
    vault: VaultId,
    per_tag_cap: u32,
    limit: usize,
) -> Result<Vec<(DocumentId, DocumentId, u64)>> {
    let mut stmt = conn.prepare(
        "WITH scoped AS (
             SELECT dt.document_id AS doc, dt.tag_id AS tag
             FROM document_tag dt
             JOIN document d ON d.id = dt.document_id
             WHERE d.vault_id = ?1
         ),
         narrow AS (
             SELECT tag FROM scoped GROUP BY tag HAVING COUNT(*) BETWEEN 2 AND ?2
         )
         SELECT a.doc, b.doc, COUNT(*) AS shared
         FROM scoped a
         JOIN scoped b ON a.tag = b.tag AND a.doc < b.doc
         JOIN narrow n ON n.tag = a.tag
         GROUP BY a.doc, b.doc
         ORDER BY shared DESC, a.doc, b.doc
         LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            rusqlite::params![vault.to_sql_blob(), per_tag_cap, limit as i64],
            |row| {
                Ok((
                    id_from_row(row, 0)?,
                    id_from_row(row, 1)?,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Record a `[[link]]` whose target does not exist yet (MD-007).
pub fn add_pending_link(
    conn: &Connection,
    document: DocumentId,
    vault: VaultId,
    raw_target: &str,
    kind: RelationshipKind,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO pending_link
            (document_id, vault_id, target_key, raw_target, kind)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            document.to_sql_blob(),
            vault.to_sql_blob(),
            crate::documents::fold_name(raw_target),
            raw_target,
            kind.as_str()
        ],
    )?;
    Ok(())
}

/// Forget one unresolved link, once it has been turned into a real edge.
pub fn remove_pending_link(
    conn: &Connection,
    document: DocumentId,
    raw_target: &str,
    kind: RelationshipKind,
) -> Result<bool> {
    let removed = conn.execute(
        "DELETE FROM pending_link WHERE document_id = ?1 AND target_key = ?2 AND kind = ?3",
        params![
            document.to_sql_blob(),
            crate::documents::fold_name(raw_target),
            kind.as_str()
        ],
    )?;
    Ok(removed > 0)
}

/// Forget every unresolved link recorded for a document.
pub fn clear_pending_links(conn: &Connection, document: DocumentId) -> Result<()> {
    conn.execute(
        "DELETE FROM pending_link WHERE document_id = ?1",
        params![document.to_sql_blob()],
    )?;
    Ok(())
}

/// Documents whose unresolved links would now match `name`.
///
/// Creating a note completes the links that were waiting for it, without a
/// full reindex.
pub fn documents_awaiting(
    conn: &Connection,
    vault: VaultId,
    name: &str,
) -> Result<Vec<(DocumentId, String, RelationshipKind)>> {
    let mut stmt = conn.prepare(
        "SELECT document_id, raw_target, kind FROM pending_link
         WHERE vault_id = ?1 AND target_key = ?2",
    )?;
    let rows = stmt
        .query_map(
            params![vault.to_sql_blob(), crate::documents::fold_name(name)],
            |row| Ok((id_from_row(row, 0)?, row.get(1)?, parsed_from_row(row, 2)?)),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every unresolved link in a vault, for the end-of-pass resolution sweep.
pub fn pending_in_vault(
    conn: &Connection,
    vault: VaultId,
) -> Result<Vec<(DocumentId, String, RelationshipKind)>> {
    let mut stmt = conn.prepare(
        "SELECT document_id, raw_target, kind FROM pending_link WHERE vault_id = ?1
         ORDER BY document_id, target_key",
    )?;
    let rows = stmt
        .query_map(params![vault.to_sql_blob()], |row| {
            Ok((id_from_row(row, 0)?, row.get(1)?, parsed_from_row(row, 2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Unresolved links from a document, for a broken-link report.
pub fn pending_links(conn: &Connection, document: DocumentId) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT raw_target FROM pending_link WHERE document_id = ?1 ORDER BY raw_target",
    )?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Total stored edges, for diagnostics.
pub fn count(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM relationship", [], |row| row.get(0))?;
    Ok(n as u64)
}

fn row_to_relationship(row: &rusqlite::Row<'_>) -> rusqlite::Result<Relationship> {
    Ok(Relationship {
        id: id_from_row(row, 0)?,
        endpoints: Endpoints::new(
            GlobalDocumentId::new(id_from_row(row, 1)?, id_from_row(row, 2)?),
            GlobalDocumentId::new(id_from_row(row, 3)?, id_from_row(row, 4)?),
        ),
        kind: parsed_from_row(row, 5)?,
        provenance: parsed_from_row(row, 6)?,
        review: parsed_from_row(row, 7)?,
        confidence: opt_confidence_from_row(row, 8)?,
        model: row.get(9)?,
        rationale: row.get(10)?,
        created_at: time_from_row(row, 11)?,
        updated_at: time_from_row(row, 12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{documents, tags, vaults, Database};
    use marius_model::{
        Authorship, ContentHash, Document, DocumentKind, DocumentTag, TagName, Vault, VaultPath,
        VaultPolicy,
    };
    use std::collections::BTreeMap;

    struct Fixture {
        db: Database,
        vault: VaultId,
        a: GlobalDocumentId,
        b: GlobalDocumentId,
    }

    fn fixture() -> Fixture {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::new_local("V", "/v");
        vaults::upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        let a = add(&db, vault.id, "a.md");
        let b = add(&db, vault.id, "b.md");
        Fixture {
            db,
            vault: vault.id,
            a,
            b,
        }
    }

    fn add(db: &Database, vault: VaultId, path: &str) -> GlobalDocumentId {
        let now = OffsetDateTime::now_utc();
        let doc = Document {
            id: DocumentId::new(),
            vault,
            path: VaultPath::new(path).unwrap(),
            title: path.trim_end_matches(".md").to_owned(),
            kind: DocumentKind::Markdown,
            authorship: Authorship::Source,
            content_hash: ContentHash::of(path.as_bytes()),
            size_bytes: 1,
            created_at: now,
            modified_at: now,
            indexed_at: now,
            archived_at: None,
            sensitivity: None,
            frontmatter: BTreeMap::new(),
            external: false,
        };
        documents::upsert(db.conn(), &doc).unwrap();
        doc.global_id()
    }

    #[test]
    fn a_link_round_trips_and_shows_up_as_a_backlink() {
        // GRAPH-002.
        let f = fixture();
        let edge = Relationship::new(Endpoints::new(f.a, f.b), RelationshipKind::Link);
        assert_eq!(upsert(f.db.conn(), &edge).unwrap(), Upserted::Inserted);

        let out = outgoing(f.db.conn(), f.a.document).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].endpoints.target, f.b);

        let back = incoming(f.db.conn(), f.b.document).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].endpoints.source, f.a);
        assert_eq!(back[0].id, edge.id);
    }

    #[test]
    fn computed_relationships_are_refused_for_storage() {
        // The spec's graph-interpretation note, enforced.
        let f = fixture();
        let edge = Relationship::new(Endpoints::new(f.a, f.b), RelationshipKind::SharedTag);
        assert!(upsert(f.db.conn(), &edge).is_err());
    }

    #[test]
    fn self_loops_are_dropped() {
        let f = fixture();
        let edge = Relationship::new(Endpoints::new(f.a, f.a), RelationshipKind::Link);
        assert_eq!(
            upsert(f.db.conn(), &edge).unwrap(),
            Upserted::SkippedSelfLoop
        );
        assert_eq!(count(f.db.conn()).unwrap(), 0);
    }

    #[test]
    fn an_undirected_edge_is_stored_once_whichever_way_it_arrives() {
        let f = fixture();
        let forward =
            Relationship::semantic(Endpoints::new(f.a, f.b), "m", Confidence::new(0.7), None);
        let backward =
            Relationship::semantic(Endpoints::new(f.b, f.a), "m", Confidence::new(0.8), None);
        assert_eq!(upsert(f.db.conn(), &forward).unwrap(), Upserted::Inserted);
        assert_eq!(upsert(f.db.conn(), &backward).unwrap(), Upserted::Updated);
        assert_eq!(count(f.db.conn()).unwrap(), 1);
    }

    #[test]
    fn a_rejected_edge_is_not_reproposed_in_either_direction() {
        // AC-006, GRAPH-011, integrity rule 7.
        let f = fixture();
        let edge =
            Relationship::semantic(Endpoints::new(f.a, f.b), "m", Confidence::new(0.7), None);
        upsert(f.db.conn(), &edge).unwrap();
        assert!(reject(f.db.conn(), edge.id, Some("unrelated")).unwrap());
        assert_eq!(count(f.db.conn()).unwrap(), 0);

        let reproposed = Relationship::semantic(
            Endpoints::new(f.b, f.a),
            "a-different-model",
            Confidence::CERTAIN,
            None,
        );
        assert_ne!(reproposed.id, edge.id);
        assert_eq!(
            upsert(f.db.conn(), &reproposed).unwrap(),
            Upserted::SkippedRejected
        );
        assert_eq!(rejections(f.db.conn(), f.a.document).unwrap().len(), 1);
    }

    #[test]
    fn rejecting_a_semantic_edge_does_not_suppress_an_explicit_link() {
        // The user's own `[[link]]` must stand.
        let f = fixture();
        let semantic =
            Relationship::semantic(Endpoints::new(f.a, f.b), "m", Confidence::new(0.7), None);
        upsert(f.db.conn(), &semantic).unwrap();
        reject(f.db.conn(), semantic.id, None).unwrap();

        let explicit = Relationship::new(Endpoints::new(f.a, f.b), RelationshipKind::Link);
        assert_eq!(upsert(f.db.conn(), &explicit).unwrap(), Upserted::Inserted);
    }

    #[test]
    fn the_user_may_assert_an_edge_they_previously_rejected() {
        let f = fixture();
        let edge =
            Relationship::semantic(Endpoints::new(f.a, f.b), "m", Confidence::new(0.7), None);
        upsert(f.db.conn(), &edge).unwrap();
        reject(f.db.conn(), edge.id, None).unwrap();

        let asserted = Relationship::new(Endpoints::new(f.a, f.b), RelationshipKind::Asserted);
        assert_eq!(upsert(f.db.conn(), &asserted).unwrap(), Upserted::Inserted);
    }

    #[test]
    fn deleting_without_rejecting_allows_re_inference() {
        let f = fixture();
        let edge =
            Relationship::semantic(Endpoints::new(f.a, f.b), "m", Confidence::new(0.7), None);
        upsert(f.db.conn(), &edge).unwrap();
        assert!(delete(f.db.conn(), edge.id).unwrap());

        let again =
            Relationship::semantic(Endpoints::new(f.a, f.b), "m", Confidence::new(0.7), None);
        assert_eq!(upsert(f.db.conn(), &again).unwrap(), Upserted::Inserted);
    }

    #[test]
    fn confirming_an_edge_protects_it_from_re_inference() {
        // GRAPH-012.
        let f = fixture();
        let edge =
            Relationship::semantic(Endpoints::new(f.a, f.b), "m", Confidence::new(0.5), None);
        upsert(f.db.conn(), &edge).unwrap();
        assert!(confirm(f.db.conn(), edge.id).unwrap());

        let stored = get(f.db.conn(), edge.id).unwrap().unwrap();
        assert_eq!(stored.provenance, Provenance::AiConfirmed);
        assert_eq!(stored.review, ReviewState::Approved);

        let replacement =
            Relationship::semantic(Endpoints::new(f.a, f.b), "other", Confidence::CERTAIN, None);
        assert_eq!(
            upsert(f.db.conn(), &replacement).unwrap(),
            Upserted::SkippedProtected
        );
    }

    #[test]
    fn re_inference_replaces_stale_edges_but_keeps_confirmed_ones() {
        // GRAPH-012 — the whole point of the predicate.
        let f = fixture();
        let c = add(&f.db, f.vault, "c.md");
        let d = add(&f.db, f.vault, "d.md");

        let confirmed =
            Relationship::semantic(Endpoints::new(f.a, f.b), "m", Confidence::new(0.6), None);
        let stale = Relationship::semantic(Endpoints::new(f.a, c), "m", Confidence::new(0.4), None);
        upsert(f.db.conn(), &confirmed).unwrap();
        upsert(f.db.conn(), &stale).unwrap();
        confirm(f.db.conn(), confirmed.id).unwrap();

        let fresh = Relationship::semantic(Endpoints::new(f.a, d), "m", Confidence::new(0.9), None);
        replace_machine_edges(
            f.db.conn(),
            f.a.document,
            RelationshipKind::Semantic,
            std::slice::from_ref(&fresh),
        )
        .unwrap();

        let remaining: Vec<GlobalDocumentId> = touching(f.db.conn(), f.a.document)
            .unwrap()
            .iter()
            .filter_map(|e| e.endpoints.other(f.a))
            .collect();
        assert!(remaining.contains(&f.b), "confirmed edge must survive");
        assert!(remaining.contains(&d), "fresh edge must be stored");
        assert!(!remaining.contains(&c), "stale machine edge must be gone");
    }

    #[test]
    fn re_inference_with_no_proposals_clears_only_machine_edges() {
        let f = fixture();
        let asserted = Relationship::new(Endpoints::new(f.a, f.b), RelationshipKind::Semantic);
        let mut manual = asserted.clone();
        manual.provenance = Provenance::Manual;
        manual.review = ReviewState::Approved;
        upsert(f.db.conn(), &manual).unwrap();

        replace_machine_edges(f.db.conn(), f.a.document, RelationshipKind::Semantic, &[]).unwrap();
        assert_eq!(count(f.db.conn()).unwrap(), 1, "a manual edge must survive");
    }

    #[test]
    fn re_reading_a_file_does_not_delete_its_incoming_backlinks() {
        // The backlink is a fact about the *other* document's file.
        let f = fixture();
        let inbound = Relationship::new(Endpoints::new(f.b, f.a), RelationshipKind::Link);
        upsert(f.db.conn(), &inbound).unwrap();

        // Re-reading `a`, which itself links to nothing.
        replace_link_edges(f.db.conn(), f.a.document, &[]).unwrap();

        let back = incoming(f.db.conn(), f.a.document).unwrap();
        assert_eq!(back.len(), 1, "the backlink must survive");
        assert_eq!(back[0].id, inbound.id);
    }

    #[test]
    fn re_reading_a_file_does_remove_its_own_stale_outgoing_links() {
        let f = fixture();
        let outbound = Relationship::new(Endpoints::new(f.a, f.b), RelationshipKind::Link);
        upsert(f.db.conn(), &outbound).unwrap();
        replace_link_edges(f.db.conn(), f.a.document, &[]).unwrap();
        assert!(outgoing(f.db.conn(), f.a.document).unwrap().is_empty());
    }

    #[test]
    fn re_reading_a_file_does_not_delete_inferred_edges() {
        // GRAPH-002 alongside GRAPH-012: each writer clears only its own rows.
        let f = fixture();
        let c = add(&f.db, f.vault, "c.md");
        let d = add(&f.db, f.vault, "d.md");

        let old_link = Relationship::new(Endpoints::new(f.a, f.b), RelationshipKind::Link);
        let inferred =
            Relationship::semantic(Endpoints::new(f.a, c), "m", Confidence::new(0.8), None);
        upsert(f.db.conn(), &old_link).unwrap();
        upsert(f.db.conn(), &inferred).unwrap();

        let new_link = Relationship::new(Endpoints::new(f.a, d), RelationshipKind::Link);
        replace_link_edges(f.db.conn(), f.a.document, &[new_link]).unwrap();

        let kinds: Vec<(RelationshipKind, GlobalDocumentId)> = touching(f.db.conn(), f.a.document)
            .unwrap()
            .iter()
            .map(|e| (e.kind, e.endpoints.other(f.a).unwrap()))
            .collect();
        assert!(
            kinds.contains(&(RelationshipKind::Semantic, c)),
            "{kinds:?}"
        );
        assert!(kinds.contains(&(RelationshipKind::Link, d)), "{kinds:?}");
        assert!(!kinds.contains(&(RelationshipKind::Link, f.b)), "{kinds:?}");
    }

    #[test]
    fn cross_vault_edges_survive_their_far_vault_becoming_unavailable() {
        // SYNC-012, AC-009 — the reason endpoints carry no foreign key.
        let f = fixture();
        let other = Vault::new_local("Other", "/other");
        vaults::upsert(f.db.conn(), &other, &VaultPolicy::conservative(other.id)).unwrap();
        let far = add(&f.db, other.id, "far.md");

        let edge = Relationship::new(Endpoints::new(f.a, far), RelationshipKind::Link);
        upsert(f.db.conn(), &edge).unwrap();
        assert!(edge.endpoints.is_cross_vault());

        vaults::set_available(f.db.conn(), other.id, false).unwrap();
        let still_there = get(f.db.conn(), edge.id).unwrap().unwrap();
        assert_eq!(still_there.endpoints.target, far);

        // And even removing the vault registration leaves the edge metadata.
        vaults::remove(f.db.conn(), other.id).unwrap();
        let orphaned = get(f.db.conn(), edge.id).unwrap().unwrap();
        assert_eq!(orphaned.endpoints.target, far);
        assert!(documents::get(f.db.conn(), far.document).unwrap().is_none());
    }

    #[test]
    fn graph_queries_see_cross_vault_edges_from_either_side() {
        // GRAPH-008.
        let f = fixture();
        let other = Vault::new_local("Other", "/other");
        vaults::upsert(f.db.conn(), &other, &VaultPolicy::conservative(other.id)).unwrap();
        let far = add(&f.db, other.id, "far.md");
        upsert(
            f.db.conn(),
            &Relationship::new(Endpoints::new(f.a, far), RelationshipKind::Link),
        )
        .unwrap();

        assert_eq!(
            in_vaults(f.db.conn(), &[f.vault], &[], None).unwrap().len(),
            1
        );
        assert_eq!(
            in_vaults(f.db.conn(), &[other.id], &[], None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn shared_tags_are_computed_and_ranked_by_overlap() {
        // GRAPH-003: no stored rows for this.
        let f = fixture();
        let c = add(&f.db, f.vault, "c.md");
        let history = tags::ensure(
            f.db.conn(),
            Some(f.vault),
            &TagName::new("history").unwrap(),
        )
        .unwrap();
        let lighthouse = tags::ensure(
            f.db.conn(),
            Some(f.vault),
            &TagName::new("lighthouse").unwrap(),
        )
        .unwrap();

        for tag in [history.id, lighthouse.id] {
            tags::assign(f.db.conn(), &DocumentTag::manual(f.a.document, tag)).unwrap();
            tags::assign(f.db.conn(), &DocumentTag::manual(f.b.document, tag)).unwrap();
        }
        tags::assign(f.db.conn(), &DocumentTag::manual(c.document, history.id)).unwrap();

        let kin = shared_tag_neighbours(f.db.conn(), f.a.document, &[f.vault], 10).unwrap();
        assert_eq!(kin.len(), 2);
        assert_eq!(kin[0].document, f.b);
        assert_eq!(kin[0].shared_tags, 2);
        assert_eq!(kin[1].document, c);
        assert_eq!(kin[1].shared_tags, 1);

        // And nothing was stored.
        assert_eq!(count(f.db.conn()).unwrap(), 0);
    }

    #[test]
    fn co_tagged_pairs_skip_broad_tags_and_rank_by_overlap() {
        // GRAPH-003 globally, GRAPH-014: a tag on more documents than the cap
        // contributes no pairwise edges.
        let f = fixture();
        let c = add(&f.db, f.vault, "c.md");
        let narrow =
            tags::ensure(f.db.conn(), Some(f.vault), &TagName::new("niche").unwrap()).unwrap();
        let narrow2 =
            tags::ensure(f.db.conn(), Some(f.vault), &TagName::new("focus").unwrap()).unwrap();
        let broad =
            tags::ensure(f.db.conn(), Some(f.vault), &TagName::new("todo").unwrap()).unwrap();

        // a and b share two narrow tags; everything carries the broad one.
        for tag in [narrow.id, narrow2.id] {
            tags::assign(f.db.conn(), &DocumentTag::manual(f.a.document, tag)).unwrap();
            tags::assign(f.db.conn(), &DocumentTag::manual(f.b.document, tag)).unwrap();
        }
        for doc in [f.a.document, f.b.document, c.document] {
            tags::assign(f.db.conn(), &DocumentTag::manual(doc, broad.id)).unwrap();
        }

        // Cap of 2 excludes "todo" (3 documents): only the a—b pair remains.
        let pairs = co_tagged_in_vault(f.db.conn(), f.vault, 2, 10).unwrap();
        assert_eq!(pairs.len(), 1);
        let (x, y, shared) = pairs[0];
        assert_eq!(shared, 2);
        let mut got = [x, y];
        got.sort();
        let mut want = [f.a.document, f.b.document];
        want.sort();
        assert_eq!(got, want);

        // With the cap lifted, the broad tag joins every pair once.
        let pairs = co_tagged_in_vault(f.db.conn(), f.vault, 100, 10).unwrap();
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].2, 3); // a—b: niche + focus + todo

        // The limit truncates from the top.
        assert_eq!(
            co_tagged_in_vault(f.db.conn(), f.vault, 100, 1)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn shared_tag_neighbours_excludes_the_document_itself_and_honours_the_limit() {
        let f = fixture();
        let tag = tags::ensure(f.db.conn(), Some(f.vault), &TagName::new("t").unwrap()).unwrap();
        let mut docs = vec![f.a, f.b];
        for i in 0..5 {
            docs.push(add(&f.db, f.vault, &format!("extra{i}.md")));
        }
        for doc in &docs {
            tags::assign(f.db.conn(), &DocumentTag::manual(doc.document, tag.id)).unwrap();
        }

        let kin = shared_tag_neighbours(f.db.conn(), f.a.document, &[f.vault], 3).unwrap();
        assert_eq!(kin.len(), 3);
        assert!(kin.iter().all(|k| k.document != f.a));
    }

    #[test]
    fn unresolved_links_are_remembered_and_matched_when_the_target_appears() {
        // MD-007: creating the target completes the link without a reindex.
        let f = fixture();
        add_pending_link(
            f.db.conn(),
            f.a.document,
            f.vault,
            "Future Note",
            RelationshipKind::Link,
        )
        .unwrap();
        assert_eq!(
            pending_links(f.db.conn(), f.a.document).unwrap(),
            vec!["Future Note"]
        );

        let waiting = documents_awaiting(f.db.conn(), f.vault, "future note").unwrap();
        assert_eq!(
            waiting,
            vec![(
                f.a.document,
                "Future Note".to_string(),
                RelationshipKind::Link
            )]
        );

        // A completed link is forgotten individually.
        assert!(remove_pending_link(
            f.db.conn(),
            f.a.document,
            "Future Note",
            RelationshipKind::Link
        )
        .unwrap());
        assert!(pending_links(f.db.conn(), f.a.document).unwrap().is_empty());
        relationships_add_back(&f);

        clear_pending_links(f.db.conn(), f.a.document).unwrap();
        assert!(documents_awaiting(f.db.conn(), f.vault, "Future Note")
            .unwrap()
            .is_empty());
    }

    fn relationships_add_back(f: &Fixture) {
        add_pending_link(
            f.db.conn(),
            f.a.document,
            f.vault,
            "Future Note",
            RelationshipKind::Link,
        )
        .unwrap();
    }

    #[test]
    fn edges_can_be_filtered_by_kind_for_the_graph_view() {
        // GRAPH-004.
        let f = fixture();
        let c = add(&f.db, f.vault, "c.md");
        upsert(
            f.db.conn(),
            &Relationship::new(Endpoints::new(f.a, f.b), RelationshipKind::Link),
        )
        .unwrap();
        upsert(
            f.db.conn(),
            &Relationship::semantic(Endpoints::new(f.a, c), "m", Confidence::new(0.5), None),
        )
        .unwrap();

        let links = in_vaults(f.db.conn(), &[f.vault], &[RelationshipKind::Link], None).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, RelationshipKind::Link);
        assert_eq!(
            in_vaults(f.db.conn(), &[f.vault], &[], None).unwrap().len(),
            2
        );
    }

    #[test]
    fn edge_metadata_survives_a_round_trip() {
        // GRAPH-009, UI-010.
        let f = fixture();
        let edge = Relationship::semantic(
            Endpoints::new(f.a, f.b),
            "llama3.2",
            Confidence::new(0.77),
            Some("both discuss harbour navigation".into()),
        );
        upsert(f.db.conn(), &edge).unwrap();
        let got = get(f.db.conn(), edge.id).unwrap().unwrap();
        assert_eq!(got.model.as_deref(), Some("llama3.2"));
        assert_eq!(
            got.rationale.as_deref(),
            Some("both discuss harbour navigation")
        );
        assert_eq!(got.confidence.unwrap().get(), 0.77);
        assert_eq!(got.kind, RelationshipKind::Semantic);
    }
}
