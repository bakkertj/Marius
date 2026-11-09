//! # The privacy and sensitivity policy engine
//!
//! Decides, for every inference request, which providers are eligible — and
//! refuses rather than falling back to an ineligible one.
//!
//! Requirements: ARCH-016, AI-005 .. AI-008, SEC-002, AC-016.
//!
//! Not yet implemented. Every rule it will enforce is already implemented and
//! tested in `marius_model::policy`: `Sensitivity` is ordered and combines by
//! taking the maximum, so one confidential source restricts a whole request;
//! unknown sensitivity defaults conservatively; a document override may only
//! tighten its vault's floor. What remains is the decision procedure over
//! those rules plus the audit record, which `marius_model::InferenceRecord`
//! and the `inference_record` table already define — including the ability to
//! record a refusal, which is the evidence a user needs most.
//!
//! See `docs/STATUS.md` for what is built and `docs/TRACEABILITY.md` for the
//! per-requirement position.

#![forbid(unsafe_code)]
