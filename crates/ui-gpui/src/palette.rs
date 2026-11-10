//! The command palette's matcher (UI-006): a thin re-export of the
//! shared fuzzy ranking in `marius-search`, so the quick switcher and
//! the palette agree about what matches.

pub use marius_search::fuzzy::{rank, score};
