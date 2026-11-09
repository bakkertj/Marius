//! # Embedding generation and vector retrieval
//!
//! Turns passages into vectors and answers similarity queries.
//!
//! Requirements: SRCH-007, SRCH-009, SRCH-013.
//!
//! Not yet implemented. The storage and invalidation rules are:
//! `marius-markdown`'s `chunk` module produces passages whose spans quote the
//! document exactly, `marius_storage::chunks` stores vectors keyed by
//! embedding space so two models' vectors are never compared, and
//! `stale_chunks` finds passages whose vector was computed from a different
//! content hash. `marius_model::cosine_similarity` is the comparison. What is
//! missing is the client that calls a model, and the hybrid fusion that
//! combines vector scores with BM25 relevance.
//!
//! See `docs/STATUS.md` for what is built and `docs/TRACEABILITY.md` for the
//! per-requirement position.

#![forbid(unsafe_code)]
