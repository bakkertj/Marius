//! # Graph assembly and layout input
//!
//! Builds the node-and-edge views the UI renders: local neighbourhoods, global
//! graphs, tag-bipartite mode, clustering and progressive rendering budgets.
//!
//! Requirements: GRAPH-001, GRAPH-004 .. GRAPH-006, GRAPH-013 .. GRAPH-016.
//!
//! Not yet implemented. The data it needs already exists: `marius-storage`'s
//! `relationships` module serves stored edges with kind filters and limits and
//! computes shared-tag kinship on demand, and `marius-core`'s `neighbourhood`
//! assembles a local view. This crate is where layout-facing aggregation will
//! live once there is a renderer to aggregate for.
//!
//! See `docs/STATUS.md` for what is built and `docs/TRACEABILITY.md` for the
//! per-requirement position.

#![forbid(unsafe_code)]
