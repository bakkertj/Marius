//! # Importers for formats that are not Markdown
//!
//! Extracts searchable text and metadata from PDF, HTML, Word, `.eml` and
//! `.mbox` sources.
//!
//! Requirements: IM-008, IM-009, IM-011, IM-012.
//!
//! Not yet implemented. Markdown, plain text and pasted email already work
//! without an importer, because a vault is opened in place rather than
//! converted (KB-002, AC-001, AC-002) — which is also why there is no
//! "import an Obsidian vault" step to write. `marius_model::DocumentKind`
//! already distinguishes the kinds, and `marius-indexing` already indexes
//! non-text files by name and metadata without reading them (KB-013).
//!
//! See `docs/STATUS.md` for what is built and `docs/TRACEABILITY.md` for the
//! per-requirement position.

#![forbid(unsafe_code)]
