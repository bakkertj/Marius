//! Running a parsed query against the index.
//!
//! Requirements: SRCH-001 .. SRCH-006, SRCH-004 (**one global query across all
//! accessible vaults**), SRCH-005 (**every result shows its originating vault
//! and document identity**), TAG-003, KB-012, AI-012, NFR-004.
//!
//! ## How the two halves are combined
//!
//! Full-text relevance lives in FTS5; tag logic, dates, paths and document
//! state live in ordinary tables. Rather than forcing both into one SQL
//! statement, a query takes whichever side is selective as its candidate set
//! and filters the other in memory:
//!
//! * with text terms, FTS5 produces ranked candidates which are then filtered;
//! * without them, the structured filters produce candidates ordered by recency.
//!
//! The cost is that a text query whose candidates are mostly filtered out can
//! return fewer than `limit` results even though more exist further down the
//! ranking. [`SearchOptions::scan_limit`] bounds that window, and
//! [`SearchResults::truncated`] says plainly when it was reached, so the caller
//! can offer "search more" instead of quietly under-reporting.

use std::collections::BTreeSet;

use marius_model::{Document, DocumentId, TagName, Vault, VaultId};
use marius_search::Query;
use marius_storage::{documents, search_index, tags, Connection, Result as StorageResult};

/// One result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub document: Document,
    /// Name of the vault the document belongs to. SRCH-005 requires this on
    /// every result, because a global search spans vaults and a bare title is
    /// ambiguous across them.
    pub vault_name: String,
    /// Highlighted excerpt, when the query had text terms (SRCH-003).
    pub snippet: Option<String>,
    /// Relevance, higher is better. Zero for a query with no text terms.
    pub score: f64,
    pub tags: Vec<TagName>,
}

impl SearchHit {
    /// Unambiguous reference for display and for scripting (SRCH-005).
    pub fn reference(&self) -> String {
        format!("{}:{}", self.vault_name, self.document.path)
    }
}

/// A page of results.
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    /// True when the candidate window was exhausted, so more matches may exist.
    pub truncated: bool,
    /// Candidates examined, for diagnostics.
    pub examined: usize,
}

/// How to run the search.
#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub limit: usize,
    pub offset: usize,
    /// Upper bound on full-text candidates fetched before filtering. Large
    /// enough that ordinary queries are unaffected, small enough to stay inside
    /// NFR-004's 200 ms budget.
    pub scan_limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
            scan_limit: 2_000,
        }
    }
}

/// Run `query` over `vaults`.
pub fn execute(
    conn: &Connection,
    query: &Query,
    vaults: &[Vault],
    options: SearchOptions,
) -> StorageResult<SearchResults> {
    let scope = vault_scope(query, vaults);
    if scope.is_empty() {
        return Ok(SearchResults::default());
    }
    let scope_ids: Vec<VaultId> = scope.iter().map(|v| v.id).collect();

    // Tag constraints become an explicit allow/deny set of document ids, which
    // is cheap to intersect with either candidate source (TAG-003).
    let tag_filter = build_tag_filter(conn, query, &scope_ids)?;

    let mut results = SearchResults::default();
    let wanted = options.offset + options.limit;

    if query.needs_full_text() {
        let expression = query.fts.as_deref().unwrap_or_default();
        let candidates = search_index::matching_seqs(conn, expression, options.scan_limit)?;
        results.examined = candidates.len();
        results.truncated = candidates.len() >= options.scan_limit;

        for (seq, rank) in candidates {
            let Some(document) = documents::by_seq(conn, seq)? else {
                continue;
            };
            if !keep(&document, query, &scope_ids, &tag_filter) {
                continue;
            }
            // Snippets cost one targeted FTS lookup each, so only the rows
            // that survive filtering pay for one.
            let snippet = search_index::snippet_for(conn, expression, seq)?;
            // bm25 reports lower-is-better; present it the other way up.
            results
                .hits
                .push(build_hit(document, &scope, snippet, -rank));
            if results.hits.len() >= wanted {
                break;
            }
        }
    } else {
        let filter = documents::Filter {
            vaults: scope_ids.clone(),
            folder: None,
            kinds: query.kinds.clone(),
            exclude_archived: query.archived == Some(false),
            exclude_generated: query.generated == Some(false),
            modified_after: query.modified_after,
            modified_before: query.modified_before,
            limit: Some(options.scan_limit),
        };
        let candidates = documents::list(conn, &filter)?;
        results.examined = candidates.len();
        results.truncated = candidates.len() >= options.scan_limit;
        for document in candidates {
            if !keep(&document, query, &scope_ids, &tag_filter) {
                continue;
            }
            results.hits.push(build_hit(document, &scope, None, 0.0));
            if results.hits.len() >= wanted {
                break;
            }
        }
    }

    if options.offset > 0 {
        results.hits.drain(..options.offset.min(results.hits.len()));
    }
    results.hits.truncate(options.limit);
    attach_tags(conn, &mut results.hits)?;
    Ok(results)
}

/// Which vaults a query covers.
///
/// With no `vault:` filter the answer is every *available* vault — SRCH-004's
/// global search. An unavailable vault is excluded from results while its data
/// and its graph edges stay intact (SYNC-012).
fn vault_scope(query: &Query, vaults: &[Vault]) -> Vec<Vault> {
    vaults
        .iter()
        .filter(|vault| vault.available)
        .filter(|vault| {
            query.vaults.is_empty()
                || query.vaults.iter().any(|wanted| {
                    vault.name.eq_ignore_ascii_case(wanted) || vault.id.to_string() == *wanted
                })
        })
        .cloned()
        .collect()
}

/// Document ids permitted and forbidden by the query's tag terms.
struct TagFilter {
    required: Option<BTreeSet<DocumentId>>,
    forbidden: BTreeSet<DocumentId>,
}

impl TagFilter {
    fn permits(&self, id: DocumentId) -> bool {
        if self.forbidden.contains(&id) {
            return false;
        }
        match &self.required {
            Some(allowed) => allowed.contains(&id),
            None => true,
        }
    }
}

fn build_tag_filter(
    conn: &Connection,
    query: &Query,
    scope: &[VaultId],
) -> StorageResult<TagFilter> {
    let mut filter = TagFilter {
        required: None,
        forbidden: BTreeSet::new(),
    };
    if query.all_tags.is_empty() && query.any_tags.is_empty() && query.none_tags.is_empty() {
        return Ok(filter);
    }

    // A tag name may exist separately in each vault, so it is resolved per
    // vault and the results unioned.
    let mut tag_query = tags::TagQuery {
        include_descendants: true,
        ..Default::default()
    };
    for vault in scope {
        tag_query
            .all
            .extend(resolve(conn, *vault, &query.all_tags)?);
        tag_query
            .any
            .extend(resolve(conn, *vault, &query.any_tags)?);
        tag_query
            .none
            .extend(resolve(conn, *vault, &query.none_tags)?);
    }

    // A requested tag that exists nowhere means nothing can match.
    if !query.all_tags.is_empty() && tag_query.all.is_empty() {
        filter.required = Some(BTreeSet::new());
        return Ok(filter);
    }
    if !query.any_tags.is_empty() && tag_query.any.is_empty() {
        filter.required = Some(BTreeSet::new());
        return Ok(filter);
    }

    // `all` across vaults would become an impossible intersection, since no
    // document carries another vault's tag. Only `any` is unioned; `all` is
    // evaluated per vault and the results combined.
    let mut required: BTreeSet<DocumentId> = BTreeSet::new();
    if query.all_tags.is_empty() && query.any_tags.is_empty() {
        filter.required = None;
    } else {
        for vault in scope {
            let per_vault = tags::TagQuery {
                all: resolve(conn, *vault, &query.all_tags)?,
                any: resolve(conn, *vault, &query.any_tags)?,
                none: Vec::new(),
                include_descendants: true,
            };
            if per_vault.is_empty() {
                continue;
            }
            required.extend(tags::documents_matching(conn, &[*vault], &per_vault)?);
        }
        filter.required = Some(required);
    }

    if !tag_query.none.is_empty() {
        let excluded = tags::TagQuery {
            any: tag_query.none.clone(),
            include_descendants: true,
            ..Default::default()
        };
        filter
            .forbidden
            .extend(tags::documents_matching(conn, scope, &excluded)?);
    }
    Ok(filter)
}

fn resolve(
    conn: &Connection,
    vault: VaultId,
    names: &[String],
) -> StorageResult<Vec<marius_model::TagId>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let Ok(parsed) = TagName::new(name) else {
            continue;
        };
        if let Some(tag) = tags::by_name(conn, Some(vault), &parsed)? {
            out.push(tag.id);
        }
    }
    Ok(out)
}

fn keep(document: &Document, query: &Query, scope: &[VaultId], tag_filter: &TagFilter) -> bool {
    if !scope.contains(&document.vault) {
        return false;
    }
    if !tag_filter.permits(document.id) {
        return false;
    }
    if !query.kinds.is_empty() && !query.kinds.contains(&document.kind) {
        return false;
    }
    // KB-012: archived content stays in results unless explicitly filtered.
    match query.archived {
        Some(true) if !document.is_archived() => return false,
        Some(false) if document.is_archived() => return false,
        _ => {}
    }
    // AI-012: generated notes are visible but filterable.
    let generated = document.authorship == marius_model::Authorship::Derived;
    match query.generated {
        Some(true) if !generated => return false,
        Some(false) if generated => return false,
        _ => {}
    }
    if !query.paths.is_empty() {
        let path = document.path.as_str();
        let under_any = query.paths.iter().any(|prefix| {
            let prefix = prefix.trim_end_matches('/');
            path == prefix
                || path.starts_with(&format!("{prefix}/"))
                || path.contains(&format!("/{prefix}/"))
        });
        if !under_any {
            return false;
        }
    }
    if let Some(after) = query.modified_after {
        if document.modified_at <= after {
            return false;
        }
    }
    if let Some(before) = query.modified_before {
        if document.modified_at >= before {
            return false;
        }
    }
    if let Some(after) = query.created_after {
        if document.created_at <= after {
            return false;
        }
    }
    if let Some(before) = query.created_before {
        if document.created_at >= before {
            return false;
        }
    }
    true
}

fn build_hit(
    document: Document,
    scope: &[Vault],
    snippet: Option<String>,
    score: f64,
) -> SearchHit {
    let vault_name = scope
        .iter()
        .find(|v| v.id == document.vault)
        .map(|v| v.name.clone())
        .unwrap_or_else(|| document.vault.to_string());
    SearchHit {
        document,
        vault_name,
        snippet,
        score,
        // Filled in for the whole page at once; see `attach_tags`.
        tags: Vec::new(),
    }
}

/// Fill in the tags for a whole page in one query.
///
/// Done after truncation, so a page of twenty costs one query rather than
/// twenty — and a scan that examined ten thousand candidates does not pay for
/// the ones it discarded.
fn attach_tags(conn: &Connection, hits: &mut [SearchHit]) -> StorageResult<()> {
    if hits.is_empty() {
        return Ok(());
    }
    let ids: Vec<_> = hits.iter().map(|hit| hit.document.id).collect();
    let mut by_document = tags::names_for_documents(conn, &ids)?;
    for hit in hits {
        hit.tags = by_document.remove(&hit.document.id).unwrap_or_default();
    }
    Ok(())
}
