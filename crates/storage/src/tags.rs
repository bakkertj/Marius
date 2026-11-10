//! Tags, assignments, approvals and rejections.
//!
//! Requirements: TAG-001 .. TAG-013, AC-004, AC-005, AC-006, integrity rules 4
//! and 7.
//!
//! Two rules are enforced here and nowhere else, because enforcing them at the
//! storage boundary means no caller can get them wrong:
//!
//! 1. **A machine may not overwrite a person.** [`assign`] refuses to replace
//!    an existing assignment whose provenance or review state makes it
//!    user-owned (TAG-006, integrity rule 4).
//! 2. **A rejection outlives what it rejected.** [`reject`] writes a
//!    `rejected_tag` row keyed by document and tag, and [`assign`] consults it
//!    before inserting, so reindexing or a sync round cannot bring the tag back
//!    (TAG-012, AC-006, integrity rule 7).

use marius_model::{
    ChangeKind, Confidence, DocumentId, DocumentTag, EntityKind, Provenance, RejectedTag,
    ReviewState, Tag, TagId, TagName, TagSource, VaultId,
};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use time::OffsetDateTime;

use crate::convert::{
    id_from_row, opt_confidence_from_row, opt_f32_to_sql, parsed_from_row, time_from_row,
    time_to_sql, vault_from_nil, vault_or_nil, IdSql,
};
use crate::{changelog, Result, StorageError};

const TAG_COLUMNS: &str = "id, vault_id, canonical_name, display_name, created_at";

/// Find an existing tag by canonical name or alias, or create it.
///
/// This is the mechanism behind TAG-008: a model proposing `Knowledge
/// Management` is routed to the existing `knowledge-management` tag — or to
/// whatever tag declares it as an alias — instead of minting a near-duplicate.
pub fn ensure(conn: &Connection, vault: Option<VaultId>, name: &TagName) -> Result<Tag> {
    if let Some(existing) = by_name(conn, vault, name)? {
        return Ok(existing);
    }
    let tag = Tag::new(vault, name.clone());
    insert(conn, &tag)?;
    Ok(tag)
}

fn insert(conn: &Connection, tag: &Tag) -> Result<u64> {
    let revision: i64 = conn.query_row(
        "INSERT INTO tag (id, vault_id, canonical_name, display_name, created_at, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, 1)
         ON CONFLICT(id) DO UPDATE SET
            canonical_name = excluded.canonical_name,
            display_name = excluded.display_name,
            revision = tag.revision + 1
         RETURNING revision",
        params![
            tag.id.to_sql_blob(),
            vault_or_nil(tag.vault),
            tag.canonical_name.as_str(),
            tag.display_name,
            time_to_sql(tag.created_at),
        ],
        |row| row.get(0),
    )?;
    set_aliases(conn, tag.id, &tag.aliases)?;
    changelog::record(
        conn,
        tag.vault.unwrap_or_else(VaultId::nil),
        EntityKind::Tag,
        &tag.id.to_string(),
        ChangeKind::Upsert,
        revision as u64,
    )?;
    Ok(revision as u64)
}

/// Look a tag up by canonical name, then by alias (TAG-010).
pub fn by_name(conn: &Connection, vault: Option<VaultId>, name: &TagName) -> Result<Option<Tag>> {
    let scopes = [vault_or_nil(vault), vault_or_nil(None)];
    for scope in scopes.iter().take(if vault.is_some() { 2 } else { 1 }) {
        let sql =
            format!("SELECT {TAG_COLUMNS} FROM tag WHERE vault_id = ?1 AND canonical_name = ?2");
        if let Some(id) = conn
            .query_row(&sql, params![scope, name.as_str()], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .optional()?
        {
            return load(conn, TagId::from_sql_blob(&id)?);
        }
        if let Some(id) = conn
            .query_row(
                "SELECT t.id FROM tag_alias a JOIN tag t ON t.id = a.tag_id
                 WHERE t.vault_id = ?1 AND a.alias = ?2 LIMIT 1",
                params![scope, name.as_str()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        {
            return load(conn, TagId::from_sql_blob(&id)?);
        }
    }
    Ok(None)
}

/// Fetch a tag with its aliases.
pub fn load(conn: &Connection, id: TagId) -> Result<Option<Tag>> {
    let sql = format!("SELECT {TAG_COLUMNS} FROM tag WHERE id = ?1");
    let Some(mut tag) = conn
        .query_row(&sql, params![id.to_sql_blob()], row_to_tag)
        .optional()?
    else {
        return Ok(None);
    };
    tag.aliases = aliases(conn, id)?;
    Ok(Some(tag))
}

/// Replace a tag's aliases.
pub fn set_aliases(conn: &Connection, tag: TagId, aliases: &[TagName]) -> Result<()> {
    conn.execute(
        "DELETE FROM tag_alias WHERE tag_id = ?1",
        params![tag.to_sql_blob()],
    )?;
    let mut stmt =
        conn.prepare_cached("INSERT OR IGNORE INTO tag_alias (tag_id, alias) VALUES (?1, ?2)")?;
    for alias in aliases {
        stmt.execute(params![tag.to_sql_blob(), alias.as_str()])?;
    }
    Ok(())
}

/// A tag's aliases.
pub fn aliases(conn: &Connection, tag: TagId) -> Result<Vec<TagName>> {
    let mut stmt = conn.prepare("SELECT alias FROM tag_alias WHERE tag_id = ?1 ORDER BY alias")?;
    let rows: Vec<String> = stmt
        .query_map(params![tag.to_sql_blob()], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows.iter().filter_map(|a| TagName::new(a).ok()).collect())
}

/// Every tag visible to a vault, canonical-name order.
pub fn list(conn: &Connection, vault: Option<VaultId>) -> Result<Vec<Tag>> {
    let sql =
        format!("SELECT {TAG_COLUMNS} FROM tag WHERE vault_id IN (?1, ?2) ORDER BY canonical_name");
    let mut stmt = conn.prepare(&sql)?;
    let mut tags: Vec<Tag> = stmt
        .query_map(params![vault_or_nil(vault), vault_or_nil(None)], row_to_tag)?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    for tag in &mut tags {
        tag.aliases = aliases(conn, tag.id)?;
    }
    Ok(tags)
}

/// Rename a tag globally, keeping every assignment (TAG-010).
///
/// Because assignments reference the tag's ID and never its name, a rename is
/// one `UPDATE` and cannot orphan anything.
pub fn rename(conn: &Connection, id: TagId, new_name: &TagName, display: &str) -> Result<()> {
    let row: Option<(i64, Vec<u8>)> = conn
        .query_row(
            "UPDATE tag SET canonical_name = ?2, display_name = ?3, revision = revision + 1
             WHERE id = ?1 RETURNING revision, vault_id",
            params![id.to_sql_blob(), new_name.as_str(), display],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((revision, vault)) = row else {
        return Err(StorageError::NotFound {
            entity: "tag",
            id: id.to_string(),
        });
    };
    changelog::record(
        conn,
        VaultId::from_sql_blob(&vault)?,
        EntityKind::Tag,
        &id.to_string(),
        ChangeKind::Upsert,
        revision as u64,
    )?;
    Ok(())
}

/// Merge `from` into `into`, moving assignments and keeping the old name as an
/// alias so existing links and queries keep working (TAG-010).
pub fn merge(conn: &Connection, from: TagId, into: TagId) -> Result<usize> {
    if from == into {
        return Ok(0);
    }
    let Some(source) = load(conn, from)? else {
        return Err(StorageError::NotFound {
            entity: "tag",
            id: from.to_string(),
        });
    };
    // Move assignments that the target does not already have. `OR IGNORE`
    // resolves the overlap in favour of the assignment already on the target,
    // which is the one that has been reviewed.
    let moved = conn.execute(
        "UPDATE OR IGNORE document_tag SET tag_id = ?2 WHERE tag_id = ?1",
        params![from.to_sql_blob(), into.to_sql_blob()],
    )?;
    conn.execute(
        "UPDATE OR IGNORE rejected_tag SET tag_id = ?2 WHERE tag_id = ?1",
        params![from.to_sql_blob(), into.to_sql_blob()],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO tag_alias (tag_id, alias) VALUES (?1, ?2)",
        params![into.to_sql_blob(), source.canonical_name.as_str()],
    )?;
    for alias in &source.aliases {
        conn.execute(
            "INSERT OR IGNORE INTO tag_alias (tag_id, alias) VALUES (?1, ?2)",
            params![into.to_sql_blob(), alias.as_str()],
        )?;
    }
    conn.execute("DELETE FROM tag WHERE id = ?1", params![from.to_sql_blob()])?;
    changelog::record(
        conn,
        source.vault.unwrap_or_else(VaultId::nil),
        EntityKind::Tag,
        &from.to_string(),
        ChangeKind::Delete,
        0,
    )?;
    Ok(moved)
}

/// What [`assign`] did, so callers can report honestly rather than assuming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assigned {
    /// A new assignment was stored.
    Inserted,
    /// An existing machine-owned assignment was replaced.
    Updated,
    /// The user previously rejected this tag for this document (TAG-012).
    SkippedRejected,
    /// A user-authored or pinned assignment already exists (TAG-006).
    SkippedProtected,
}

impl Assigned {
    /// True when the database changed.
    pub fn changed(self) -> bool {
        matches!(self, Self::Inserted | Self::Updated)
    }
}

/// Apply one tag assignment, respecting rejections and user ownership.
pub fn assign(conn: &Connection, assignment: &DocumentTag) -> Result<Assigned> {
    assign_impl(conn, assignment, true)
}

/// [`assign`] without the journal entry, for applying *replicated*
/// assignments: re-journaling what another device just told us would echo
/// the operation straight back to it. Every gate still applies — a durable
/// rejection suppresses a synced machine proposal exactly as it suppresses
/// a reindexed one (integrity rule 7).
pub fn assign_replica(conn: &Connection, assignment: &DocumentTag) -> Result<Assigned> {
    assign_impl(conn, assignment, false)
}

fn assign_impl(conn: &Connection, assignment: &DocumentTag, journal: bool) -> Result<Assigned> {
    let doc = assignment.document.to_sql_blob();
    let tag = assignment.tag.to_sql_blob();

    // A machine proposal for a tag the user rejected is dropped. A *manual*
    // assignment is allowed through: the user changing their mind is not the
    // classifier overriding them, and it clears the rejection.
    let rejected: bool = conn
        .query_row(
            "SELECT 1 FROM rejected_tag WHERE document_id = ?1 AND tag_id = ?2",
            params![&doc, &tag],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if rejected {
        if !assignment.provenance.is_user_authored() {
            return Ok(Assigned::SkippedRejected);
        }
        conn.execute(
            "DELETE FROM rejected_tag WHERE document_id = ?1 AND tag_id = ?2",
            params![&doc, &tag],
        )?;
    }

    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT provenance, review FROM document_tag WHERE document_id = ?1 AND tag_id = ?2",
            params![&doc, &tag],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    if let Some((provenance, review)) = existing {
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
        let protected = !provenance.is_machine_writable() || review.pins_assertion();
        // A user-authored incoming assignment may always upgrade the record;
        // a machine one may not touch a protected row.
        if protected && !assignment.provenance.is_user_authored() {
            return Ok(Assigned::SkippedProtected);
        }
        write_assignment(conn, assignment, journal)?;
        return Ok(Assigned::Updated);
    }

    write_assignment(conn, assignment, journal)?;
    Ok(Assigned::Inserted)
}

fn write_assignment(conn: &Connection, a: &DocumentTag, journal: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO document_tag
            (document_id, tag_id, provenance, review, source, confidence, model, assigned_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(document_id, tag_id) DO UPDATE SET
            provenance = excluded.provenance,
            review = excluded.review,
            source = excluded.source,
            confidence = excluded.confidence,
            model = excluded.model,
            assigned_at = excluded.assigned_at",
        params![
            a.document.to_sql_blob(),
            a.tag.to_sql_blob(),
            a.provenance.as_str(),
            a.review.as_str(),
            a.source.as_str(),
            opt_f32_to_sql(a.confidence.map(Confidence::get)),
            a.model,
            time_to_sql(a.assigned_at),
        ],
    )?;
    if journal {
        journal_assignment(conn, a.document, a.tag, ChangeKind::Upsert)?;
    }
    Ok(())
}

fn journal_assignment(
    conn: &Connection,
    document: DocumentId,
    tag: TagId,
    change: ChangeKind,
) -> Result<()> {
    let vault: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vault_id FROM document WHERE id = ?1",
            params![document.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    changelog::record(
        conn,
        vault
            .map(|v| VaultId::from_sql_blob(&v))
            .transpose()?
            .unwrap_or_else(VaultId::nil),
        EntityKind::DocumentTag,
        &format!("{document}/{tag}"),
        change,
        1,
    )?;
    Ok(())
}

/// Remove an assignment without recording a rejection.
///
/// Used when the indexer sees that an inline `#tag` has been deleted from the
/// body: the tag is gone, but the user did not *reject* it, so re-adding the
/// text must bring it back (TAG-004).
pub fn unassign(conn: &Connection, document: DocumentId, tag: TagId) -> Result<bool> {
    let removed = conn.execute(
        "DELETE FROM document_tag WHERE document_id = ?1 AND tag_id = ?2",
        params![document.to_sql_blob(), tag.to_sql_blob()],
    )?;
    if removed > 0 {
        journal_assignment(conn, document, tag, ChangeKind::Delete)?;
    }
    Ok(removed > 0)
}

/// Record the user's acceptance of an AI tag, promoting it to confirmed
/// (TAG-007).
pub fn approve(conn: &Connection, document: DocumentId, tag: TagId) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE document_tag
         SET review = 'approved',
             provenance = CASE provenance WHEN 'ai_generated' THEN 'ai_confirmed' ELSE provenance END
         WHERE document_id = ?1 AND tag_id = ?2",
        params![document.to_sql_blob(), tag.to_sql_blob()],
    )?;
    if changed > 0 {
        journal_assignment(conn, document, tag, ChangeKind::Upsert)?;
    }
    Ok(changed > 0)
}

/// Pin an assignment so automatic classification leaves it alone (TAG-012).
pub fn lock(conn: &Connection, document: DocumentId, tag: TagId) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE document_tag SET review = 'locked' WHERE document_id = ?1 AND tag_id = ?2",
        params![document.to_sql_blob(), tag.to_sql_blob()],
    )?;
    if changed > 0 {
        journal_assignment(conn, document, tag, ChangeKind::Upsert)?;
    }
    Ok(changed > 0)
}

/// Reject a tag for a document: remove it and remember the decision.
///
/// The rejection is what makes AC-006 pass — it survives reindexing and sync
/// because it is a durable row, not the absence of one.
pub fn reject(
    conn: &Connection,
    document: DocumentId,
    tag: TagId,
    reason: Option<&str>,
) -> Result<()> {
    conn.execute(
        "DELETE FROM document_tag WHERE document_id = ?1 AND tag_id = ?2",
        params![document.to_sql_blob(), tag.to_sql_blob()],
    )?;
    conn.execute(
        "INSERT INTO rejected_tag (document_id, tag_id, rejected_at, reason)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(document_id, tag_id) DO UPDATE SET
            rejected_at = excluded.rejected_at, reason = excluded.reason",
        params![
            document.to_sql_blob(),
            tag.to_sql_blob(),
            time_to_sql(OffsetDateTime::now_utc()),
            reason,
        ],
    )?;
    journal_assignment(conn, document, tag, ChangeKind::Delete)?;
    changelog::record(
        conn,
        VaultId::nil(),
        EntityKind::RejectedTag,
        &format!("{document}/{tag}"),
        ChangeKind::Upsert,
        1,
    )?;
    Ok(())
}

/// Withdraw a rejection, so the tag may be proposed again (GRAPH-011's
/// "unless requested" counterpart for tags).
pub fn unreject(conn: &Connection, document: DocumentId, tag: TagId) -> Result<bool> {
    let removed = conn.execute(
        "DELETE FROM rejected_tag WHERE document_id = ?1 AND tag_id = ?2",
        params![document.to_sql_blob(), tag.to_sql_blob()],
    )?;
    if removed > 0 {
        changelog::record(
            conn,
            VaultId::nil(),
            EntityKind::RejectedTag,
            &format!("{document}/{tag}"),
            ChangeKind::Delete,
            0,
        )?;
    }
    Ok(removed > 0)
}

/// Tags the user has rejected for a document.
pub fn rejections(conn: &Connection, document: DocumentId) -> Result<Vec<RejectedTag>> {
    let mut stmt = conn.prepare(
        "SELECT document_id, tag_id, rejected_at, reason FROM rejected_tag
         WHERE document_id = ?1",
    )?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], |row| {
            Ok(RejectedTag {
                document: id_from_row(row, 0)?,
                tag: id_from_row(row, 1)?,
                rejected_at: time_from_row(row, 2)?,
                reason: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every tag on a document, with its assignment metadata (TAG-002).
pub fn of_document(conn: &Connection, document: DocumentId) -> Result<Vec<(Tag, DocumentTag)>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.vault_id, t.canonical_name, t.display_name, t.created_at,
                dt.document_id, dt.tag_id, dt.provenance, dt.review, dt.source,
                dt.confidence, dt.model, dt.assigned_at
         FROM document_tag dt JOIN tag t ON t.id = dt.tag_id
         WHERE dt.document_id = ?1
         ORDER BY t.canonical_name",
    )?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], |row| {
            Ok((row_to_tag(row)?, row_to_assignment(row, 5)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Tag names for many documents at once.
///
/// The search path needs the tags of every hit on a page. Asking per hit is an
/// N+1 query: invisible at twenty rows and ruinous at ten thousand, which is
/// exactly the shape of bug that only shows up once there is real data.
pub fn names_for_documents(
    conn: &Connection,
    documents: &[DocumentId],
) -> Result<std::collections::HashMap<DocumentId, Vec<TagName>>> {
    let mut out: std::collections::HashMap<DocumentId, Vec<TagName>> =
        std::collections::HashMap::with_capacity(documents.len());
    if documents.is_empty() {
        return Ok(out);
    }
    // SQLite's default parameter limit is 999, so long pages are chunked.
    for batch in documents.chunks(500) {
        let holes = vec!["?"; batch.len()].join(", ");
        let sql = format!(
            "SELECT dt.document_id, t.canonical_name
             FROM document_tag dt JOIN tag t ON t.id = dt.tag_id
             WHERE dt.document_id IN ({holes})
             ORDER BY t.canonical_name"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(batch.iter().map(|d| d.to_sql_blob())),
            |row| {
                let id: DocumentId = id_from_row(row, 0)?;
                let name: String = row.get(1)?;
                Ok((id, name))
            },
        )?;
        for row in rows {
            let (id, name) = row?;
            if let Ok(parsed) = TagName::new(&name) {
                out.entry(id).or_default().push(parsed);
            }
        }
    }
    Ok(out)
}

/// Replace every machine-owned tag on a document with `proposed`.
///
/// This is the classifier's only write path (TAG-013). It deletes the
/// document's unreviewed *AI* tags, then applies the proposals through
/// [`assign`], so manual, imported, confirmed and locked tags survive
/// untouched and rejections still suppress.
///
/// It deliberately does **not** touch `derived` tags: those are the inline and
/// frontmatter tags the indexer read out of the file, and only
/// [`replace_file_tags`] owns them (TAG-004).
pub fn replace_machine_tags(
    conn: &Connection,
    document: DocumentId,
    proposed: &[DocumentTag],
) -> Result<Vec<Assigned>> {
    let keep: Vec<Vec<u8>> = proposed.iter().map(|p| p.tag.to_sql_blob()).collect();
    // `tag_id NOT IN (NULL)` is NULL, not true, so an empty proposal set would
    // silently delete nothing. The clause has to be omitted, not emptied.
    let exclusion = if keep.is_empty() {
        String::new()
    } else {
        format!(" AND tag_id NOT IN ({})", vec!["?"; keep.len()].join(", "))
    };
    let sql = format!(
        // `+provenance`: see replace_file_tags — keep the planner on the
        // document_id primary key.
        "DELETE FROM document_tag
         WHERE document_id = ?1
           AND +provenance = 'ai_generated'
           AND review NOT IN ('approved', 'locked'){exclusion}"
    );
    let mut args: Vec<rusqlite::types::Value> = vec![document.to_sql_blob().into()];
    args.extend(keep.into_iter().map(Into::into));
    conn.execute(&sql, params_from_iter(args))?;

    proposed.iter().map(|p| assign(conn, p)).collect()
}

/// Replace the tags the *indexer* owns — those read out of the file itself.
///
/// Distinct from [`replace_machine_tags`]: re-reading a file must not discard a
/// model's proposals, and re-running the classifier must not discard the
/// inline `#tags` in the body. Each writer only clears what it put there
/// (TAG-004 alongside TAG-006).
pub fn replace_file_tags(
    conn: &Connection,
    document: DocumentId,
    proposed: &[DocumentTag],
) -> Result<Vec<Assigned>> {
    let keep: Vec<Vec<u8>> = proposed.iter().map(|p| p.tag.to_sql_blob()).collect();
    let exclusion = if keep.is_empty() {
        String::new()
    } else {
        format!(" AND tag_id NOT IN ({})", vec!["?"; keep.len()].join(", "))
    };
    let sql = format!(
        // `+provenance`: the unary plus stops SQLite from choosing the
        // (provenance, review) index, whose 'derived' entries grow with
        // the vault — scanning them per document made indexing O(n²) at
        // 100k notes. The primary key on document_id is the right index.
        "DELETE FROM document_tag
         WHERE document_id = ?1
           AND +provenance = 'derived'
           AND source IN ('inline', 'frontmatter')
           AND review NOT IN ('approved', 'locked'){exclusion}"
    );
    let mut args: Vec<rusqlite::types::Value> = vec![document.to_sql_blob().into()];
    args.extend(keep.into_iter().map(Into::into));
    conn.execute(&sql, params_from_iter(args))?;

    proposed.iter().map(|p| assign(conn, p)).collect()
}

/// Documents carrying a tag, optionally including its hierarchy descendants
/// (TAG-002, TAG-010).
pub fn documents_with(
    conn: &Connection,
    tag: TagId,
    include_descendants: bool,
) -> Result<Vec<DocumentId>> {
    let ids = expand(conn, &[tag], include_descendants)?;
    let holes = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT DISTINCT dt.document_id FROM document_tag dt
         JOIN document d ON d.id = dt.document_id
         WHERE dt.tag_id IN ({holes}) ORDER BY d.path"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(
            params_from_iter(ids.iter().map(|t| t.to_sql_blob())),
            |row| id_from_row(row, 0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Expand tags to include every descendant in the hierarchy.
fn expand(conn: &Connection, tags: &[TagId], include_descendants: bool) -> Result<Vec<TagId>> {
    if !include_descendants || tags.is_empty() {
        return Ok(tags.to_vec());
    }
    let mut out: Vec<TagId> = tags.to_vec();
    for tag in tags {
        let Some(parent) = load(conn, *tag)? else {
            continue;
        };
        let prefix = format!("{}/", parent.canonical_name.as_str());
        let mut stmt = conn
            .prepare_cached("SELECT id FROM tag WHERE vault_id = ?1 AND canonical_name GLOB ?2")?;
        let children: Vec<TagId> = stmt
            .query_map(
                params![vault_or_nil(parent.vault), format!("{prefix}*")],
                |row| id_from_row(row, 0),
            )?
            .collect::<std::result::Result<_, _>>()?;
        for child in children {
            if !out.contains(&child) {
                out.push(child);
            }
        }
    }
    Ok(out)
}

/// A boolean tag query (TAG-003).
#[derive(Debug, Clone, Default)]
pub struct TagQuery {
    /// Documents must have all of these.
    pub all: Vec<TagId>,
    /// Documents must have at least one of these.
    pub any: Vec<TagId>,
    /// Documents must have none of these.
    pub none: Vec<TagId>,
    /// Treat each tag as matching its hierarchy descendants too (TAG-010).
    pub include_descendants: bool,
}

impl TagQuery {
    /// True when the query constrains nothing.
    pub fn is_empty(&self) -> bool {
        self.all.is_empty() && self.any.is_empty() && self.none.is_empty()
    }
}

/// Documents matching a boolean tag query (TAG-003).
pub fn documents_matching(
    conn: &Connection,
    vaults: &[VaultId],
    query: &TagQuery,
) -> Result<Vec<DocumentId>> {
    let mut sql = String::from("SELECT d.id FROM document d WHERE 1 = 1");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();

    if !vaults.is_empty() {
        let holes = vec!["?"; vaults.len()].join(", ");
        sql.push_str(&format!(" AND d.vault_id IN ({holes})"));
        args.extend(vaults.iter().map(|v| v.to_sql_blob().into()));
    }

    for tag in &expand(conn, &query.all, query.include_descendants)? {
        // Each AND term needs its own EXISTS: one join could be satisfied by a
        // single row matching any of the tags.
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM document_tag x WHERE x.document_id = d.id AND x.tag_id = ?)",
        );
        args.push(tag.to_sql_blob().into());
    }
    let any = expand(conn, &query.any, query.include_descendants)?;
    if !any.is_empty() {
        let holes = vec!["?"; any.len()].join(", ");
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM document_tag x WHERE x.document_id = d.id AND x.tag_id IN ({holes}))"
        ));
        args.extend(any.iter().map(|t| t.to_sql_blob().into()));
    }

    let none = expand(conn, &query.none, query.include_descendants)?;
    if !none.is_empty() {
        let holes = vec!["?"; none.len()].join(", ");
        sql.push_str(&format!(
            " AND NOT EXISTS (SELECT 1 FROM document_tag x WHERE x.document_id = d.id AND x.tag_id IN ({holes}))"
        ));
        args.extend(none.iter().map(|t| t.to_sql_blob().into()));
    }

    sql.push_str(" ORDER BY d.path");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args), |row| id_from_row(row, 0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Tag usage counts across every vault, for a global tag browser.
///
/// Distinct from [`counts`], where `None` means "the global tag scope" rather
/// than "all vaults" — a distinction that is right for lookup and wrong for
/// browsing.
pub fn counts_everywhere(conn: &Connection) -> Result<Vec<(Tag, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.vault_id, t.canonical_name, t.display_name, t.created_at,
                COUNT(dt.document_id)
         FROM tag t
         LEFT JOIN document_tag dt ON dt.tag_id = t.id
         GROUP BY t.id
         ORDER BY t.canonical_name",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row_to_tag(row)?, row.get::<_, i64>(5)? as u64))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Tag usage counts for the tag browser (TAG-002).
pub fn counts(conn: &Connection, vault: Option<VaultId>) -> Result<Vec<(Tag, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.vault_id, t.canonical_name, t.display_name, t.created_at,
                COUNT(dt.document_id)
         FROM tag t
         LEFT JOIN document_tag dt ON dt.tag_id = t.id
         WHERE t.vault_id IN (?1, ?2)
         GROUP BY t.id
         ORDER BY t.canonical_name",
    )?;
    let rows = stmt
        .query_map(params![vault_or_nil(vault), vault_or_nil(None)], |row| {
            Ok((row_to_tag(row)?, row.get::<_, i64>(5)? as u64))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_tag(row: &rusqlite::Row<'_>) -> rusqlite::Result<Tag> {
    let vault: VaultId = id_from_row(row, 1)?;
    let name: String = row.get(2)?;
    Ok(Tag {
        id: id_from_row(row, 0)?,
        vault: vault_from_nil(vault),
        canonical_name: TagName::new(&name).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?,
        display_name: row.get(3)?,
        aliases: Vec::new(),
        created_at: time_from_row(row, 4)?,
    })
}

fn row_to_assignment(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<DocumentTag> {
    Ok(DocumentTag {
        document: id_from_row(row, base)?,
        tag: id_from_row(row, base + 1)?,
        provenance: parsed_from_row(row, base + 2)?,
        review: parsed_from_row(row, base + 3)?,
        source: parsed_from_row::<TagSource>(row, base + 4)?,
        confidence: opt_confidence_from_row(row, base + 5)?,
        model: row.get(base + 6)?,
        assigned_at: time_from_row(row, base + 7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{documents, vaults, Database};
    use marius_model::{
        Authorship, ContentHash, Document, DocumentKind, Vault, VaultPath, VaultPolicy,
    };
    use std::collections::BTreeMap;

    struct Fixture {
        db: Database,
        vault: VaultId,
        doc: DocumentId,
    }

    fn fixture() -> Fixture {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::new_local("V", "/v");
        vaults::upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        let doc = document(vault.id, "a.md");
        documents::upsert(db.conn(), &doc).unwrap();
        Fixture {
            db,
            vault: vault.id,
            doc: doc.id,
        }
    }

    fn document(vault: VaultId, path: &str) -> Document {
        let now = OffsetDateTime::now_utc();
        Document {
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
        }
    }

    fn tag(f: &Fixture, name: &str) -> Tag {
        ensure(f.db.conn(), Some(f.vault), &TagName::new(name).unwrap()).unwrap()
    }

    #[test]
    fn ensure_is_idempotent_and_folds_spelling_variants() {
        // TAG-008.
        let f = fixture();
        let a = tag(&f, "Knowledge Management");
        let b = tag(&f, "knowledge_management");
        let c = tag(&f, "KNOWLEDGE-MANAGEMENT");
        assert_eq!(a.id, b.id);
        assert_eq!(a.id, c.id);
        assert_eq!(list(f.db.conn(), Some(f.vault)).unwrap().len(), 1);
    }

    #[test]
    fn an_alias_routes_a_proposal_to_the_existing_tag() {
        // TAG-008: this is what stops a model inventing a near-duplicate.
        let f = fixture();
        let canonical = tag(&f, "knowledge-management");
        set_aliases(f.db.conn(), canonical.id, &[TagName::new("pkm").unwrap()]).unwrap();

        let resolved = ensure(f.db.conn(), Some(f.vault), &TagName::new("PKM").unwrap()).unwrap();
        assert_eq!(resolved.id, canonical.id);
        assert_eq!(list(f.db.conn(), Some(f.vault)).unwrap().len(), 1);
    }

    #[test]
    fn manual_tags_are_never_replaced_by_the_classifier() {
        // TAG-006, AC-005, integrity rule 4.
        let f = fixture();
        let t = tag(&f, "history");
        assert_eq!(
            assign(f.db.conn(), &DocumentTag::manual(f.doc, t.id)).unwrap(),
            Assigned::Inserted
        );

        let proposal = DocumentTag::ai_generated(f.doc, t.id, "llama3.2", Confidence::new(0.9));
        assert_eq!(
            assign(f.db.conn(), &proposal).unwrap(),
            Assigned::SkippedProtected
        );

        let (_, stored) = of_document(f.db.conn(), f.doc).unwrap().remove(0);
        assert_eq!(stored.provenance, Provenance::Manual);
        assert_eq!(stored.model, None);
    }

    #[test]
    fn an_unreviewed_ai_tag_can_be_refined_by_a_later_pass() {
        let f = fixture();
        let t = tag(&f, "history");
        assign(
            f.db.conn(),
            &DocumentTag::ai_generated(f.doc, t.id, "small", Confidence::new(0.4)),
        )
        .unwrap();
        assert_eq!(
            assign(
                f.db.conn(),
                &DocumentTag::ai_generated(f.doc, t.id, "large", Confidence::new(0.95))
            )
            .unwrap(),
            Assigned::Updated
        );
        let (_, stored) = of_document(f.db.conn(), f.doc).unwrap().remove(0);
        assert_eq!(stored.model.as_deref(), Some("large"));
    }

    #[test]
    fn approving_promotes_an_ai_tag_and_then_protects_it() {
        // TAG-007.
        let f = fixture();
        let t = tag(&f, "history");
        assign(
            f.db.conn(),
            &DocumentTag::ai_generated(f.doc, t.id, "m", Confidence::new(0.5)),
        )
        .unwrap();
        assert!(approve(f.db.conn(), f.doc, t.id).unwrap());

        let (_, stored) = of_document(f.db.conn(), f.doc).unwrap().remove(0);
        assert_eq!(stored.provenance, Provenance::AiConfirmed);
        assert_eq!(stored.review, ReviewState::Approved);

        assert_eq!(
            assign(
                f.db.conn(),
                &DocumentTag::ai_generated(f.doc, t.id, "other", Confidence::CERTAIN)
            )
            .unwrap(),
            Assigned::SkippedProtected
        );
    }

    #[test]
    fn locking_pins_a_tag_against_reclassification() {
        // TAG-012.
        let f = fixture();
        let t = tag(&f, "history");
        assign(
            f.db.conn(),
            &DocumentTag::ai_generated(f.doc, t.id, "m", Confidence::new(0.5)),
        )
        .unwrap();
        assert!(lock(f.db.conn(), f.doc, t.id).unwrap());
        assert_eq!(
            assign(
                f.db.conn(),
                &DocumentTag::ai_generated(f.doc, t.id, "m2", Confidence::CERTAIN)
            )
            .unwrap(),
            Assigned::SkippedProtected
        );
    }

    #[test]
    fn a_rejected_tag_is_not_reproposed_even_by_a_different_model() {
        // AC-006, integrity rule 7 — the headline guarantee for rejections.
        let f = fixture();
        let t = tag(&f, "history");
        assign(
            f.db.conn(),
            &DocumentTag::ai_generated(f.doc, t.id, "m", Confidence::new(0.8)),
        )
        .unwrap();
        reject(f.db.conn(), f.doc, t.id, Some("not about history")).unwrap();

        assert!(of_document(f.db.conn(), f.doc).unwrap().is_empty());
        assert_eq!(
            assign(
                f.db.conn(),
                &DocumentTag::ai_generated(f.doc, t.id, "a-better-model", Confidence::CERTAIN)
            )
            .unwrap(),
            Assigned::SkippedRejected
        );
        assert_eq!(rejections(f.db.conn(), f.doc).unwrap().len(), 1);
    }

    #[test]
    fn a_rejection_survives_a_full_reclassification_pass() {
        // The reindex half of integrity rule 7.
        let f = fixture();
        let rejected = tag(&f, "history");
        let fresh = tag(&f, "architecture");
        reject(f.db.conn(), f.doc, rejected.id, None).unwrap();

        let outcomes = replace_machine_tags(
            f.db.conn(),
            f.doc,
            &[
                DocumentTag::ai_generated(f.doc, rejected.id, "m", Confidence::CERTAIN),
                DocumentTag::ai_generated(f.doc, fresh.id, "m", Confidence::CERTAIN),
            ],
        )
        .unwrap();
        assert_eq!(outcomes[0], Assigned::SkippedRejected);
        assert_eq!(outcomes[1], Assigned::Inserted);

        let names: Vec<String> = of_document(f.db.conn(), f.doc)
            .unwrap()
            .iter()
            .map(|(t, _)| t.canonical_name.to_string())
            .collect();
        assert_eq!(names, vec!["architecture"]);
    }

    #[test]
    fn the_user_may_apply_a_tag_they_previously_rejected() {
        // Changing your mind is not the classifier overriding you.
        let f = fixture();
        let t = tag(&f, "history");
        reject(f.db.conn(), f.doc, t.id, None).unwrap();
        assert_eq!(
            assign(f.db.conn(), &DocumentTag::manual(f.doc, t.id)).unwrap(),
            Assigned::Inserted
        );
        assert!(rejections(f.db.conn(), f.doc).unwrap().is_empty());
    }

    #[test]
    fn a_rejection_can_be_withdrawn() {
        let f = fixture();
        let t = tag(&f, "history");
        reject(f.db.conn(), f.doc, t.id, None).unwrap();
        assert!(unreject(f.db.conn(), f.doc, t.id).unwrap());
        assert_eq!(
            assign(
                f.db.conn(),
                &DocumentTag::ai_generated(f.doc, t.id, "m", Confidence::CERTAIN)
            )
            .unwrap(),
            Assigned::Inserted
        );
    }

    #[test]
    fn reclassification_preserves_user_tags_and_drops_stale_machine_tags() {
        // TAG-013 with TAG-006.
        let f = fixture();
        let manual = tag(&f, "manual-keep");
        let stale = tag(&f, "stale-machine");
        let fresh = tag(&f, "fresh-machine");
        assign(f.db.conn(), &DocumentTag::manual(f.doc, manual.id)).unwrap();
        assign(
            f.db.conn(),
            &DocumentTag::ai_generated(f.doc, stale.id, "m", Confidence::new(0.5)),
        )
        .unwrap();

        replace_machine_tags(
            f.db.conn(),
            f.doc,
            &[DocumentTag::ai_generated(
                f.doc,
                fresh.id,
                "m",
                Confidence::new(0.7),
            )],
        )
        .unwrap();

        let mut names: Vec<String> = of_document(f.db.conn(), f.doc)
            .unwrap()
            .iter()
            .map(|(t, _)| t.canonical_name.to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["fresh-machine", "manual-keep"]);
    }

    #[test]
    fn reclassification_with_no_proposals_clears_only_machine_tags() {
        let f = fixture();
        let manual = tag(&f, "keep");
        let machine = tag(&f, "drop");
        assign(f.db.conn(), &DocumentTag::manual(f.doc, manual.id)).unwrap();
        assign(
            f.db.conn(),
            &DocumentTag::ai_generated(f.doc, machine.id, "m", Confidence::new(0.5)),
        )
        .unwrap();

        replace_machine_tags(f.db.conn(), f.doc, &[]).unwrap();
        let names: Vec<String> = of_document(f.db.conn(), f.doc)
            .unwrap()
            .iter()
            .map(|(t, _)| t.canonical_name.to_string())
            .collect();
        assert_eq!(names, vec!["keep"]);
    }

    #[test]
    fn re_reading_a_file_does_not_discard_ai_tags() {
        // TAG-004 and TAG-006 together: each writer clears only its own rows.
        let f = fixture();
        let inline = tag(&f, "inline-old");
        let fresh_inline = tag(&f, "inline-new");
        let from_model = tag(&f, "from-model");
        assign(
            f.db.conn(),
            &DocumentTag::derived(f.doc, inline.id, TagSource::Inline),
        )
        .unwrap();
        assign(
            f.db.conn(),
            &DocumentTag::ai_generated(f.doc, from_model.id, "m", Confidence::new(0.8)),
        )
        .unwrap();

        replace_file_tags(
            f.db.conn(),
            f.doc,
            &[DocumentTag::derived(
                f.doc,
                fresh_inline.id,
                TagSource::Inline,
            )],
        )
        .unwrap();

        let mut names: Vec<String> = of_document(f.db.conn(), f.doc)
            .unwrap()
            .iter()
            .map(|(t, _)| t.canonical_name.to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["from-model", "inline-new"]);
    }

    #[test]
    fn reclassifying_does_not_discard_inline_tags() {
        // The mirror image of the previous test.
        let f = fixture();
        let inline = tag(&f, "inline");
        let old_ai = tag(&f, "old-ai");
        let new_ai = tag(&f, "new-ai");
        assign(
            f.db.conn(),
            &DocumentTag::derived(f.doc, inline.id, TagSource::Inline),
        )
        .unwrap();
        assign(
            f.db.conn(),
            &DocumentTag::ai_generated(f.doc, old_ai.id, "m", Confidence::new(0.5)),
        )
        .unwrap();

        replace_machine_tags(
            f.db.conn(),
            f.doc,
            &[DocumentTag::ai_generated(
                f.doc,
                new_ai.id,
                "m",
                Confidence::new(0.9),
            )],
        )
        .unwrap();

        let mut names: Vec<String> = of_document(f.db.conn(), f.doc)
            .unwrap()
            .iter()
            .map(|(t, _)| t.canonical_name.to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["inline", "new-ai"]);
    }

    #[test]
    fn unassigning_an_inline_tag_lets_it_come_back() {
        // TAG-004: deleting the `#tag` text is not a rejection.
        let f = fixture();
        let t = tag(&f, "inline");
        assign(
            f.db.conn(),
            &DocumentTag::derived(f.doc, t.id, TagSource::Inline),
        )
        .unwrap();
        assert!(unassign(f.db.conn(), f.doc, t.id).unwrap());
        assert!(rejections(f.db.conn(), f.doc).unwrap().is_empty());
        assert_eq!(
            assign(
                f.db.conn(),
                &DocumentTag::derived(f.doc, t.id, TagSource::Inline)
            )
            .unwrap(),
            Assigned::Inserted
        );
    }

    #[test]
    fn boolean_tag_queries_combine_and_or_and_not() {
        // TAG-003.
        let f = fixture();
        let history = tag(&f, "history");
        let lighthouse = tag(&f, "lighthouse");
        let draft = tag(&f, "draft");

        let both = document(f.vault, "both.md");
        let only_history = document(f.vault, "history.md");
        let drafted = document(f.vault, "drafted.md");
        for d in [&both, &only_history, &drafted] {
            documents::upsert(f.db.conn(), d).unwrap();
        }
        for (doc, tags) in [
            (both.id, vec![history.id, lighthouse.id]),
            (only_history.id, vec![history.id]),
            (drafted.id, vec![history.id, draft.id]),
        ] {
            for t in tags {
                assign(f.db.conn(), &DocumentTag::manual(doc, t)).unwrap();
            }
        }

        let and = documents_matching(
            f.db.conn(),
            &[f.vault],
            &TagQuery {
                all: vec![history.id, lighthouse.id],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(and, vec![both.id]);

        let or = documents_matching(
            f.db.conn(),
            &[f.vault],
            &TagQuery {
                any: vec![lighthouse.id, draft.id],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(or.len(), 2);

        let not = documents_matching(
            f.db.conn(),
            &[f.vault],
            &TagQuery {
                all: vec![history.id],
                none: vec![draft.id],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(not.len(), 2);
        assert!(!not.contains(&drafted.id));
    }

    #[test]
    fn hierarchical_tags_match_their_descendants_when_asked() {
        // TAG-010.
        let f = fixture();
        let parent = tag(&f, "project");
        let child = tag(&f, "project/marius");
        let unrelated = tag(&f, "projection");

        let on_child = document(f.vault, "child.md");
        let on_unrelated = document(f.vault, "unrelated.md");
        documents::upsert(f.db.conn(), &on_child).unwrap();
        documents::upsert(f.db.conn(), &on_unrelated).unwrap();
        assign(f.db.conn(), &DocumentTag::manual(on_child.id, child.id)).unwrap();
        assign(
            f.db.conn(),
            &DocumentTag::manual(on_unrelated.id, unrelated.id),
        )
        .unwrap();

        assert!(documents_with(f.db.conn(), parent.id, false)
            .unwrap()
            .is_empty());
        let descendants = documents_with(f.db.conn(), parent.id, true).unwrap();
        assert_eq!(descendants, vec![on_child.id]);
    }

    #[test]
    fn renaming_a_tag_keeps_every_assignment() {
        // TAG-010.
        let f = fixture();
        let t = tag(&f, "old-name");
        assign(f.db.conn(), &DocumentTag::manual(f.doc, t.id)).unwrap();

        rename(
            f.db.conn(),
            t.id,
            &TagName::new("new-name").unwrap(),
            "New Name",
        )
        .unwrap();

        let (stored, _) = of_document(f.db.conn(), f.doc).unwrap().remove(0);
        assert_eq!(stored.canonical_name.as_str(), "new-name");
        assert_eq!(stored.display_name, "New Name");
        assert_eq!(stored.id, t.id, "the identity is stable across a rename");
    }

    #[test]
    fn merging_tags_moves_assignments_and_keeps_the_old_name_as_an_alias() {
        let f = fixture();
        let from = tag(&f, "ml");
        let into = tag(&f, "machine-learning");
        assign(f.db.conn(), &DocumentTag::manual(f.doc, from.id)).unwrap();

        assert_eq!(merge(f.db.conn(), from.id, into.id).unwrap(), 1);
        let (stored, _) = of_document(f.db.conn(), f.doc).unwrap().remove(0);
        assert_eq!(stored.id, into.id);
        assert!(load(f.db.conn(), from.id).unwrap().is_none());

        // The old spelling still resolves, so existing notes and queries work.
        let resolved = ensure(f.db.conn(), Some(f.vault), &TagName::new("ml").unwrap()).unwrap();
        assert_eq!(resolved.id, into.id);
    }

    #[test]
    fn merging_does_not_duplicate_an_assignment_the_target_already_has() {
        let f = fixture();
        let from = tag(&f, "ml");
        let into = tag(&f, "machine-learning");
        assign(f.db.conn(), &DocumentTag::manual(f.doc, from.id)).unwrap();
        assign(f.db.conn(), &DocumentTag::manual(f.doc, into.id)).unwrap();

        merge(f.db.conn(), from.id, into.id).unwrap();
        assert_eq!(of_document(f.db.conn(), f.doc).unwrap().len(), 1);
    }

    #[test]
    fn tags_can_be_fetched_for_many_documents_in_one_query() {
        let f = fixture();
        let history = tag(&f, "history");
        let lighthouse = tag(&f, "lighthouse");
        let second = document(f.vault, "b.md");
        documents::upsert(f.db.conn(), &second).unwrap();

        assign(f.db.conn(), &DocumentTag::manual(f.doc, history.id)).unwrap();
        assign(f.db.conn(), &DocumentTag::manual(f.doc, lighthouse.id)).unwrap();
        assign(f.db.conn(), &DocumentTag::manual(second.id, history.id)).unwrap();

        let untagged = document(f.vault, "c.md");
        documents::upsert(f.db.conn(), &untagged).unwrap();

        let map = names_for_documents(f.db.conn(), &[f.doc, second.id, untagged.id]).unwrap();
        assert_eq!(
            map[&f.doc]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["history", "lighthouse"]
        );
        assert_eq!(map[&second.id].len(), 1);
        // A document with no tags is simply absent rather than erroring.
        assert!(!map.contains_key(&untagged.id));
        assert!(names_for_documents(f.db.conn(), &[]).unwrap().is_empty());
    }

    #[test]
    fn tag_counts_include_unused_tags() {
        let f = fixture();
        let used = tag(&f, "used");
        tag(&f, "unused");
        assign(f.db.conn(), &DocumentTag::manual(f.doc, used.id)).unwrap();

        let counts = counts(f.db.conn(), Some(f.vault)).unwrap();
        assert_eq!(counts.len(), 2);
        let map: std::collections::BTreeMap<_, _> = counts
            .iter()
            .map(|(t, n)| (t.canonical_name.to_string(), *n))
            .collect();
        assert_eq!(map["used"], 1);
        assert_eq!(map["unused"], 0);
    }

    #[test]
    fn counts_everywhere_spans_vaults_while_counts_scopes_to_one() {
        let f = fixture();
        let other = Vault::new_local("Other", "/other");
        vaults::upsert(f.db.conn(), &other, &VaultPolicy::conservative(other.id)).unwrap();

        let here = tag(&f, "here");
        let there = ensure(f.db.conn(), Some(other.id), &TagName::new("there").unwrap()).unwrap();
        assign(f.db.conn(), &DocumentTag::manual(f.doc, here.id)).unwrap();

        let scoped: Vec<String> = counts(f.db.conn(), Some(f.vault))
            .unwrap()
            .iter()
            .map(|(t, _)| t.canonical_name.to_string())
            .collect();
        assert_eq!(scoped, vec!["here"]);

        let everywhere: Vec<String> = counts_everywhere(f.db.conn())
            .unwrap()
            .iter()
            .map(|(t, _)| t.canonical_name.to_string())
            .collect();
        assert_eq!(everywhere, vec!["here", "there"]);
        assert!(load(f.db.conn(), there.id).unwrap().is_some());
    }

    #[test]
    fn global_tags_are_visible_from_every_vault() {
        let f = fixture();
        let global = ensure(f.db.conn(), None, &TagName::new("shared").unwrap()).unwrap();
        assert_eq!(global.vault, None);
        let visible = list(f.db.conn(), Some(f.vault)).unwrap();
        assert!(visible.iter().any(|t| t.id == global.id));
    }

    #[test]
    fn deleting_a_document_clears_its_assignments() {
        let f = fixture();
        let t = tag(&f, "history");
        assign(f.db.conn(), &DocumentTag::manual(f.doc, t.id)).unwrap();
        documents::delete(f.db.conn(), f.doc).unwrap();
        assert!(of_document(f.db.conn(), f.doc).unwrap().is_empty());
        // The tag itself survives; it is shared.
        assert!(load(f.db.conn(), t.id).unwrap().is_some());
    }
}
