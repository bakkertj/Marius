//! # Automatic tagging and relationship discovery
//!
//! Proposes tags and semantic relationships using a configured model.
//!
//! Requirements: TAG-005, TAG-013, TAG-016, GRAPH-007, AI-010, AI-018.
//!
//! Not yet implemented. Everything that protects the user from it is:
//! `marius_storage::tags::assign` refuses to overwrite a manual, imported,
//! confirmed or locked assignment; `replace_machine_tags` clears only the
//! classifier's own unreviewed rows and leaves the indexer's inline tags
//! alone; rejections are durable records that survive reindexing and sync.
//! `tags::ensure` routes a proposal to an existing canonical tag or alias
//! rather than minting a near-duplicate (TAG-008).
//!
//! See `docs/STATUS.md` for what is built and `docs/TRACEABILITY.md` for the
//! per-requirement position.

#![forbid(unsafe_code)]
