//! Document records, naming and link resolution.
//!
//! Requirements: KB-003, KB-005, KB-006, KB-007, KB-010, KB-012, KB-013,
//! MD-006, MD-007, IM-003, SRCH-002, AC-003, integrity rules 1 and 2.
//!
//! The rule that shapes this module: **a document's identity is never derived
//! from where it lives.** `path` is an ordinary mutable column; `id` is the
//! primary key everything else references. Renaming or moving a file is
//! therefore an `UPDATE` of one column, and every tag, relationship, revision
//! and embedding keeps pointing at the same row (AC-003, integrity rule 1).

use marius_model::{
    ChangeKind, ContentHash, Document, DocumentId, DocumentKind, EntityKind, Sensitivity, VaultId,
    VaultPath,
};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use time::OffsetDateTime;

use crate::convert::{
    bool_to_sql, hash_from_row, hash_to_sql, id_from_row, json_from_row, json_to_sql,
    opt_time_from_row, parsed_from_row, path_from_row, time_from_row, time_to_sql, IdSql,
};
use crate::{changelog, Result, StorageError};

const COLUMNS: &str = "id, vault_id, path, title, kind, authorship, content_hash, size_bytes, \
                       created_at, modified_at, indexed_at, archived_at, sensitivity, \
                       frontmatter, external";

/// Fold a name for link resolution.
///
/// `[[alexandria]]`, `[[Alexandria]]` and `[[  Alexandria  ]]` must all find
/// the note called `Alexandria.md`. Punctuation is *kept* — `C# notes` and
/// `C notes` are different documents — but case and whitespace runs are not
/// distinctions anyone intends when typing a link.
pub fn fold_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_space = false;
    for ch in name.trim().chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// The names a document can be linked by: its title and its filename stem.
fn name_keys(document: &Document) -> Vec<String> {
    let mut keys = vec![fold_name(document.path.file_stem())];
    let title = fold_name(&document.title);
    if !title.is_empty() && !keys.contains(&title) {
        keys.push(title);
    }
    keys.retain(|k| !k.is_empty());
    keys
}

/// Insert or update a document, keeping its identity and bumping its revision.
///
/// Returns the new revision counter.
pub fn upsert(conn: &Connection, document: &Document) -> Result<u64> {
    let revision: i64 = conn.query_row(
        "INSERT INTO document
            (id, vault_id, path, title, kind, authorship, content_hash, size_bytes,
             created_at, modified_at, indexed_at, archived_at, sensitivity, frontmatter,
             external, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 1)
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
            revision = document.revision + 1
         RETURNING revision",
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
        ],
        |row| row.get(0),
    )?;

    refresh_names(conn, document)?;
    changelog::record(
        conn,
        document.vault,
        EntityKind::Document,
        &document.id.to_string(),
        ChangeKind::Upsert,
        revision as u64,
    )?;
    Ok(revision as u64)
}

/// [`upsert`] for an *apply-replication* index pass: the row's stored
/// revision is kept (the replica writer already set the remote value) and
/// nothing is journaled — re-journaling what another device told us would
/// echo the operation straight back (SYNC-010).
pub fn upsert_unjournaled(conn: &Connection, document: &Document) -> Result<()> {
    conn.execute(
        "INSERT INTO document
            (id, vault_id, path, title, kind, authorship, content_hash, size_bytes,
             created_at, modified_at, indexed_at, archived_at, sensitivity, frontmatter,
             external, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 1)
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
            external = excluded.external",
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
        ],
    )?;
    refresh_names(conn, document)?;
    Ok(())
}

/// Rewrite the names this document can be linked by.
pub(crate) fn refresh_names(conn: &Connection, document: &Document) -> Result<()> {
    conn.execute(
        "DELETE FROM document_name WHERE document_id = ?1",
        params![document.id.to_sql_blob()],
    )?;
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO document_name (document_id, vault_id, name_key)
         VALUES (?1, ?2, ?3)",
    )?;
    for key in name_keys(document) {
        stmt.execute(params![
            document.id.to_sql_blob(),
            document.vault.to_sql_blob(),
            key
        ])?;
    }
    Ok(())
}

/// Replace a document's aliases (KB-010).
pub fn set_aliases(conn: &Connection, document: DocumentId, aliases: &[String]) -> Result<()> {
    let vault: Vec<u8> = conn.query_row(
        "SELECT vault_id FROM document WHERE id = ?1",
        params![document.to_sql_blob()],
        |row| row.get(0),
    )?;
    conn.execute(
        "DELETE FROM document_alias WHERE document_id = ?1",
        params![document.to_sql_blob()],
    )?;
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO document_alias (document_id, vault_id, alias, alias_key)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for alias in aliases {
        let key = fold_name(alias);
        if key.is_empty() {
            continue;
        }
        stmt.execute(params![document.to_sql_blob(), vault, alias, key])?;
    }
    Ok(())
}

/// A document's aliases.
pub fn aliases(conn: &Connection, document: DocumentId) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT alias FROM document_alias WHERE document_id = ?1 ORDER BY alias")?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Fetch one document.
pub fn get(conn: &Connection, id: DocumentId) -> Result<Option<Document>> {
    let sql = format!("SELECT {COLUMNS} FROM document WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![id.to_sql_blob()], row_to_document)
        .optional()?)
}

/// Fetch by vault-relative path.
pub fn by_path(conn: &Connection, vault: VaultId, path: &VaultPath) -> Result<Option<Document>> {
    let sql = format!("SELECT {COLUMNS} FROM document WHERE vault_id = ?1 AND path = ?2");
    Ok(conn
        .query_row(
            &sql,
            params![vault.to_sql_blob(), path.as_str()],
            row_to_document,
        )
        .optional()?)
}

/// Every document with this exact content, across all vaults.
///
/// Used both for duplicate detection on import (IM-003) and to recognise a
/// file that was moved out of band as the document it already is (KB-005).
pub fn by_hash(conn: &Connection, hash: &ContentHash) -> Result<Vec<Document>> {
    let sql = format!("SELECT {COLUMNS} FROM document WHERE content_hash = ?1 ORDER BY path");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![hash_to_sql(hash)], row_to_document)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Filters for listing documents (SRCH-002).
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Restrict to these vaults. Empty means every vault (KB-009, SRCH-004).
    pub vaults: Vec<VaultId>,
    /// Restrict to documents under this folder.
    pub folder: Option<VaultPath>,
    pub kinds: Vec<DocumentKind>,
    /// Archived documents are included unless this is set (KB-012).
    pub exclude_archived: bool,
    /// Exclude AI-generated notes (AI-012).
    pub exclude_generated: bool,
    pub modified_after: Option<OffsetDateTime>,
    pub modified_before: Option<OffsetDateTime>,
    pub limit: Option<usize>,
}

/// List documents matching `filter`, most recently modified first.
pub fn list(conn: &Connection, filter: &Filter) -> Result<Vec<Document>> {
    let mut sql = format!("SELECT {COLUMNS} FROM document WHERE 1 = 1");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();

    if !filter.vaults.is_empty() {
        let holes = vec!["?"; filter.vaults.len()].join(", ");
        sql.push_str(&format!(" AND vault_id IN ({holes})"));
        args.extend(filter.vaults.iter().map(|v| v.to_sql_blob().into()));
    }
    if let Some(folder) = &filter.folder {
        // `LIKE 'notes/%'` would also match a folder called `notes-archive`
        // on some collations; anchoring on the separator cannot.
        sql.push_str(" AND (path = ? OR path LIKE ? ESCAPE '\\')");
        args.push(folder.as_str().to_owned().into());
        args.push(format!("{}/%", escape_like(folder.as_str())).into());
    }
    if !filter.kinds.is_empty() {
        let holes = vec!["?"; filter.kinds.len()].join(", ");
        sql.push_str(&format!(" AND kind IN ({holes})"));
        args.extend(filter.kinds.iter().map(|k| k.as_str().to_owned().into()));
    }
    if filter.exclude_archived {
        sql.push_str(" AND archived_at IS NULL");
    }
    if filter.exclude_generated {
        sql.push_str(" AND authorship <> 'derived'");
    }
    if let Some(after) = filter.modified_after {
        sql.push_str(" AND modified_at > ?");
        args.push(time_to_sql(after).into());
    }
    if let Some(before) = filter.modified_before {
        sql.push_str(" AND modified_at < ?");
        args.push(time_to_sql(before).into());
    }
    sql.push_str(" ORDER BY modified_at DESC, path ASC");
    if let Some(limit) = filter.limit {
        sql.push_str(" LIMIT ?");
        args.push((limit as i64).into());
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args), row_to_document)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Every indexed document in a vault, for reconciling against disk (KB-005).
///
/// One query rather than a path list followed by a lookup per path: a 10,000
/// document vault made 10,001 round trips through the old shape.
pub fn all_in_vault(conn: &Connection, vault: VaultId) -> Result<Vec<Document>> {
    let sql = format!("SELECT {COLUMNS} FROM document WHERE vault_id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![vault.to_sql_blob()], row_to_document)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every path currently indexed for a vault, for reconciling with disk
/// (KB-005).
pub fn paths(
    conn: &Connection,
    vault: VaultId,
) -> Result<Vec<(DocumentId, VaultPath, ContentHash)>> {
    let mut stmt =
        conn.prepare("SELECT id, path, content_hash FROM document WHERE vault_id = ?1")?;
    let rows = stmt
        .query_map(params![vault.to_sql_blob()], |row| {
            Ok((
                id_from_row(row, 0)?,
                path_from_row(row, 1)?,
                hash_from_row(row, 2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Move or rename a document, preserving its identity (AC-003).
pub fn relocate(conn: &Connection, id: DocumentId, new_path: &VaultPath) -> Result<u64> {
    let revision: Option<i64> = conn
        .query_row(
            "UPDATE document SET path = ?2, revision = revision + 1 WHERE id = ?1
             RETURNING revision",
            params![id.to_sql_blob(), new_path.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(revision) = revision else {
        return Err(StorageError::NotFound {
            entity: "document",
            id: id.to_string(),
        });
    };
    // The filename is one of the names the document is linked by, so the
    // lookup keys move with it.
    if let Some(document) = get(conn, id)? {
        refresh_names(conn, &document)?;
        changelog::record(
            conn,
            document.vault,
            EntityKind::Document,
            &id.to_string(),
            ChangeKind::Upsert,
            revision as u64,
        )?;
    }
    Ok(revision as u64)
}

/// Archive or unarchive (KB-012).
pub fn set_archived(conn: &Connection, id: DocumentId, archived: bool) -> Result<()> {
    let at = archived.then(|| time_to_sql(OffsetDateTime::now_utc()));
    update_one(conn, id, "archived_at", at)
}

/// Set or clear a document-level sensitivity override (SEC-005).
///
/// AI-007 forbids automatically lowering a restriction, which is enforced in
/// `VaultPolicy::effective`; this function records the user's explicit choice.
/// Set a document's authorship — the generation pipeline marking a new
/// note `Derived`, or the user promoting one to `Promoted` (AI-014).
pub fn set_authorship(
    conn: &Connection,
    id: DocumentId,
    authorship: marius_model::Authorship,
) -> Result<()> {
    update_one(conn, id, "authorship", Some(authorship.as_str().to_owned()))
}

pub fn set_sensitivity(
    conn: &Connection,
    id: DocumentId,
    sensitivity: Option<Sensitivity>,
) -> Result<()> {
    update_one(
        conn,
        id,
        "sensitivity",
        sensitivity.map(|s| s.as_str().to_owned()),
    )
}

fn update_one(
    conn: &Connection,
    id: DocumentId,
    column: &str,
    value: Option<String>,
) -> Result<()> {
    let sql = format!(
        "UPDATE document SET {column} = ?2, revision = revision + 1 WHERE id = ?1
         RETURNING revision, vault_id"
    );
    let row: Option<(i64, Vec<u8>)> = conn
        .query_row(&sql, params![id.to_sql_blob(), value], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    let Some((revision, vault)) = row else {
        return Err(StorageError::NotFound {
            entity: "document",
            id: id.to_string(),
        });
    };
    changelog::record(
        conn,
        VaultId::from_sql_blob(&vault)?,
        EntityKind::Document,
        &id.to_string(),
        ChangeKind::Upsert,
        revision as u64,
    )?;
    Ok(())
}

/// Remove a document's record. The file itself is the caller's business.
pub fn delete(conn: &Connection, id: DocumentId) -> Result<()> {
    let vault: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vault_id FROM document WHERE id = ?1",
            params![id.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(vault) = vault else {
        return Err(StorageError::NotFound {
            entity: "document",
            id: id.to_string(),
        });
    };
    conn.execute(
        "DELETE FROM document WHERE id = ?1",
        params![id.to_sql_blob()],
    )?;
    changelog::record(
        conn,
        VaultId::from_sql_blob(&vault)?,
        EntityKind::Document,
        &id.to_string(),
        ChangeKind::Delete,
        0,
    )?;
    Ok(())
}

/// How a link target was matched, so the UI can show why a link resolved the
/// way it did and flag ambiguity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMatch {
    /// The target named a path exactly.
    Path,
    /// The target matched a filename or title.
    Name,
    /// The target matched a declared alias (KB-010).
    Alias,
    /// Several documents matched; the first by path was taken.
    Ambiguous,
}

/// Resolve a `[[wiki link]]` or relative Markdown destination to a document.
///
/// Resolution order is path, then name, then alias — the order of
/// specificity — and is scoped to `vault` unless `also` names further vaults
/// to fall back to, which is how a cross-vault link resolves (GRAPH-008).
pub fn resolve_link(
    conn: &Connection,
    vault: VaultId,
    target: &str,
    also: &[VaultId],
) -> Result<Option<(DocumentId, LinkMatch)>> {
    Ok(resolve_link_global(conn, vault, target, also)?.map(|(global, how)| (global.document, how)))
}

/// Resolve a link to a cross-vault reference.
///
/// The vault half matters: an edge into another vault has to record which
/// vault, so it keeps meaning something when that vault is detached
/// (GRAPH-008, SYNC-012).
pub fn resolve_link_global(
    conn: &Connection,
    vault: VaultId,
    target: &str,
    also: &[VaultId],
) -> Result<Option<(marius_model::GlobalDocumentId, LinkMatch)>> {
    let target = target.trim().trim_start_matches("./");
    if target.is_empty() {
        return Ok(None);
    }
    let mut scopes = vec![vault];
    scopes.extend(also.iter().copied().filter(|v| *v != vault));

    for scope in &scopes {
        if let Some((document, how)) = resolve_in_vault(conn, *scope, target)? {
            return Ok(Some((
                marius_model::GlobalDocumentId::new(*scope, document),
                how,
            )));
        }
    }
    Ok(None)
}

fn resolve_in_vault(
    conn: &Connection,
    vault: VaultId,
    target: &str,
) -> Result<Option<(DocumentId, LinkMatch)>> {
    // 1. An exact path, with or without the extension the user omitted.
    let mut candidates = vec![target.to_owned()];
    if !target.contains('.') {
        candidates.push(format!("{target}.md"));
    }
    for candidate in &candidates {
        if let Ok(path) = VaultPath::new(candidate) {
            if let Some(id) = conn
                .query_row(
                    "SELECT id FROM document WHERE vault_id = ?1 AND path = ?2",
                    params![vault.to_sql_blob(), path.as_str()],
                    |row| id_from_row(row, 0),
                )
                .optional()?
            {
                return Ok(Some((id, LinkMatch::Path)));
            }
        }
    }

    // 2. A filename or title. The last path segment is what people type.
    let leaf = target.rsplit('/').next().unwrap_or(target);
    let key = fold_name(strip_markdown_extension(leaf));
    if !key.is_empty() {
        let mut stmt = conn.prepare_cached(
            "SELECT d.id FROM document_name n
             JOIN document d ON d.id = n.document_id
             WHERE n.vault_id = ?1 AND n.name_key = ?2
             ORDER BY d.path",
        )?;
        let hits: Vec<DocumentId> = stmt
            .query_map(params![vault.to_sql_blob(), &key], |row| {
                id_from_row(row, 0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        match hits.len() {
            0 => {}
            1 => return Ok(Some((hits[0], LinkMatch::Name))),
            // Two notes with the same name in different folders: pick
            // deterministically and tell the caller it was a guess.
            _ => return Ok(Some((hits[0], LinkMatch::Ambiguous))),
        }

        // 3. A declared alias.
        if let Some(id) = conn
            .query_row(
                "SELECT a.document_id FROM document_alias a
                 JOIN document d ON d.id = a.document_id
                 WHERE a.vault_id = ?1 AND a.alias_key = ?2
                 ORDER BY d.path LIMIT 1",
                params![vault.to_sql_blob(), key],
                |row| id_from_row(row, 0),
            )
            .optional()?
        {
            return Ok(Some((id, LinkMatch::Alias)));
        }
    }
    Ok(None)
}

fn strip_markdown_extension(name: &str) -> &str {
    for ext in [".md", ".markdown", ".mdown", ".mkd"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem;
        }
    }
    name
}

/// Resolve a link that may be written relative to the linking document.
///
/// A Markdown link inside `notes/a.md` is normally written `[b](b.md)`, meaning
/// `notes/b.md`. A `[[wiki link]]` is normally vault-wide. Both spellings occur
/// in real vaults — and in imported Obsidian vaults in particular — so
/// resolution tries the target as written first, then relative to the linking
/// document's folder.
pub fn resolve_relative(
    conn: &Connection,
    vault: VaultId,
    base: Option<&VaultPath>,
    target: &str,
    also: &[VaultId],
) -> Result<Option<(marius_model::GlobalDocumentId, LinkMatch)>> {
    if let Some(hit) = resolve_link_global(conn, vault, target, also)? {
        return Ok(Some(hit));
    }
    // An absolute-looking target was already tried as written.
    let trimmed = target.trim();
    if trimmed.starts_with('/') {
        return Ok(None);
    }
    let Some(folder) = base.and_then(VaultPath::parent) else {
        return Ok(None);
    };
    let joined = format!("{}/{}", folder.as_str(), trimmed);
    // `VaultPath::new` resolves the `..` segments a relative link may contain.
    match VaultPath::new(&joined) {
        Ok(path) => resolve_link_global(conn, vault, path.as_str(), also),
        Err(_) => Ok(None),
    }
}

/// Number of documents in a vault.
pub fn count(conn: &Connection, vault: VaultId) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM document WHERE vault_id = ?1",
        params![vault.to_sql_blob()],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// The integer rowid the FTS index keys on.
pub fn seq_of(conn: &Connection, id: DocumentId) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT seq FROM document WHERE id = ?1",
            params![id.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?)
}

/// The document a rowid belongs to.
pub fn by_seq(conn: &Connection, seq: i64) -> Result<Option<Document>> {
    let sql = format!("SELECT {COLUMNS} FROM document WHERE seq = ?1");
    Ok(conn
        .query_row(&sql, params![seq], row_to_document)
        .optional()?)
}

fn row_to_document(row: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        id: id_from_row(row, 0)?,
        vault: id_from_row(row, 1)?,
        path: path_from_row(row, 2)?,
        title: row.get(3)?,
        kind: parsed_from_row(row, 4)?,
        authorship: parsed_from_row(row, 5)?,
        content_hash: hash_from_row(row, 6)?,
        size_bytes: row.get::<_, i64>(7)? as u64,
        created_at: time_from_row(row, 8)?,
        modified_at: time_from_row(row, 9)?,
        indexed_at: time_from_row(row, 10)?,
        archived_at: opt_time_from_row(row, 11)?,
        sensitivity: row
            .get::<_, Option<String>>(12)?
            .map(|s| s.parse::<Sensitivity>())
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
        frontmatter: json_from_row(row, 13)?,
        external: row.get::<_, i64>(14)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{vaults, Database};
    use marius_model::{Authorship, MetadataValue, Vault, VaultPolicy};
    use std::collections::BTreeMap;

    struct Fixture {
        db: Database,
        vault: VaultId,
    }

    fn fixture() -> Fixture {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::new_local("Research", "/vaults/research");
        vaults::upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        Fixture {
            db,
            vault: vault.id,
        }
    }

    fn doc(vault: VaultId, path: &str, title: &str) -> Document {
        let now = OffsetDateTime::now_utc();
        Document {
            id: DocumentId::new(),
            vault,
            path: VaultPath::new(path).unwrap(),
            title: title.to_owned(),
            kind: DocumentKind::Markdown,
            authorship: Authorship::Source,
            content_hash: ContentHash::of(path.as_bytes()),
            size_bytes: 42,
            created_at: now,
            modified_at: now,
            indexed_at: now,
            archived_at: None,
            sensitivity: None,
            frontmatter: BTreeMap::new(),
            external: false,
        }
    }

    #[test]
    fn a_document_round_trips_with_every_field() {
        let f = fixture();
        let mut d = doc(f.vault, "notes/alexandria.md", "Alexandria");
        d.frontmatter
            .insert("cssclass".into(), MetadataValue::Text("wide".into()));
        d.sensitivity = Some(Sensitivity::Confidential);
        d.external = true;

        upsert(f.db.conn(), &d).unwrap();
        let got = get(f.db.conn(), d.id).unwrap().unwrap();
        assert_eq!(got, d);
    }

    #[test]
    fn upserting_bumps_the_revision_and_journals_each_change() {
        let f = fixture();
        let mut d = doc(f.vault, "a.md", "A");
        assert_eq!(upsert(f.db.conn(), &d).unwrap(), 1);
        d.title = "A2".into();
        assert_eq!(upsert(f.db.conn(), &d).unwrap(), 2);

        let history =
            changelog::history(f.db.conn(), EntityKind::Document, &d.id.to_string()).unwrap();
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn two_documents_cannot_occupy_one_path() {
        let f = fixture();
        upsert(f.db.conn(), &doc(f.vault, "a.md", "A")).unwrap();
        let err = upsert(f.db.conn(), &doc(f.vault, "a.md", "Other")).unwrap_err();
        assert!(err.is_conflict(), "{err:?}");
    }

    #[test]
    fn the_same_path_in_two_vaults_is_fine() {
        // KB-008.
        let f = fixture();
        let other = Vault::new_local("Other", "/vaults/other");
        vaults::upsert(f.db.conn(), &other, &VaultPolicy::conservative(other.id)).unwrap();
        upsert(f.db.conn(), &doc(f.vault, "a.md", "A")).unwrap();
        upsert(f.db.conn(), &doc(other.id, "a.md", "A")).unwrap();
        assert_eq!(count(f.db.conn(), f.vault).unwrap(), 1);
        assert_eq!(count(f.db.conn(), other.id).unwrap(), 1);
    }

    #[test]
    fn renaming_preserves_identity_and_every_reference_to_it() {
        // AC-003, integrity rule 1 — the headline guarantee.
        let f = fixture();
        let d = doc(f.vault, "notes/old-name.md", "Old Name");
        upsert(f.db.conn(), &d).unwrap();

        let new_path = VaultPath::new("archive/new-name.md").unwrap();
        relocate(f.db.conn(), d.id, &new_path).unwrap();

        let moved = get(f.db.conn(), d.id).unwrap().unwrap();
        assert_eq!(moved.id, d.id, "the identity must not change");
        assert_eq!(moved.path, new_path);
        assert!(by_path(f.db.conn(), f.vault, &d.path).unwrap().is_none());

        // And the new filename is immediately linkable.
        let (found, how) = resolve_link(f.db.conn(), f.vault, "new-name", &[])
            .unwrap()
            .unwrap();
        assert_eq!(found, d.id);
        assert_eq!(how, LinkMatch::Name);
    }

    #[test]
    fn duplicate_content_is_detectable_across_vaults() {
        // IM-003.
        let f = fixture();
        let other = Vault::new_local("Other", "/vaults/other");
        vaults::upsert(f.db.conn(), &other, &VaultPolicy::conservative(other.id)).unwrap();

        let hash = ContentHash::of(b"identical body");
        let mut a = doc(f.vault, "a.md", "A");
        a.content_hash = hash;
        let mut b = doc(other.id, "b.md", "B");
        b.content_hash = hash;
        upsert(f.db.conn(), &a).unwrap();
        upsert(f.db.conn(), &b).unwrap();

        let duplicates = by_hash(f.db.conn(), &hash).unwrap();
        assert_eq!(duplicates.len(), 2);
    }

    #[test]
    fn links_resolve_by_path_name_and_alias_in_that_order() {
        // MD-006.
        let f = fixture();
        let d = doc(f.vault, "research/Alexandria.md", "The Pharos");
        upsert(f.db.conn(), &d).unwrap();
        set_aliases(
            f.db.conn(),
            d.id,
            &["Pharos".into(), "Wonder of the World".into()],
        )
        .unwrap();

        for (target, expected) in [
            ("research/Alexandria.md", LinkMatch::Path),
            ("research/Alexandria", LinkMatch::Path),
            ("./research/Alexandria.md", LinkMatch::Path),
            ("Alexandria", LinkMatch::Name),
            ("alexandria", LinkMatch::Name),
            ("  ALEXANDRIA  ", LinkMatch::Name),
            ("The Pharos", LinkMatch::Name),
            ("Wonder of the World", LinkMatch::Alias),
        ] {
            let got = resolve_link(f.db.conn(), f.vault, target, &[])
                .unwrap()
                .unwrap_or_else(|| panic!("{target:?} did not resolve"));
            assert_eq!(got.0, d.id, "{target:?}");
            assert_eq!(got.1, expected, "{target:?}");
        }

        assert!(resolve_link(f.db.conn(), f.vault, "Nonexistent", &[])
            .unwrap()
            .is_none());
        assert!(resolve_link(f.db.conn(), f.vault, "  ", &[])
            .unwrap()
            .is_none());
    }

    #[test]
    fn an_ambiguous_name_resolves_deterministically_and_says_so() {
        let f = fixture();
        upsert(f.db.conn(), &doc(f.vault, "b/Note.md", "Note")).unwrap();
        upsert(f.db.conn(), &doc(f.vault, "a/Note.md", "Note")).unwrap();

        let first = resolve_link(f.db.conn(), f.vault, "Note", &[])
            .unwrap()
            .unwrap();
        let again = resolve_link(f.db.conn(), f.vault, "Note", &[])
            .unwrap()
            .unwrap();
        assert_eq!(first.1, LinkMatch::Ambiguous);
        assert_eq!(first.0, again.0, "resolution must be stable");
    }

    #[test]
    fn link_resolution_is_scoped_to_the_vault_unless_told_otherwise() {
        // GRAPH-008 / AC-009.
        let f = fixture();
        let other = Vault::new_local("Other", "/vaults/other");
        vaults::upsert(f.db.conn(), &other, &VaultPolicy::conservative(other.id)).unwrap();
        let far = doc(other.id, "Far.md", "Far");
        upsert(f.db.conn(), &far).unwrap();

        assert!(resolve_link(f.db.conn(), f.vault, "Far", &[])
            .unwrap()
            .is_none());
        let (found, _) = resolve_link(f.db.conn(), f.vault, "Far", &[other.id])
            .unwrap()
            .unwrap();
        assert_eq!(found, far.id);
    }

    #[test]
    fn the_local_vault_wins_over_a_fallback_vault() {
        let f = fixture();
        let other = Vault::new_local("Other", "/vaults/other");
        vaults::upsert(f.db.conn(), &other, &VaultPolicy::conservative(other.id)).unwrap();
        let near = doc(f.vault, "Shared.md", "Shared");
        upsert(f.db.conn(), &near).unwrap();
        upsert(f.db.conn(), &doc(other.id, "Shared.md", "Shared")).unwrap();

        let (found, _) = resolve_link(f.db.conn(), f.vault, "Shared", &[other.id])
            .unwrap()
            .unwrap();
        assert_eq!(found, near.id);
    }

    #[test]
    fn a_relative_markdown_destination_resolves_against_its_own_folder() {
        // `[b](b.md)` inside `notes/a.md` means `notes/b.md`.
        let f = fixture();
        let source = doc(f.vault, "notes/a.md", "A");
        let target = doc(f.vault, "notes/b.md", "B");
        upsert(f.db.conn(), &source).unwrap();
        upsert(f.db.conn(), &target).unwrap();

        // Without a base there is nothing called `b.md` at the vault root, but
        // the name still matches, so ask for something that only works
        // relatively.
        let deeper = doc(f.vault, "notes/sub/c.md", "C");
        upsert(f.db.conn(), &deeper).unwrap();
        let (found, _) =
            resolve_relative(f.db.conn(), f.vault, Some(&source.path), "sub/c.md", &[])
                .unwrap()
                .unwrap();
        assert_eq!(found.document, deeper.id);

        // And a parent traversal works too.
        let (found, _) = resolve_relative(f.db.conn(), f.vault, Some(&deeper.path), "../b.md", &[])
            .unwrap()
            .unwrap();
        assert_eq!(found.document, target.id);
    }

    #[test]
    fn a_relative_resolution_does_not_invent_matches() {
        let f = fixture();
        let source = doc(f.vault, "notes/a.md", "A");
        upsert(f.db.conn(), &source).unwrap();
        assert!(
            resolve_relative(f.db.conn(), f.vault, Some(&source.path), "nowhere.md", &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn filters_select_by_folder_kind_archive_and_time() {
        // SRCH-002, KB-012, AI-012.
        let f = fixture();
        let mut note = doc(f.vault, "notes/a.md", "A");
        let mut archived = doc(f.vault, "notes/b.md", "B");
        archived.archived_at = Some(OffsetDateTime::now_utc());
        let mut generated = doc(f.vault, "notes/c.md", "C");
        generated.authorship = Authorship::Derived;
        generated.kind = DocumentKind::GeneratedNote;
        let elsewhere = doc(f.vault, "other/d.md", "D");
        note.modified_at = OffsetDateTime::now_utc();
        for d in [&note, &archived, &generated, &elsewhere] {
            upsert(f.db.conn(), d).unwrap();
        }

        let all = list(f.db.conn(), &Filter::default()).unwrap();
        assert_eq!(
            all.len(),
            4,
            "archived documents stay in results by default"
        );

        let in_folder = list(
            f.db.conn(),
            &Filter {
                folder: Some(VaultPath::new("notes").unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(in_folder.len(), 3);

        let visible = list(
            f.db.conn(),
            &Filter {
                exclude_archived: true,
                exclude_generated: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(visible.len(), 2);

        let markdown_only = list(
            f.db.conn(),
            &Filter {
                kinds: vec![DocumentKind::GeneratedNote],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(markdown_only.len(), 1);
        assert_eq!(markdown_only[0].title, "C");
    }

    #[test]
    fn a_folder_filter_does_not_match_a_similarly_named_sibling() {
        let f = fixture();
        upsert(f.db.conn(), &doc(f.vault, "notes/a.md", "A")).unwrap();
        upsert(f.db.conn(), &doc(f.vault, "notes-archive/b.md", "B")).unwrap();

        let hits = list(
            f.db.conn(),
            &Filter {
                folder: Some(VaultPath::new("notes").unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "A");
    }

    #[test]
    fn a_folder_with_sql_wildcards_in_its_name_is_matched_literally() {
        let f = fixture();
        upsert(f.db.conn(), &doc(f.vault, "100%_done/a.md", "A")).unwrap();
        upsert(f.db.conn(), &doc(f.vault, "100x_done/b.md", "B")).unwrap();

        let hits = list(
            f.db.conn(),
            &Filter {
                folder: Some(VaultPath::new("100%_done").unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert_eq!(hits[0].title, "A");
    }

    #[test]
    fn archiving_keeps_a_document_searchable_but_marks_it() {
        // KB-012.
        let f = fixture();
        let d = doc(f.vault, "a.md", "A");
        upsert(f.db.conn(), &d).unwrap();
        set_archived(f.db.conn(), d.id, true).unwrap();
        assert!(get(f.db.conn(), d.id).unwrap().unwrap().is_archived());
        set_archived(f.db.conn(), d.id, false).unwrap();
        assert!(!get(f.db.conn(), d.id).unwrap().unwrap().is_archived());
    }

    #[test]
    fn a_sensitivity_override_is_stored_and_clearable() {
        let f = fixture();
        let d = doc(f.vault, "a.md", "A");
        upsert(f.db.conn(), &d).unwrap();
        set_sensitivity(f.db.conn(), d.id, Some(Sensitivity::NoInference)).unwrap();
        assert_eq!(
            get(f.db.conn(), d.id).unwrap().unwrap().sensitivity,
            Some(Sensitivity::NoInference)
        );
        set_sensitivity(f.db.conn(), d.id, None).unwrap();
        assert_eq!(get(f.db.conn(), d.id).unwrap().unwrap().sensitivity, None);
    }

    #[test]
    fn deleting_a_document_journals_a_tombstone_and_cascades_its_names() {
        let f = fixture();
        let d = doc(f.vault, "a.md", "A");
        upsert(f.db.conn(), &d).unwrap();
        set_aliases(f.db.conn(), d.id, &["Alias".into()]).unwrap();

        delete(f.db.conn(), d.id).unwrap();
        assert!(get(f.db.conn(), d.id).unwrap().is_none());
        assert!(resolve_link(f.db.conn(), f.vault, "Alias", &[])
            .unwrap()
            .is_none());

        let history =
            changelog::history(f.db.conn(), EntityKind::Document, &d.id.to_string()).unwrap();
        assert_eq!(history.last().unwrap().change, ChangeKind::Delete);
    }

    #[test]
    fn removing_a_vault_removes_its_documents() {
        let f = fixture();
        upsert(f.db.conn(), &doc(f.vault, "a.md", "A")).unwrap();
        vaults::remove(f.db.conn(), f.vault).unwrap();
        assert_eq!(count(f.db.conn(), f.vault).unwrap(), 0);
    }

    #[test]
    fn paths_lists_what_the_scanner_needs_to_reconcile_with_disk() {
        // KB-005.
        let f = fixture();
        let a = doc(f.vault, "a.md", "A");
        upsert(f.db.conn(), &a).unwrap();
        let listed = paths(f.db.conn(), f.vault).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, a.id);
        assert_eq!(listed[0].2, a.content_hash);
    }

    #[test]
    fn rowids_map_both_ways_for_the_search_index() {
        let f = fixture();
        let d = doc(f.vault, "a.md", "A");
        upsert(f.db.conn(), &d).unwrap();
        let seq = seq_of(f.db.conn(), d.id).unwrap().unwrap();
        assert_eq!(by_seq(f.db.conn(), seq).unwrap().unwrap().id, d.id);
        assert!(seq_of(f.db.conn(), DocumentId::new()).unwrap().is_none());
    }

    #[test]
    fn name_folding_ignores_case_and_whitespace_but_not_punctuation() {
        assert_eq!(fold_name("  The   Pharos "), "the pharos");
        assert_eq!(fold_name("C# Notes"), "c# notes");
        assert_ne!(fold_name("C# Notes"), fold_name("C Notes"));
        assert_eq!(fold_name("Φάρος"), "φάρος");
        assert_eq!(fold_name("   "), "");
    }
}
