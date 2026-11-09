//! # Model provider adapters
//!
//! Interchangeable clients for OpenRouter, OpenAI-compatible endpoints and
//! locally hosted models such as Ollama.
//!
//! Requirements: AI-001, AI-002, AI-003, AI-016.
//!
//! Not yet implemented, and deliberately not implemented before
//! `marius-llm-policy`: an adapter that can reach the network before anything
//! decides whether it may is the wrong order to build these two in. Model
//! selection is already expressed in `marius_core::config::ModelSettings`,
//! keyed by `marius_model::InferenceTask`, and credentials are referenced by
//! name rather than stored (SEC-001).
//!
//! See `docs/STATUS.md` for what is built and `docs/TRACEABILITY.md` for the
//! per-requirement position.

#![forbid(unsafe_code)]
