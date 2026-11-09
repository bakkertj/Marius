//! The FTS5 full-text index.
//!
//! Requirements: SRCH-001, SRCH-003 (exact phrases, Boolean operators,
//! highlighting and snippets), SRCH-013 (incremental update), NFR-004 (about
//! 200 ms for a 10,000-document vault), integrity rule 3.
//!
//! The index is keyed by `document.seq`, the integer rowid, because FTS5 keys
//! on an integer. Everything in here is derived and rebuildable from the source
//! files, which is what lets `Database::rebuild_derived` throw it away safely.
//!
//! ## Column weighting
//!
//! BM25 is given per-column weights rather than treating a title hit as
//! equivalent to a hit in the middle of a long note. Titles and aliases are the
//! names a person actually remembers, so they dominate; body text is the
//! baseline; a path match is a weak signal; and tags sit in between because a
//! tag is a deliberate human judgement about what a note is *about*.

use rusqlite::{params, Connection};

use crate::Result;

/// Relative importance of each indexed column, in the order the virtual table
/// declares them: title, body, tags, path, aliases.
pub const WEIGHTS: [f64; 5] = [10.0, 1.0, 5.0, 2.0, 8.0];

/// Text to index for one document.
#[derive(Debug, Clone, Default)]
pub struct IndexedText {
    pub title: String,
    pub body: String,
    /// Space-separated canonical tag names.
    pub tags: String,
    pub path: String,
    /// Space-separated aliases.
    pub aliases: String,
}

/// Insert or replace a document's indexed text (SRCH-013).
pub fn index(conn: &Connection, seq: i64, text: &IndexedText) -> Result<()> {
    // FTS5 has no UPSERT, so a replace is a delete followed by an insert.
    remove(conn, seq)?;
    conn.execute(
        "INSERT INTO document_fts (rowid, title, body, tags, path, aliases)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            seq,
            text.title,
            text.body,
            text.tags,
            text.path,
            text.aliases
        ],
    )?;
    Ok(())
}

/// Remove a document from the index.
pub fn remove(conn: &Connection, seq: i64) -> Result<()> {
    conn.execute("DELETE FROM document_fts WHERE rowid = ?1", params![seq])?;
    Ok(())
}

/// One full-text hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Rowid of the matching document; map back with `documents::by_seq`.
    pub seq: i64,
    /// BM25 relevance. **Lower is better**, as FTS5 reports it; callers that
    /// show a score to users should negate it.
    pub rank: f64,
    /// Highlighted excerpt from the best-matching column (SRCH-003).
    pub snippet: String,
}

/// How much text a snippet may contain, in tokens.
const SNIPPET_TOKENS: i32 = 32;

/// Run a prepared FTS5 MATCH expression.
///
/// `match_expr` must already be valid FTS5 syntax — building it from untrusted
/// user input is `marius-search`'s job, so that query parsing and its error
/// messages live in one place.
pub fn search(
    conn: &Connection,
    match_expr: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<Hit>> {
    let sql = format!(
        "SELECT rowid,
                bm25(document_fts, {}, {}, {}, {}, {}) AS rank,
                snippet(document_fts, -1, '{{', '}}', '…', {SNIPPET_TOKENS})
         FROM document_fts
         WHERE document_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2 OFFSET ?3",
        WEIGHTS[0], WEIGHTS[1], WEIGHTS[2], WEIGHTS[3], WEIGHTS[4]
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![match_expr, limit as i64, offset as i64], |row| {
            Ok(Hit {
                seq: row.get(0)?,
                rank: row.get(1)?,
                snippet: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Rowids matching an expression, without ranking or snippets.
///
/// Used when full-text is one term of a larger query and the caller will do its
/// own combining (SRCH-009).
pub fn matching_seqs(conn: &Connection, match_expr: &str, limit: usize) -> Result<Vec<(i64, f64)>> {
    let sql = format!(
        "SELECT rowid, bm25(document_fts, {}, {}, {}, {}, {}) AS rank
         FROM document_fts WHERE document_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        WEIGHTS[0], WEIGHTS[1], WEIGHTS[2], WEIGHTS[3], WEIGHTS[4]
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![match_expr, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The highlighted excerpt for one specific document.
///
/// A targeted lookup, because asking for the ranked result list and hoping the
/// row you want is in it only ever works for the top hit.
pub fn snippet_for(conn: &Connection, match_expr: &str, seq: i64) -> Result<Option<String>> {
    let sql = format!(
        "SELECT snippet(document_fts, -1, '{{', '}}', '…', {SNIPPET_TOKENS})
         FROM document_fts WHERE rowid = ?2 AND document_fts MATCH ?1"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query_map(params![match_expr, seq], |row| row.get::<_, String>(0))?;
    Ok(rows.next().transpose()?)
}

/// How many documents are indexed.
pub fn count(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM document_fts", [], |row| row.get(0))?;
    Ok(n as u64)
}

/// Merge the index's b-tree segments. Worth doing after a bulk reindex.
pub fn optimize(conn: &Connection) -> Result<()> {
    conn.execute_batch("INSERT INTO document_fts(document_fts) VALUES('optimize')")?;
    Ok(())
}

/// Quote a string as a single FTS5 phrase.
///
/// FTS5 treats a bare `-`, `*`, `"`, `(`, `)`, `:` and `^` as syntax, so any
/// text coming from a user, a filename or a tag has to be quoted before it
/// reaches a MATCH expression. Doubling the inner quotes is FTS5's own escape.
pub fn quote_phrase(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn index_note(db: &Database, seq: i64, title: &str, body: &str) {
        index(
            db.conn(),
            seq,
            &IndexedText {
                title: title.into(),
                body: body.into(),
                path: format!("notes/{title}.md"),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn indexed_text_is_findable_by_word() {
        // SRCH-001.
        let db = db();
        index_note(
            &db,
            1,
            "Pharos",
            "It stood at the harbour mouth of Alexandria.",
        );
        let hits = search(db.conn(), "harbour", 10, 0).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 1);
    }

    #[test]
    fn snippets_highlight_the_match() {
        // SRCH-003.
        let db = db();
        index_note(
            &db,
            1,
            "Pharos",
            "It stood at the harbour mouth of Alexandria.",
        );
        let hits = search(db.conn(), "harbour", 10, 0).unwrap();
        assert!(
            hits[0].snippet.contains("{harbour}"),
            "{:?}",
            hits[0].snippet
        );
    }

    #[test]
    fn phrases_match_only_adjacent_words() {
        // SRCH-003.
        let db = db();
        index_note(&db, 1, "A", "the harbour mouth");
        index_note(&db, 2, "B", "the mouth of the harbour");

        let phrase = search(db.conn(), "\"harbour mouth\"", 10, 0).unwrap();
        assert_eq!(phrase.len(), 1);
        assert_eq!(phrase[0].seq, 1);

        let both = search(db.conn(), "harbour AND mouth", 10, 0).unwrap();
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn boolean_operators_work() {
        // SRCH-003.
        let db = db();
        index_note(&db, 1, "A", "lighthouse history");
        index_note(&db, 2, "B", "lighthouse engineering");

        assert_eq!(search(db.conn(), "lighthouse", 10, 0).unwrap().len(), 2);
        assert_eq!(
            search(db.conn(), "lighthouse AND history", 10, 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search(db.conn(), "lighthouse NOT history", 10, 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search(db.conn(), "history OR engineering", 10, 0)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_title_match_outranks_a_body_match() {
        // The weighting decision, verified rather than assumed.
        let db = db();
        index(
            db.conn(),
            1,
            &IndexedText {
                title: "Lighthouse".into(),
                body: "Unrelated prose about harbours and ships.".into(),
                ..Default::default()
            },
        )
        .unwrap();
        index(
            db.conn(),
            2,
            &IndexedText {
                title: "Something else".into(),
                body: "A passing mention of a lighthouse somewhere in the text.".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let hits = search(db.conn(), "lighthouse", 10, 0).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].seq, 1, "the titled note must come first");
        assert!(hits[0].rank < hits[1].rank, "bm25: lower is better");
    }

    #[test]
    fn an_alias_match_outranks_a_body_match() {
        let db = db();
        index(
            db.conn(),
            1,
            &IndexedText {
                title: "Alexandria".into(),
                aliases: "Pharos".into(),
                body: "nothing else".into(),
                ..Default::default()
            },
        )
        .unwrap();
        index(
            db.conn(),
            2,
            &IndexedText {
                body: "a stray mention of Pharos".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let hits = search(db.conn(), "pharos", 10, 0).unwrap();
        assert_eq!(hits[0].seq, 1);
    }

    #[test]
    fn tags_and_paths_are_searchable() {
        // SRCH-002.
        let db = db();
        index(
            db.conn(),
            1,
            &IndexedText {
                title: "Note".into(),
                tags: "history lighthouse".into(),
                path: "research/ancient/note.md".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            search(db.conn(), "tags:lighthouse", 10, 0).unwrap().len(),
            1
        );
        assert_eq!(search(db.conn(), "path:ancient", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn reindexing_replaces_rather_than_duplicates() {
        // SRCH-013.
        let db = db();
        index_note(&db, 1, "Pharos", "original text");
        index_note(&db, 1, "Pharos", "replacement text");

        assert_eq!(count(db.conn()).unwrap(), 1);
        assert!(search(db.conn(), "original", 10, 0).unwrap().is_empty());
        assert_eq!(search(db.conn(), "replacement", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn removing_a_document_removes_it_from_results() {
        let db = db();
        index_note(&db, 1, "Pharos", "harbour");
        remove(db.conn(), 1).unwrap();
        assert!(search(db.conn(), "harbour", 10, 0).unwrap().is_empty());
        assert_eq!(count(db.conn()).unwrap(), 0);
    }

    #[test]
    fn diacritics_are_folded_so_searches_match_either_spelling() {
        let db = db();
        index_note(&db, 1, "Pharos", "the Φάρος at Alexandrie with a café");
        assert_eq!(search(db.conn(), "cafe", 10, 0).unwrap().len(), 1);
        assert_eq!(search(db.conn(), "café", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn prefix_search_works_for_as_you_type_queries() {
        let db = db();
        index_note(&db, 1, "Lighthouse", "illumination");
        assert_eq!(search(db.conn(), "light*", 10, 0).unwrap().len(), 1);
        assert_eq!(search(db.conn(), "illum*", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn results_paginate() {
        let db = db();
        for seq in 1..=10 {
            index_note(&db, seq, &format!("Note{seq}"), "shared term");
        }
        let first = search(db.conn(), "shared", 4, 0).unwrap();
        let second = search(db.conn(), "shared", 4, 4).unwrap();
        assert_eq!(first.len(), 4);
        assert_eq!(second.len(), 4);
        let overlap = first
            .iter()
            .filter(|h| second.iter().any(|s| s.seq == h.seq))
            .count();
        assert_eq!(overlap, 0, "pages must not repeat rows");
    }

    #[test]
    fn text_with_fts_syntax_is_safely_quotable() {
        // A tag or filename containing `-`, `*` or `"` must not become syntax.
        let db = db();
        index_note(
            &db,
            1,
            "Odd",
            "a term with-a-hyphen and a \"quoted\" word and C++",
        );

        for raw in ["with-a-hyphen", "\"quoted\"", "C++"] {
            let expr = quote_phrase(raw);
            let hits = search(db.conn(), &expr, 10, 0)
                .unwrap_or_else(|e| panic!("{raw:?} -> {expr}: {e}"));
            assert_eq!(hits.len(), 1, "{raw:?} via {expr}");
        }
    }

    #[test]
    fn a_snippet_can_be_fetched_for_any_matching_row_not_just_the_best() {
        let db = db();
        index_note(&db, 1, "Best", "harbour harbour harbour");
        index_note(&db, 2, "Weaker", "a single harbour mention");

        let ranked = search(db.conn(), "harbour", 10, 0).unwrap();
        assert_eq!(ranked[0].seq, 1, "row 1 ranks first");

        // The weaker row still yields its own snippet.
        let snippet = snippet_for(db.conn(), "harbour", 2).unwrap().unwrap();
        assert!(snippet.contains("{harbour}"), "{snippet}");
        assert!(snippet.contains("mention"), "{snippet}");

        // And a non-matching row yields nothing.
        index_note(&db, 3, "Other", "nothing relevant");
        assert_eq!(snippet_for(db.conn(), "harbour", 3).unwrap(), None);
    }

    #[test]
    fn an_unquoted_fts_syntax_error_is_reported_not_swallowed() {
        let db = db();
        index_note(&db, 1, "A", "text");
        // A lone `"` is invalid FTS5; the caller must learn about it.
        assert!(search(db.conn(), "\"", 10, 0).is_err());
    }

    #[test]
    fn matching_seqs_returns_rowids_and_ranks_for_hybrid_combining() {
        // SRCH-009.
        let db = db();
        index_note(&db, 1, "A", "harbour");
        index_note(&db, 2, "B", "harbour harbour");
        let hits = matching_seqs(db.conn(), "harbour", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].1 <= hits[1].1);
    }

    #[test]
    fn a_rebuild_clears_the_index_but_keeps_authoritative_data() {
        // NFR-007, integrity rule 3.
        let mut db = db();
        index_note(&db, 1, "Pharos", "harbour");
        db.conn()
            .execute(
                "INSERT INTO saved_search (name, query, created_at) VALUES ('q', 'harbour', 'now')",
                [],
            )
            .unwrap();

        db.rebuild_derived().unwrap();
        assert_eq!(count(db.conn()).unwrap(), 0);
        let saved: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM saved_search", [], |row| row.get(0))
            .unwrap();
        assert_eq!(saved, 1, "authoritative rows must survive a rebuild");
    }

    #[test]
    fn optimize_is_safe_on_an_empty_index() {
        let db = db();
        optimize(db.conn()).unwrap();
    }
}
