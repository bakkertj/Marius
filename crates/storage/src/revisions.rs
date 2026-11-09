//! Revision history.
//!
//! Requirements: KB-011 (maintain revision history and recover earlier
//! revisions), AI-013 and AC-010 (regeneration creates a new revision instead
//! of overwriting a user's edits), NFR-001, integrity rule 6.
//!
//! Bodies live in their own table so that history can be pruned for space
//! without losing the revision *metadata* that proves what happened and when —
//! which is the part SEC-006's review trail needs.

use marius_model::{ContentHash, DocumentId, DocumentRevision, RevisionCause, RevisionId};
use rusqlite::{params, Connection, OptionalExtension};

use crate::convert::{
    hash_from_row, hash_to_sql, id_from_row, parsed_from_row, time_from_row, time_to_sql, IdSql,
};
use crate::Result;

const COLUMNS: &str =
    "id, document_id, sequence, cause, content_hash, size_bytes, path, created_at, note";

/// What to record about a change.
///
/// A named struct rather than eight positional arguments: `(hash, size, path,
/// body, note)` in a row is easy to transpose at a call site and impossible to
/// read at a glance.
#[derive(Debug, Clone)]
pub struct RevisionDraft<'a> {
    pub cause: RevisionCause,
    pub content_hash: ContentHash,
    pub size_bytes: u64,
    /// Path at the time of the change, so a restore can restore location too.
    pub path: &'a str,
    /// The body to keep. `None` records the change without duplicating
    /// content — right for a relocation, where nothing was edited, and for a
    /// file too large to be worth storing twice.
    pub body: Option<&'a str>,
    pub note: Option<&'a str>,
}

impl<'a> RevisionDraft<'a> {
    /// A draft describing a change to `path` with the given content.
    pub fn new(
        cause: RevisionCause,
        content_hash: ContentHash,
        size_bytes: u64,
        path: &'a str,
    ) -> Self {
        Self {
            cause,
            content_hash,
            size_bytes,
            path,
            body: None,
            note: None,
        }
    }

    /// Keep the body, so the revision can be recovered (KB-011).
    pub fn with_body(mut self, body: &'a str) -> Self {
        self.body = Some(body);
        self
    }

    /// Attach a human-readable note, shown in the history view.
    pub fn with_note(mut self, note: &'a str) -> Self {
        self.note = Some(note);
        self
    }
}

/// Append a revision, assigning the next sequence number.
pub fn record(
    conn: &Connection,
    document: DocumentId,
    draft: RevisionDraft<'_>,
) -> Result<DocumentRevision> {
    let RevisionDraft {
        cause,
        content_hash,
        size_bytes,
        path,
        body,
        note,
    } = draft;
    let sequence = next_sequence(conn, document)?;
    let mut revision =
        DocumentRevision::new(document, sequence, cause, content_hash, size_bytes, path);
    revision.note = note.map(str::to_owned);

    conn.execute(
        "INSERT INTO document_revision
            (id, document_id, sequence, cause, content_hash, size_bytes, path, created_at, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            revision.id.to_sql_blob(),
            document.to_sql_blob(),
            sequence as i64,
            cause.as_str(),
            hash_to_sql(&content_hash),
            size_bytes as i64,
            path,
            time_to_sql(revision.created_at),
            revision.note,
        ],
    )?;
    if let Some(body) = body {
        conn.execute(
            "INSERT INTO revision_body (revision_id, body) VALUES (?1, ?2)",
            params![revision.id.to_sql_blob(), body],
        )?;
    }
    // Revisions are journalled so that history converges across devices, but
    // they are append-only, so revision 1 is always correct here.
    crate::changelog::record(
        conn,
        vault_of(conn, document)?,
        marius_model::EntityKind::DocumentRevision,
        &revision.id.to_string(),
        marius_model::ChangeKind::Upsert,
        1,
    )?;
    Ok(revision)
}

fn vault_of(conn: &Connection, document: DocumentId) -> Result<marius_model::VaultId> {
    let vault: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vault_id FROM document WHERE id = ?1",
            params![document.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(vault
        .map(|v| marius_model::VaultId::from_sql_blob(&v))
        .transpose()?
        .unwrap_or_else(marius_model::VaultId::nil))
}

/// The sequence number the next revision will take.
pub fn next_sequence(conn: &Connection, document: DocumentId) -> Result<u64> {
    let highest: Option<i64> = conn.query_row(
        "SELECT MAX(sequence) FROM document_revision WHERE document_id = ?1",
        params![document.to_sql_blob()],
        |row| row.get(0),
    )?;
    Ok(highest.unwrap_or(0) as u64 + 1)
}

/// A document's history, newest first.
pub fn list(conn: &Connection, document: DocumentId) -> Result<Vec<DocumentRevision>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM document_revision WHERE document_id = ?1
         ORDER BY sequence DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], row_to_revision)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Fetch one revision.
pub fn get(conn: &Connection, id: RevisionId) -> Result<Option<DocumentRevision>> {
    let sql = format!("SELECT {COLUMNS} FROM document_revision WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![id.to_sql_blob()], row_to_revision)
        .optional()?)
}

/// The most recent revision.
pub fn latest(conn: &Connection, document: DocumentId) -> Result<Option<DocumentRevision>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM document_revision WHERE document_id = ?1
         ORDER BY sequence DESC LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![document.to_sql_blob()], row_to_revision)
        .optional()?)
}

/// The stored body of a revision, for recovery and diffing (KB-011).
pub fn body(conn: &Connection, id: RevisionId) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT body FROM revision_body WHERE revision_id = ?1",
            params![id.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?)
}

/// Drop old revision *bodies*, keeping metadata and every user-authored
/// revision.
///
/// Integrity rule 6 is the reason for the asymmetry: a generated body can be
/// regenerated, a person's edit cannot, so pruning never discards the bytes a
/// person wrote while a newer one is still recoverable.
pub fn prune_bodies(conn: &Connection, document: DocumentId, keep_recent: usize) -> Result<usize> {
    let revisions = list(conn, document)?;
    let mut dropped = 0usize;
    for revision in revisions.iter().skip(keep_recent) {
        if revision.cause.is_user_authored() {
            continue;
        }
        dropped += conn.execute(
            "DELETE FROM revision_body WHERE revision_id = ?1",
            params![revision.id.to_sql_blob()],
        )?;
    }
    Ok(dropped)
}

/// How many revisions a document has.
pub fn count(conn: &Connection, document: DocumentId) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM document_revision WHERE document_id = ?1",
        params![document.to_sql_blob()],
        |row| row.get(0),
    )?;
    Ok(n as u64)
}

/// The revision a given content hash was last seen at, so an external edit that
/// restores an earlier state is recognised rather than recorded as novel.
pub fn by_hash(
    conn: &Connection,
    document: DocumentId,
    hash: &ContentHash,
) -> Result<Option<DocumentRevision>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM document_revision
         WHERE document_id = ?1 AND content_hash = ?2
         ORDER BY sequence DESC LIMIT 1"
    );
    Ok(conn
        .query_row(
            &sql,
            params![document.to_sql_blob(), hash_to_sql(hash)],
            row_to_revision,
        )
        .optional()?)
}

fn row_to_revision(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRevision> {
    Ok(DocumentRevision {
        id: id_from_row(row, 0)?,
        document: id_from_row(row, 1)?,
        sequence: row.get::<_, i64>(2)? as u64,
        cause: parsed_from_row(row, 3)?,
        content_hash: hash_from_row(row, 4)?,
        size_bytes: row.get::<_, i64>(5)? as u64,
        path: row.get(6)?,
        created_at: time_from_row(row, 7)?,
        note: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{documents, vaults, Database};
    use marius_model::{
        Authorship, Document, DocumentKind, Vault, VaultId, VaultPath, VaultPolicy,
    };
    use std::collections::BTreeMap;
    use time::OffsetDateTime;

    struct Fixture {
        db: Database,
        doc: DocumentId,
    }

    fn fixture() -> Fixture {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::new_local("V", "/v");
        vaults::upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        let doc = document(vault.id);
        documents::upsert(db.conn(), &doc).unwrap();
        Fixture { db, doc: doc.id }
    }

    fn document(vault: VaultId) -> Document {
        let now = OffsetDateTime::now_utc();
        Document {
            id: DocumentId::new(),
            vault,
            path: VaultPath::new("a.md").unwrap(),
            title: "A".into(),
            kind: DocumentKind::Markdown,
            authorship: Authorship::Source,
            content_hash: ContentHash::of(b"v1"),
            size_bytes: 2,
            created_at: now,
            modified_at: now,
            indexed_at: now,
            archived_at: None,
            sensitivity: None,
            frontmatter: BTreeMap::new(),
            external: false,
        }
    }

    fn add(f: &Fixture, cause: RevisionCause, body: &str) -> DocumentRevision {
        record(
            f.db.conn(),
            f.doc,
            RevisionDraft::new(
                cause,
                ContentHash::of(body.as_bytes()),
                body.len() as u64,
                "a.md",
            )
            .with_body(body),
        )
        .unwrap()
    }

    #[test]
    fn revisions_are_numbered_consecutively_from_one() {
        let f = fixture();
        for expected in 1..=3 {
            assert_eq!(
                add(&f, RevisionCause::Edit, &format!("v{expected}")).sequence,
                expected
            );
        }
        assert_eq!(count(f.db.conn(), f.doc).unwrap(), 3);
    }

    #[test]
    fn history_is_newest_first_and_bodies_are_recoverable() {
        // KB-011.
        let f = fixture();
        add(&f, RevisionCause::Created, "first");
        add(&f, RevisionCause::Edit, "second");

        let history = list(f.db.conn(), f.doc).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].sequence, 2);
        assert_eq!(
            body(f.db.conn(), history[0].id).unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(
            body(f.db.conn(), history[1].id).unwrap().as_deref(),
            Some("first")
        );
    }

    #[test]
    fn the_latest_revision_is_the_highest_sequence() {
        let f = fixture();
        add(&f, RevisionCause::Created, "a");
        let newest = add(&f, RevisionCause::Edit, "b");
        assert_eq!(latest(f.db.conn(), f.doc).unwrap().unwrap().id, newest.id);
    }

    #[test]
    fn a_relocation_records_the_path_without_a_body() {
        // AC-003: the identity trail shows the move.
        let f = fixture();
        let revision = record(
            f.db.conn(),
            f.doc,
            RevisionDraft::new(
                RevisionCause::Relocation,
                ContentHash::of(b"unchanged"),
                9,
                "archive/a.md",
            )
            .with_note("moved to archive"),
        )
        .unwrap();
        assert_eq!(revision.path, "archive/a.md");
        assert_eq!(revision.note.as_deref(), Some("moved to archive"));
        assert_eq!(body(f.db.conn(), revision.id).unwrap(), None);
    }

    #[test]
    fn pruning_never_discards_a_body_a_person_wrote() {
        // Integrity rule 6.
        let f = fixture();
        let user_edit = add(&f, RevisionCause::Edit, "the user's words");
        let generated = add(&f, RevisionCause::Generation, "a model's words");
        let external = add(
            &f,
            RevisionCause::ExternalChange,
            "edited in another editor",
        );
        let newest = add(&f, RevisionCause::Generation, "latest generation");

        // Keep only the newest; everything older is a pruning candidate.
        prune_bodies(f.db.conn(), f.doc, 1).unwrap();

        assert!(
            body(f.db.conn(), newest.id).unwrap().is_some(),
            "kept by recency"
        );
        assert!(
            body(f.db.conn(), user_edit.id).unwrap().is_some(),
            "user edit"
        );
        assert!(
            body(f.db.conn(), external.id).unwrap().is_some(),
            "external edit"
        );
        assert!(
            body(f.db.conn(), generated.id).unwrap().is_none(),
            "a regenerable body may be pruned"
        );

        // Metadata survives in every case, so the trail stays complete.
        assert_eq!(count(f.db.conn(), f.doc).unwrap(), 4);
        assert!(get(f.db.conn(), generated.id).unwrap().is_some());
    }

    #[test]
    fn an_earlier_state_is_recognised_by_its_hash() {
        let f = fixture();
        add(&f, RevisionCause::Created, "original");
        add(&f, RevisionCause::Edit, "changed");

        let found = by_hash(f.db.conn(), f.doc, &ContentHash::of(b"original"))
            .unwrap()
            .unwrap();
        assert_eq!(found.sequence, 1);
        assert!(by_hash(f.db.conn(), f.doc, &ContentHash::of(b"never"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn deleting_a_document_removes_its_history() {
        let f = fixture();
        add(&f, RevisionCause::Created, "a");
        documents::delete(f.db.conn(), f.doc).unwrap();
        assert_eq!(count(f.db.conn(), f.doc).unwrap(), 0);
    }

    #[test]
    fn two_documents_keep_independent_sequences() {
        let f = fixture();
        let other = {
            let mut d = document(documents::get(f.db.conn(), f.doc).unwrap().unwrap().vault);
            d.path = VaultPath::new("b.md").unwrap();
            documents::upsert(f.db.conn(), &d).unwrap();
            d.id
        };
        add(&f, RevisionCause::Created, "a1");
        add(&f, RevisionCause::Edit, "a2");
        assert_eq!(next_sequence(f.db.conn(), other).unwrap(), 1);
        assert_eq!(next_sequence(f.db.conn(), f.doc).unwrap(), 3);
    }
}
