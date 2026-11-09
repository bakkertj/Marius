//! Chunks and embeddings for semantic retrieval.
//!
//! Requirements: SRCH-007 (local or policy-eligible embeddings), SRCH-008
//! (index passages as well as whole documents), SRCH-013 (incremental update),
//! open question 5 (model/version changes trigger reindexing).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{ChunkId, ContentHash, DocumentId};

/// Identifies which model produced a vector.
///
/// Vectors from different models are not comparable, so this is part of the
/// lookup key for every similarity query. Changing the configured model
/// invalidates existing vectors rather than silently mixing spaces — open
/// question 5 answered by construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EmbeddingModel {
    /// Provider-qualified model name, e.g. `ollama/nomic-embed-text`.
    pub name: String,
    /// Provider's own version or revision string, when it exposes one.
    pub version: Option<String>,
    /// Vector length. Stored so a dimension change is caught before a vector of
    /// the wrong width is compared against the index.
    pub dimensions: usize,
}

impl EmbeddingModel {
    pub fn new(name: impl Into<String>, dimensions: usize) -> Self {
        Self {
            name: name.into(),
            version: None,
            dimensions,
        }
    }

    /// Stable key identifying this exact embedding space.
    pub fn space_key(&self) -> String {
        match &self.version {
            Some(v) => format!("{}@{}:{}", self.name, v, self.dimensions),
            None => format!("{}:{}", self.name, self.dimensions),
        }
    }

    /// True when vectors from `self` and `other` may be compared.
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        self.space_key() == other.space_key()
    }
}

/// A contiguous passage of a document, the unit of semantic retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: ChunkId,
    pub document: DocumentId,
    /// Position within the document, from 0. Stable for a given content hash.
    pub ordinal: u32,
    /// Byte range in the document body, for precise citation highlighting
    /// (SRCH-010 requires clickable sources).
    pub byte_start: usize,
    pub byte_end: usize,
    /// Heading path the chunk sits under, e.g. `Design > Storage`. Carried into
    /// the prompt so a cited passage is identifiable to the reader.
    pub heading_path: Vec<String>,
    pub text: String,
}

impl Chunk {
    /// Human-readable citation label.
    pub fn citation_label(&self) -> String {
        if self.heading_path.is_empty() {
            format!("¶{}", self.ordinal + 1)
        } else {
            self.heading_path.join(" › ")
        }
    }

    pub fn byte_len(&self) -> usize {
        self.byte_end.saturating_sub(self.byte_start)
    }
}

/// A stored vector for one chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkEmbedding {
    pub chunk: ChunkId,
    pub document: DocumentId,
    pub model: EmbeddingModel,
    /// Hash of the document the vector was computed from. When the document's
    /// hash moves on, the vector is stale and gets recomputed (SRCH-013).
    pub source_hash: ContentHash,
    pub vector: Vec<f32>,
    #[serde(with = "time::serde::rfc3339")]
    pub computed_at: OffsetDateTime,
}

impl ChunkEmbedding {
    /// True when this vector still describes `document_hash` under `model`.
    pub fn is_current(&self, document_hash: &ContentHash, model: &EmbeddingModel) -> bool {
        &self.source_hash == document_hash && self.model.is_compatible_with(model)
    }
}

/// Cosine similarity of two vectors, in `-1.0..=1.0`.
///
/// Returns `None` for mismatched or empty vectors rather than a misleading
/// zero: a dimension mismatch is a configuration bug, not a weak match.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return None;
    }
    Some((dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_spaces_are_distinguished_by_model_and_width() {
        let a = EmbeddingModel::new("nomic-embed-text", 768);
        let b = EmbeddingModel::new("nomic-embed-text", 1024);
        let c = EmbeddingModel::new("bge-small", 768);
        assert!(
            !a.is_compatible_with(&b),
            "dimension change invalidates vectors"
        );
        assert!(!a.is_compatible_with(&c));
        assert!(a.is_compatible_with(&EmbeddingModel::new("nomic-embed-text", 768)));
    }

    #[test]
    fn model_version_participates_in_the_space_key() {
        let mut v1 = EmbeddingModel::new("m", 4);
        v1.version = Some("1".into());
        let mut v2 = v1.clone();
        v2.version = Some("2".into());
        assert!(!v1.is_compatible_with(&v2));
    }

    #[test]
    fn a_vector_goes_stale_when_its_document_changes() {
        // SRCH-013.
        let model = EmbeddingModel::new("m", 2);
        let hash = ContentHash::of(b"first");
        let emb = ChunkEmbedding {
            chunk: ChunkId::new(),
            document: DocumentId::new(),
            model: model.clone(),
            source_hash: hash,
            vector: vec![1.0, 0.0],
            computed_at: OffsetDateTime::now_utc(),
        };
        assert!(emb.is_current(&hash, &model));
        assert!(!emb.is_current(&ContentHash::of(b"second"), &model));
        assert!(!emb.is_current(&hash, &EmbeddingModel::new("other", 2)));
    }

    #[test]
    fn cosine_similarity_behaves_at_the_extremes() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]), Some(1.0));
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]), Some(-1.0));
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
    }

    #[test]
    fn cosine_similarity_refuses_incomparable_input() {
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), None);
        assert_eq!(cosine_similarity(&[], &[]), None);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), None);
    }

    #[test]
    fn chunks_label_themselves_for_citation() {
        let mut chunk = Chunk {
            id: ChunkId::new(),
            document: DocumentId::new(),
            ordinal: 2,
            byte_start: 10,
            byte_end: 40,
            heading_path: vec!["Design".into(), "Storage".into()],
            text: "…".into(),
        };
        assert_eq!(chunk.citation_label(), "Design › Storage");
        assert_eq!(chunk.byte_len(), 30);

        chunk.heading_path.clear();
        assert_eq!(chunk.citation_label(), "¶3");
    }
}
