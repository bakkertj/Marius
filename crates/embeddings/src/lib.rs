//! # Vector ranking and hybrid fusion
//!
//! The pure half of semantic search (SRCH-007, SRCH-009): given a query
//! vector and stored chunk embeddings, rank passages; given two ranked
//! lists, fuse them. The halves that do I/O — chunking documents, calling
//! an embedding model, the policy gate, the audit row — live in
//! `marius-core`, which is also where the tests that exercise them live.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use marius_model::{cosine_similarity, ChunkEmbedding, ChunkId, DocumentId};

/// One passage, scored against a query.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedChunk {
    pub chunk: ChunkId,
    pub document: DocumentId,
    /// Cosine similarity, `-1.0..=1.0`.
    pub score: f32,
}

/// Rank every stored vector against the query, best first.
///
/// Vectors from a different space (wrong dimension) are skipped, not
/// scored: `cosine_similarity` returns `None` for them, and a zero would
/// quietly bury a configuration bug in the middle of the ranking.
pub fn rank(query: &[f32], embeddings: &[ChunkEmbedding]) -> Vec<RankedChunk> {
    let mut out: Vec<RankedChunk> = embeddings
        .iter()
        .filter_map(|e| {
            cosine_similarity(query, &e.vector).map(|score| RankedChunk {
                chunk: e.chunk,
                document: e.document,
                score,
            })
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// Collapse a chunk ranking to one entry per document — its best passage —
/// preserving score order. Documents are the unit of search results;
/// chunks are the unit of citation.
pub fn best_per_document(ranked: &[RankedChunk]) -> Vec<RankedChunk> {
    let mut seen: HashMap<DocumentId, ()> = HashMap::new();
    ranked
        .iter()
        .filter(|r| seen.insert(r.document, ()).is_none())
        .cloned()
        .collect()
}

/// Reciprocal-rank fusion of several rankings (SRCH-009).
///
/// `k` damps the head of each list; 60.0 is the value from the original
/// RRF paper and nobody has needed another. Items missing from a list
/// simply contribute nothing from it — RRF needs no score calibration
/// between BM25 and cosine, which is exactly why it is used here.
pub fn fuse_reciprocal_rank(lists: &[&[DocumentId]], k: f64) -> Vec<(DocumentId, f64)> {
    let mut scores: HashMap<DocumentId, f64> = HashMap::new();
    let mut first_seen: HashMap<DocumentId, usize> = HashMap::new();
    let mut order = 0usize;
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
            first_seen.entry(*id).or_insert_with(|| {
                order += 1;
                order
            });
        }
    }
    let mut out: Vec<(DocumentId, f64)> = scores.into_iter().collect();
    // Deterministic: ties break by first appearance, not HashMap order.
    out.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| first_seen[&a.0].cmp(&first_seen[&b.0]))
    });
    out
}

/// Candidate pairs for relationship discovery (GRAPH-007): documents whose
/// vector centroids sit closer than `min_similarity`, best first, capped.
///
/// Embeddings are the cheap nominator; a model is the judge. The centroid
/// of a document's chunks is crude but exactly crude enough for a short
/// list the judge then reads properly.
pub fn similar_document_pairs(
    embeddings: &[ChunkEmbedding],
    min_similarity: f32,
    max_pairs: usize,
) -> Vec<(DocumentId, DocumentId, f32)> {
    let mut sums: HashMap<DocumentId, (Vec<f32>, usize)> = HashMap::new();
    for e in embeddings {
        let entry = sums
            .entry(e.document)
            .or_insert_with(|| (vec![0.0; e.vector.len()], 0));
        if entry.0.len() != e.vector.len() {
            continue; // stray vector from another space: skip, never mix
        }
        for (slot, v) in entry.0.iter_mut().zip(&e.vector) {
            *slot += v;
        }
        entry.1 += 1;
    }
    let centroids: Vec<(DocumentId, Vec<f32>)> = sums
        .into_iter()
        .map(|(doc, (sum, n))| {
            let n = n.max(1) as f32;
            (doc, sum.into_iter().map(|v| v / n).collect())
        })
        .collect();

    let mut pairs = Vec::new();
    for i in 0..centroids.len() {
        for j in (i + 1)..centroids.len() {
            if let Some(score) = cosine_similarity(&centroids[i].1, &centroids[j].1) {
                if score >= min_similarity {
                    pairs.push((centroids[i].0, centroids[j].0, score));
                }
            }
        }
    }
    pairs.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then_with(|| (a.0, a.1).cmp(&(b.0, b.1)))
    });
    pairs.truncate(max_pairs);
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use marius_model::{ContentHash, EmbeddingModel};
    use time::OffsetDateTime;

    fn embedding(document: DocumentId, vector: Vec<f32>) -> ChunkEmbedding {
        ChunkEmbedding {
            chunk: ChunkId::new(),
            document,
            model: EmbeddingModel::new("test", vector.len()),
            source_hash: ContentHash::of(b"x"),
            vector,
            computed_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn ranking_orders_by_similarity_and_skips_misfits() {
        let a = DocumentId::new();
        let b = DocumentId::new();
        let stored = vec![
            embedding(a, vec![0.0, 1.0]),
            embedding(b, vec![1.0, 0.0]),
            embedding(b, vec![0.5, 0.5, 0.5]), // wrong dimension: skipped
        ];
        let ranked = rank(&[1.0, 0.05], &stored);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].document, b);
        assert!(ranked[0].score > 0.9);
    }

    #[test]
    fn one_result_per_document_keeps_the_best_chunk() {
        let a = DocumentId::new();
        let b = DocumentId::new();
        let stored = vec![
            embedding(a, vec![1.0, 0.0]),
            embedding(a, vec![0.9, 0.1]),
            embedding(b, vec![0.0, 1.0]),
        ];
        let best = best_per_document(&rank(&[1.0, 0.0], &stored));
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].document, a);
        assert_eq!(best[0].score, 1.0, "the better of a's two chunks");
    }

    #[test]
    fn fusion_rewards_agreement_between_rankings() {
        let a = DocumentId::new(); // top of keyword, missing from semantic
        let b = DocumentId::new(); // mid in both
        let c = DocumentId::new(); // top of semantic, missing from keyword
        let keyword = [a, b];
        let semantic = [c, b];
        let fused = fuse_reciprocal_rank(&[&keyword, &semantic], 60.0);
        assert_eq!(fused[0].0, b, "agreement beats one first place");
        // The two single-list items tie exactly; first appearance wins.
        assert_eq!(fused[1].0, a);
        assert_eq!(fused[2].0, c);
    }

    #[test]
    fn similar_pairs_rank_by_centroid_closeness() {
        let a = DocumentId::new();
        let b = DocumentId::new();
        let c = DocumentId::new();
        let stored = vec![
            embedding(a, vec![1.0, 0.0]),
            embedding(a, vec![0.9, 0.1]),
            embedding(b, vec![1.0, 0.05]), // near a
            embedding(c, vec![0.0, 1.0]),  // far from both
        ];
        let pairs = similar_document_pairs(&stored, 0.9, 10);
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        let (x, y, score) = pairs[0];
        assert!(score > 0.99);
        let mut got = [x, y];
        got.sort();
        let mut want = [a, b];
        want.sort();
        assert_eq!(got, want);

        // The floor and the cap both bite.
        assert_eq!(similar_document_pairs(&stored, 0.2, 1).len(), 1);
    }

    #[test]
    fn fusion_of_empty_lists_is_empty() {
        assert!(fuse_reciprocal_rank(&[], 60.0).is_empty());
        let empty: [DocumentId; 0] = [];
        assert!(fuse_reciprocal_rank(&[&empty, &empty], 60.0).is_empty());
    }
}
