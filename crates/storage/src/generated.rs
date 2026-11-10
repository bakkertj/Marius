//! Provenance of generated notes (AI-011, AI-013, AC-010, integrity rules
//! 5 and 6).
//!
//! One row per derived document, stating which model produced which
//! revision from which sources. The row outlives model reconfiguration —
//! the UI must always be able to say what made a note — and its
//! `user_edited` flag is the single gate regeneration consults before
//! daring to touch a body.

use marius_model::{Citation, DocumentId, GeneratedNoteProvenance, InferenceTask};
use rusqlite::{params, Connection, OptionalExtension};

use crate::convert::{id_from_row, time_from_row, time_to_sql, IdSql};
use crate::{Result, StorageError};

/// Store or replace the provenance of one generated note.
pub fn put(conn: &Connection, provenance: &GeneratedNoteProvenance) -> Result<()> {
    let sources = serde_json::to_string(&provenance.sources)
        .map_err(|e| StorageError::Corrupt(format!("citations not serialisable: {e}")))?;
    conn.execute(
        "INSERT INTO generated_note_provenance
            (document_id, revision_id, inference_id, task, provider, model, prompt_id,
             sources, generated_at, user_edited)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(document_id) DO UPDATE SET
            revision_id = excluded.revision_id,
            inference_id = excluded.inference_id,
            task = excluded.task,
            provider = excluded.provider,
            model = excluded.model,
            prompt_id = excluded.prompt_id,
            sources = excluded.sources,
            generated_at = excluded.generated_at,
            user_edited = excluded.user_edited",
        params![
            provenance.document.to_sql_blob(),
            provenance.revision.to_sql_blob(),
            provenance.inference.to_sql_blob(),
            provenance.task.as_str(),
            provenance.provider,
            provenance.model,
            provenance.prompt_id,
            sources,
            time_to_sql(provenance.generated_at),
            provenance.user_edited as i64,
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, document: DocumentId) -> Result<Option<GeneratedNoteProvenance>> {
    conn.query_row(
        "SELECT document_id, revision_id, inference_id, task, provider, model, prompt_id,
                sources, generated_at, user_edited
         FROM generated_note_provenance WHERE document_id = ?1",
        params![document.to_sql_blob()],
        row_to_provenance,
    )
    .optional()
    .map_err(Into::into)
}

/// Record that a person touched the note's body. One-way: nothing unsets
/// it, because "the user edited this once" never stops being true
/// (integrity rule 6).
pub fn mark_user_edited(conn: &Connection, document: DocumentId) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE generated_note_provenance SET user_edited = 1 WHERE document_id = ?1",
        params![document.to_sql_blob()],
    )? > 0)
}

fn row_to_provenance(
    row: &rusqlite::Row<'_>,
) -> std::result::Result<GeneratedNoteProvenance, rusqlite::Error> {
    let sources: String = row.get(7)?;
    let sources: Vec<Citation> = serde_json::from_str(&sources).unwrap_or_default();
    Ok(GeneratedNoteProvenance {
        document: id_from_row(row, 0)?,
        revision: id_from_row(row, 1)?,
        inference: id_from_row(row, 2)?,
        task: row
            .get::<_, String>(3)?
            .parse::<InferenceTask>()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        provider: row.get(4)?,
        model: row.get(5)?,
        prompt_id: row.get(6)?,
        sources,
        generated_at: time_from_row(row, 8)?,
        user_edited: row.get::<_, i64>(9)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use marius_model::{GlobalDocumentId, InferenceId, RevisionId, VaultId};
    use time::OffsetDateTime;

    #[test]
    fn provenance_round_trips_and_user_edited_is_one_way() {
        let db = Database::open_in_memory().unwrap();
        // The FK needs a document row; borrow the vault fixture dance.
        let vault = marius_model::Vault::new_local("V", "/v");
        crate::vaults::upsert(
            db.conn(),
            &vault,
            &marius_model::VaultPolicy::conservative(vault.id),
        )
        .unwrap();
        let doc = marius_model::Document {
            id: DocumentId::new(),
            vault: vault.id,
            path: marius_model::VaultPath::new("s.md").unwrap(),
            title: "S".into(),
            kind: marius_model::DocumentKind::Markdown,
            authorship: marius_model::Authorship::Derived,
            content_hash: marius_model::ContentHash::of(b"s"),
            size_bytes: 1,
            created_at: OffsetDateTime::now_utc(),
            modified_at: OffsetDateTime::now_utc(),
            indexed_at: OffsetDateTime::now_utc(),
            archived_at: None,
            sensitivity: None,
            frontmatter: Default::default(),
            external: false,
        };
        crate::documents::upsert(db.conn(), &doc).unwrap();

        let provenance = GeneratedNoteProvenance {
            document: doc.id,
            revision: RevisionId::new(),
            inference: InferenceId::new(),
            task: InferenceTask::Summarization,
            provider: "ollama".into(),
            model: "llama3.2".into(),
            prompt_id: Some("summary-v1".into()),
            sources: vec![Citation {
                document: GlobalDocumentId::new(VaultId::new(), DocumentId::new()),
                chunk: None,
                label: "Pharos".into(),
                byte_range: None,
            }],
            generated_at: OffsetDateTime::now_utc(),
            user_edited: false,
        };
        put(db.conn(), &provenance).unwrap();
        let loaded = get(db.conn(), doc.id).unwrap().unwrap();
        assert_eq!(loaded, provenance);
        assert!(loaded.allows_in_place_regeneration());

        assert!(mark_user_edited(db.conn(), doc.id).unwrap());
        let loaded = get(db.conn(), doc.id).unwrap().unwrap();
        assert!(loaded.user_edited);
        assert!(!loaded.allows_in_place_regeneration());
    }
}
