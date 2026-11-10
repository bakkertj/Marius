//! Passages and their embedding vectors.
//!
//! Requirements: SRCH-007, SRCH-008 (index passages as well as whole
//! documents), SRCH-013 (incremental update), integrity rule 3 (rebuildable),
//! open question 5 (model and version changes trigger reindexing).
//!
//! Both tables are derived: everything here can be recomputed from the source
//! file and the configured model, which is why `Database::rebuild_derived`
//! truncates them. Vectors are stored as little-endian `f32` blobs, keyed by
//! the embedding space they came from, so two models' vectors can coexist
//! during a migration and are never compared with each other.

use marius_model::{Chunk, ChunkEmbedding, ChunkId, ContentHash, DocumentId, EmbeddingModel};
use rusqlite::{params, Connection, OptionalExtension};
use time::OffsetDateTime;

use crate::convert::{
    hash_from_row, hash_to_sql, id_from_row, json_from_row, json_to_sql, time_from_row,
    time_to_sql, IdSql,
};
use crate::{Result, StorageError};

/// Replace a document's passages.
///
/// Chunk identities are not stable across a re-chunk — the passage boundaries
/// themselves move when the document changes — so this deletes and reinserts,
/// which also cascades away the stale vectors.
pub fn replace(conn: &Connection, document: DocumentId, chunks: &[Chunk]) -> Result<()> {
    conn.execute(
        "DELETE FROM chunk WHERE document_id = ?1",
        params![document.to_sql_blob()],
    )?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO chunk (id, document_id, ordinal, byte_start, byte_end, heading_path, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for chunk in chunks {
        if chunk.document != document {
            return Err(StorageError::Corrupt(format!(
                "chunk {} belongs to document {} but was stored against {document}",
                chunk.id, chunk.document
            )));
        }
        stmt.execute(params![
            chunk.id.to_sql_blob(),
            document.to_sql_blob(),
            chunk.ordinal as i64,
            chunk.byte_start as i64,
            chunk.byte_end as i64,
            json_to_sql(&chunk.heading_path)?,
            chunk.text,
        ])?;
    }
    Ok(())
}

/// A document's passages, in order.
pub fn of_document(conn: &Connection, document: DocumentId) -> Result<Vec<Chunk>> {
    let mut stmt = conn.prepare(
        "SELECT id, document_id, ordinal, byte_start, byte_end, heading_path, text
         FROM chunk WHERE document_id = ?1 ORDER BY ordinal",
    )?;
    let rows = stmt
        .query_map(params![document.to_sql_blob()], row_to_chunk)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Fetch one passage, for rendering a citation.
pub fn get(conn: &Connection, id: ChunkId) -> Result<Option<Chunk>> {
    let mut stmt = conn.prepare(
        "SELECT id, document_id, ordinal, byte_start, byte_end, heading_path, text
         FROM chunk WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id.to_sql_blob()], row_to_chunk)?;
    Ok(rows.next().transpose()?)
}

/// Store a vector.
pub fn put_embedding(conn: &Connection, embedding: &ChunkEmbedding) -> Result<()> {
    conn.execute(
        "INSERT INTO chunk_embedding
            (chunk_id, document_id, space_key, model, dimensions, source_hash, vector, computed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(chunk_id, space_key) DO UPDATE SET
            source_hash = excluded.source_hash,
            vector = excluded.vector,
            computed_at = excluded.computed_at",
        params![
            embedding.chunk.to_sql_blob(),
            embedding.document.to_sql_blob(),
            embedding.model.space_key(),
            embedding.model.name,
            embedding.model.dimensions as i64,
            hash_to_sql(&embedding.source_hash),
            encode_vector(&embedding.vector),
            time_to_sql(embedding.computed_at),
        ],
    )?;
    Ok(())
}

/// Every vector in one embedding space, for a similarity scan.
///
/// A brute-force scan is the right starting point: it is exact, has no index to
/// keep in sync, and at 100,000 chunks a cosine pass over 768-dimensional
/// vectors is a few tens of milliseconds. An approximate index becomes worth
/// its complexity well beyond that (open question 5).
pub fn embeddings_in_space(
    conn: &Connection,
    model: &EmbeddingModel,
) -> Result<Vec<ChunkEmbedding>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_id, document_id, space_key, model, dimensions, source_hash, vector,
                computed_at
         FROM chunk_embedding WHERE space_key = ?1",
    )?;
    let rows = stmt
        .query_map(params![model.space_key()], row_to_embedding)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Passages of a document that have no current vector in `model`'s space.
///
/// "Current" means computed from the document's present content hash, so a
/// changed document's vectors are recomputed rather than quietly reused
/// (SRCH-013).
pub fn stale_chunks(
    conn: &Connection,
    document: DocumentId,
    model: &EmbeddingModel,
    document_hash: &ContentHash,
) -> Result<Vec<Chunk>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.document_id, c.ordinal, c.byte_start, c.byte_end, c.heading_path, c.text
         FROM chunk c
         LEFT JOIN chunk_embedding e
            ON e.chunk_id = c.id AND e.space_key = ?2 AND e.source_hash = ?3
         WHERE c.document_id = ?1 AND e.chunk_id IS NULL
         ORDER BY c.ordinal",
    )?;
    let rows = stmt
        .query_map(
            params![
                document.to_sql_blob(),
                model.space_key(),
                hash_to_sql(document_hash)
            ],
            row_to_chunk,
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Discard every vector outside the given space, after a model change.
pub fn drop_other_spaces(conn: &Connection, keep: &EmbeddingModel) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM chunk_embedding WHERE space_key <> ?1",
        params![keep.space_key()],
    )?)
}

/// How many passages and vectors are stored.
pub fn counts(conn: &Connection) -> Result<(u64, u64)> {
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunk", [], |row| row.get(0))?;
    let vectors: i64 =
        conn.query_row("SELECT COUNT(*) FROM chunk_embedding", [], |row| row.get(0))?;
    Ok((chunks as u64, vectors as u64))
}

/// Vectors are `f32` little-endian, so a database file is portable between a
/// Mac and a PC.
fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn decode_vector(bytes: &[u8]) -> std::result::Result<Vec<f32>, StorageError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(StorageError::Corrupt(format!(
            "embedding blob is {} bytes, not a whole number of f32s",
            bytes.len()
        )));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect())
}

fn row_to_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chunk> {
    Ok(Chunk {
        id: id_from_row(row, 0)?,
        document: id_from_row(row, 1)?,
        ordinal: row.get::<_, i64>(2)? as u32,
        byte_start: row.get::<_, i64>(3)? as usize,
        byte_end: row.get::<_, i64>(4)? as usize,
        heading_path: json_from_row(row, 5)?,
        text: row.get(6)?,
    })
}

fn row_to_embedding(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkEmbedding> {
    let blob: Vec<u8> = row.get(6)?;
    let vector = decode_vector(&blob).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Blob, Box::new(e))
    })?;
    let space_key: String = row.get(2)?;
    let name: String = row.get(3)?;
    let dimensions = row.get::<_, i64>(4)? as usize;
    // Recover the version from the space key, which is `name[@version]:dims`.
    let version = space_key
        .strip_prefix(&name)
        .and_then(|rest| rest.strip_prefix('@'))
        .and_then(|rest| rest.split(':').next())
        .map(str::to_owned);

    Ok(ChunkEmbedding {
        chunk: id_from_row(row, 0)?,
        document: id_from_row(row, 1)?,
        model: EmbeddingModel {
            name,
            version,
            dimensions,
        },
        source_hash: hash_from_row(row, 5)?,
        vector,
        computed_at: time_from_row(row, 7)?,
    })
}

/// Build an embedding record for a chunk.
pub fn embedding_for(
    chunk: &Chunk,
    model: &EmbeddingModel,
    source_hash: ContentHash,
    vector: Vec<f32>,
) -> ChunkEmbedding {
    ChunkEmbedding {
        chunk: chunk.id,
        document: chunk.document,
        model: model.clone(),
        source_hash,
        vector,
        computed_at: OffsetDateTime::now_utc(),
    }
}

/// Every vector in the most populous embedding space, for discovery's
/// candidate nomination. One space only: vectors from different models
/// must never be compared, so the minority left over after a model change
/// is ignored rather than mixed in.
pub fn dominant_space_embeddings(conn: &Connection) -> Result<Vec<ChunkEmbedding>> {
    let space: Option<String> = conn
        .query_row(
            "SELECT space_key FROM chunk_embedding
             GROUP BY space_key ORDER BY COUNT(*) DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(space) = space else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT chunk_id, document_id, space_key, model, dimensions, source_hash, vector,
                computed_at
         FROM chunk_embedding WHERE space_key = ?1",
    )?;
    let rows = stmt
        .query_map(params![space], row_to_embedding)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{documents, vaults, Database};
    use marius_model::{
        Authorship, Document, DocumentKind, Vault, VaultId, VaultPath, VaultPolicy,
    };
    use std::collections::BTreeMap;

    struct Fixture {
        db: Database,
        doc: DocumentId,
        hash: ContentHash,
    }

    fn fixture() -> Fixture {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::new_local("V", "/v");
        vaults::upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        let hash = ContentHash::of(b"body v1");
        let doc = document(vault.id, hash);
        documents::upsert(db.conn(), &doc).unwrap();
        Fixture {
            db,
            doc: doc.id,
            hash,
        }
    }

    fn document(vault: VaultId, hash: ContentHash) -> Document {
        let now = OffsetDateTime::now_utc();
        Document {
            id: DocumentId::new(),
            vault,
            path: VaultPath::new("a.md").unwrap(),
            title: "A".into(),
            kind: DocumentKind::Markdown,
            authorship: Authorship::Source,
            content_hash: hash,
            size_bytes: 7,
            created_at: now,
            modified_at: now,
            indexed_at: now,
            archived_at: None,
            sensitivity: None,
            frontmatter: BTreeMap::new(),
            external: false,
        }
    }

    fn chunk(document: DocumentId, ordinal: u32, text: &str) -> Chunk {
        Chunk {
            id: ChunkId::new(),
            document,
            ordinal,
            byte_start: ordinal as usize * 100,
            byte_end: ordinal as usize * 100 + text.len(),
            heading_path: vec!["Section".into()],
            text: text.to_owned(),
        }
    }

    fn model() -> EmbeddingModel {
        EmbeddingModel::new("nomic-embed-text", 4)
    }

    #[test]
    fn chunks_round_trip_in_order() {
        let f = fixture();
        let chunks = vec![
            chunk(f.doc, 0, "first passage"),
            chunk(f.doc, 1, "second passage"),
        ];
        replace(f.db.conn(), f.doc, &chunks).unwrap();

        let got = of_document(f.db.conn(), f.doc).unwrap();
        assert_eq!(got, chunks);
        assert_eq!(got[0].heading_path, vec!["Section"]);
        assert_eq!(get(f.db.conn(), chunks[1].id).unwrap().unwrap(), chunks[1]);
    }

    #[test]
    fn rechunking_replaces_rather_than_accumulates() {
        // SRCH-013.
        let f = fixture();
        replace(f.db.conn(), f.doc, &[chunk(f.doc, 0, "old")]).unwrap();
        replace(
            f.db.conn(),
            f.doc,
            &[chunk(f.doc, 0, "new a"), chunk(f.doc, 1, "new b")],
        )
        .unwrap();
        let got = of_document(f.db.conn(), f.doc).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].text, "new a");
    }

    #[test]
    fn rechunking_discards_the_vectors_of_the_old_passages() {
        let f = fixture();
        let old = chunk(f.doc, 0, "old");
        replace(f.db.conn(), f.doc, std::slice::from_ref(&old)).unwrap();
        put_embedding(
            f.db.conn(),
            &embedding_for(&old, &model(), f.hash, vec![1.0, 0.0, 0.0, 0.0]),
        )
        .unwrap();
        assert_eq!(counts(f.db.conn()).unwrap(), (1, 1));

        replace(f.db.conn(), f.doc, &[chunk(f.doc, 0, "new")]).unwrap();
        assert_eq!(
            counts(f.db.conn()).unwrap(),
            (1, 0),
            "stale vectors must not survive a re-chunk"
        );
    }

    #[test]
    fn a_chunk_belonging_to_another_document_is_refused() {
        let f = fixture();
        let foreign = chunk(DocumentId::new(), 0, "x");
        assert!(replace(f.db.conn(), f.doc, &[foreign]).is_err());
    }

    #[test]
    fn vectors_round_trip_exactly() {
        let f = fixture();
        let c = chunk(f.doc, 0, "passage");
        replace(f.db.conn(), f.doc, std::slice::from_ref(&c)).unwrap();
        let vector = vec![0.5, -0.25, 1.0, 0.0];
        put_embedding(
            f.db.conn(),
            &embedding_for(&c, &model(), f.hash, vector.clone()),
        )
        .unwrap();

        let stored = embeddings_in_space(f.db.conn(), &model()).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].vector, vector);
        assert_eq!(stored[0].model, model());
        assert!(stored[0].is_current(&f.hash, &model()));
    }

    #[test]
    fn a_model_version_survives_the_round_trip() {
        let f = fixture();
        let c = chunk(f.doc, 0, "passage");
        replace(f.db.conn(), f.doc, std::slice::from_ref(&c)).unwrap();
        let mut versioned = model();
        versioned.version = Some("2024-06".into());
        put_embedding(
            f.db.conn(),
            &embedding_for(&c, &versioned, f.hash, vec![1.0, 1.0, 1.0, 1.0]),
        )
        .unwrap();

        let stored = embeddings_in_space(f.db.conn(), &versioned).unwrap();
        assert_eq!(stored[0].model, versioned);
        // And the unversioned space is a different space.
        assert!(embeddings_in_space(f.db.conn(), &model())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn two_embedding_spaces_coexist_without_mixing() {
        // Open question 5: a model change must not silently compare vectors.
        let f = fixture();
        let c = chunk(f.doc, 0, "passage");
        replace(f.db.conn(), f.doc, std::slice::from_ref(&c)).unwrap();
        let small = EmbeddingModel::new("small", 2);
        let large = EmbeddingModel::new("large", 3);
        put_embedding(
            f.db.conn(),
            &embedding_for(&c, &small, f.hash, vec![1.0, 0.0]),
        )
        .unwrap();
        put_embedding(
            f.db.conn(),
            &embedding_for(&c, &large, f.hash, vec![1.0, 0.0, 0.0]),
        )
        .unwrap();

        assert_eq!(embeddings_in_space(f.db.conn(), &small).unwrap().len(), 1);
        assert_eq!(
            embeddings_in_space(f.db.conn(), &large).unwrap()[0]
                .vector
                .len(),
            3
        );
        assert_eq!(counts(f.db.conn()).unwrap().1, 2);

        assert_eq!(drop_other_spaces(f.db.conn(), &large).unwrap(), 1);
        assert!(embeddings_in_space(f.db.conn(), &small).unwrap().is_empty());
    }

    #[test]
    fn a_changed_document_makes_its_vectors_stale() {
        // SRCH-013.
        let f = fixture();
        let c = chunk(f.doc, 0, "passage");
        replace(f.db.conn(), f.doc, std::slice::from_ref(&c)).unwrap();
        put_embedding(
            f.db.conn(),
            &embedding_for(&c, &model(), f.hash, vec![1.0, 0.0, 0.0, 0.0]),
        )
        .unwrap();

        assert!(stale_chunks(f.db.conn(), f.doc, &model(), &f.hash)
            .unwrap()
            .is_empty());

        let edited = ContentHash::of(b"body v2");
        let stale = stale_chunks(f.db.conn(), f.doc, &model(), &edited).unwrap();
        assert_eq!(stale.len(), 1, "a changed document needs recomputation");
    }

    #[test]
    fn a_chunk_with_no_vector_is_stale() {
        let f = fixture();
        replace(f.db.conn(), f.doc, &[chunk(f.doc, 0, "never embedded")]).unwrap();
        assert_eq!(
            stale_chunks(f.db.conn(), f.doc, &model(), &f.hash)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn deleting_a_document_removes_its_passages_and_vectors() {
        let f = fixture();
        let c = chunk(f.doc, 0, "passage");
        replace(f.db.conn(), f.doc, std::slice::from_ref(&c)).unwrap();
        put_embedding(
            f.db.conn(),
            &embedding_for(&c, &model(), f.hash, vec![1.0, 0.0, 0.0, 0.0]),
        )
        .unwrap();

        documents::delete(f.db.conn(), f.doc).unwrap();
        assert_eq!(counts(f.db.conn()).unwrap(), (0, 0));
    }

    #[test]
    fn a_rebuild_clears_passages_and_vectors() {
        // NFR-007, integrity rule 3.
        let mut db = Database::open_in_memory().unwrap();
        let vault = Vault::new_local("V", "/v");
        vaults::upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        let hash = ContentHash::of(b"x");
        let doc = document(vault.id, hash);
        documents::upsert(db.conn(), &doc).unwrap();
        let c = chunk(doc.id, 0, "passage");
        replace(db.conn(), doc.id, std::slice::from_ref(&c)).unwrap();
        put_embedding(
            db.conn(),
            &embedding_for(&c, &model(), hash, vec![1.0, 0.0, 0.0, 0.0]),
        )
        .unwrap();

        db.rebuild_derived().unwrap();
        assert_eq!(counts(db.conn()).unwrap(), (0, 0));
        assert!(documents::get(db.conn(), doc.id).unwrap().is_some());
    }

    #[test]
    fn a_malformed_vector_blob_is_reported_as_corruption() {
        assert!(decode_vector(&[1, 2, 3]).is_err());
        assert_eq!(decode_vector(&[0, 0, 128, 63]).unwrap(), vec![1.0]);
    }
}
