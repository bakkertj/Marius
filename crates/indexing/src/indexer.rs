//! The indexing pipeline: one file in, one fully indexed document out.
//!
//! Requirements: KB-005, KB-006, KB-007, KB-010, KB-011, MD-006, MD-008,
//! TAG-004, SRCH-001, SRCH-008, SRCH-013, GRAPH-002, AI-004, NFR-002, NFR-006,
//! AC-001, AC-003, integrity rules 1–4.
//!
//! ## Ownership, again
//!
//! The indexer reads files and owns exactly the metadata that is *derived from
//! a file*: the document record, its inline and frontmatter tags, the edges its
//! links imply, its indexed text, its passages. It never touches a model's
//! proposals, a person's manual tags, a confirmed edge or a rejection — that is
//! why `marius-storage` offers `replace_file_tags` and `replace_link_edges`
//! rather than a single "replace everything" call (TAG-004 with TAG-006,
//! GRAPH-002 with GRAPH-012).
//!
//! ## It never writes to a source document
//!
//! Nothing in this module modifies the user's file. Reading is the whole job
//! (ARCH-005, ARCH-012). The two sanctioned write paths — rewriting links when
//! a document is renamed, and an authorised frontmatter writeback — live in
//! `marius-core`, where a user action can be attributed to them.
//!
//! ## Resumability
//!
//! Work is committed in batches (`IndexOptions::batch_size`), so an
//! interrupted index of a large vault keeps everything it finished and the next
//! run reconciles only what is left (NFR-006). A file that cannot be read is
//! recorded as a problem and the pass continues: one unreadable note must not
//! prevent the other nine thousand from being searchable (AI-004's principle,
//! applied to the indexer itself).

use std::collections::BTreeSet;
use std::time::Instant;

use marius_markdown::{ChunkConfig, ParsedDocument, TagOrigin, TextOptions};
use marius_model::{
    Authorship, Document, DocumentId, DocumentKind, DocumentTag, Endpoints, Relationship,
    RelationshipKind, RevisionCause, Sensitivity, TagName, TagSource, Vault, VaultId, VaultPath,
};
use marius_storage::search_index::IndexedText;
use marius_storage::{
    chunks, documents, relationships, revisions, search_index, tags, Connection, Database,
    StorageError,
};
use time::OffsetDateTime;

use crate::files::{self, FileError};
use crate::reconcile::{self, Change, Plan, Thoroughness};
use crate::scan::{self, ScanOptions, ScannedFile};

/// Anything that can stop the indexer.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    File(#[from] FileError),
}

type Result<T> = std::result::Result<T, IndexError>;

/// How to index.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub scan: ScanOptions,
    pub text: TextOptions,
    /// Produce passages for semantic retrieval. `None` skips chunking, which is
    /// the right default until an embedding model is configured (SRCH-007).
    pub chunking: Option<ChunkConfig>,
    /// Record a revision for each change (KB-011).
    pub record_revisions: bool,
    /// Keep the body of a recorded revision, up to this size. Bodies are what
    /// make recovery possible; the limit stops a vault of large notes from
    /// doubling the database.
    pub revision_body_limit: u64,
    /// Further vaults to resolve `[[links]]` against (GRAPH-008).
    pub link_vaults: Vec<VaultId>,
    /// Documents per transaction. Smaller is more resumable, larger is faster.
    pub batch_size: usize,
    pub thoroughness: Thoroughness,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            scan: ScanOptions::default(),
            text: TextOptions::default(),
            chunking: None,
            record_revisions: true,
            revision_body_limit: 256 * 1024,
            link_vaults: Vec::new(),
            batch_size: 64,
            thoroughness: Thoroughness::default(),
        }
    }
}

/// What an indexing pass did.
#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    pub added: usize,
    pub modified: usize,
    pub moved: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// Links completed because their target now exists (MD-007).
    pub links_completed: usize,
    /// Links still pointing at nothing, for a broken-link report.
    pub links_unresolved: usize,
    /// Files that could not be indexed, with why (NFR-006).
    pub problems: Vec<(String, String)>,
    pub elapsed_ms: u128,
}

impl IndexReport {
    /// Documents whose index entry was written.
    pub fn touched(&self) -> usize {
        self.added + self.modified + self.moved
    }

    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} added, {} modified, {} moved, {} removed, {} unchanged in {} ms",
            self.added, self.modified, self.moved, self.removed, self.unchanged, self.elapsed_ms
        );
        if self.links_unresolved > 0 {
            out.push_str(&format!("; {} unresolved links", self.links_unresolved));
        }
        if !self.problems.is_empty() {
            out.push_str(&format!("; {} problems", self.problems.len()));
        }
        out
    }
}

/// Scan a vault and bring the index up to date.
pub fn index_vault(
    db: &mut Database,
    vault: &Vault,
    options: &IndexOptions,
) -> Result<IndexReport> {
    let started = Instant::now();
    let scanned = scan::scan(&vault.root, &options.scan);
    let mut report = IndexReport::default();
    for (path, problem) in &scanned.problems {
        report
            .problems
            .push((path.to_string_lossy().into_owned(), problem.clone()));
    }

    let plan = reconcile::plan_with(db.conn(), vault.id, &scanned.files, options.thoroughness)?;
    report.unchanged = plan.unchanged;
    for (path, problem) in &plan.problems {
        report.problems.push((path.to_string(), problem.clone()));
    }

    apply(db, vault, plan, options, &mut report)?;

    // Links that pointed at nothing may now resolve, because their target was
    // created in this very pass (MD-007).
    let tx = db.transaction()?;
    report.links_completed = resolve_pending_links(&tx, vault, options)?;
    marius_storage::commit(tx)?;
    // `links_unresolved` was counted while indexing, before the sweep.
    report.links_unresolved = report
        .links_unresolved
        .saturating_sub(report.links_completed);

    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

/// Apply a reconciliation plan.
///
/// Durability is lowered for the duration: everything being written is derived
/// from files that are still on disk, and the pass is resumable, so paying for
/// a flush per batch buys nothing (see `Database::with_bulk_durability`).
pub fn apply(
    db: &mut Database,
    vault: &Vault,
    plan: Plan,
    options: &IndexOptions,
    report: &mut IndexReport,
) -> Result<()> {
    if plan.is_empty() {
        return Ok(());
    }
    db.with_bulk_durability(|db| {
        for batch in plan.changes.chunks(options.batch_size.max(1)) {
            // One transaction per batch: an interruption keeps the batches that
            // finished, and the next run reconciles the rest (NFR-006).
            let tx = db.transaction()?;
            for change in batch {
                if let Err(error) = apply_one(&tx, vault, change, options, report) {
                    report
                        .problems
                        .push((change_path(change), error.to_string()));
                }
            }
            tx.commit()?;
        }
        Ok(())
    })
    .map_err(IndexError::Storage)
}

fn change_path(change: &Change) -> String {
    match change {
        Change::Added { file, .. } | Change::Modified { file, .. } | Change::Moved { file, .. } => {
            file.path.to_string()
        }
        Change::Removed { document } => document.path.to_string(),
    }
}

fn apply_one(
    conn: &Connection,
    vault: &Vault,
    change: &Change,
    options: &IndexOptions,
    report: &mut IndexReport,
) -> Result<()> {
    match change {
        Change::Added { file, .. } => {
            index_file(conn, vault, file, None, options, report)?;
            report.added += 1;
        }
        Change::Modified { document, file } => {
            index_file(conn, vault, file, Some(document), options, report)?;
            report.modified += 1;
        }
        Change::Moved { document, file } => {
            // The identity is kept; only the path moves (AC-003).
            documents::relocate(conn, document.id, &file.path)?;
            if options.record_revisions {
                let note = format!("moved from {}", document.path);
                revisions::record(
                    conn,
                    document.id,
                    revisions::RevisionDraft::new(
                        RevisionCause::Relocation,
                        document.content_hash,
                        document.size_bytes,
                        file.path.as_str(),
                    )
                    .with_note(&note),
                )?;
            }
            let mut moved = document.clone();
            moved.path = file.path.clone();
            index_file(conn, vault, file, Some(&moved), options, report)?;
            report.moved += 1;
        }
        Change::Removed { document } => {
            // The rowid has to be read *before* the row goes, or the search
            // index keeps an orphaned entry.
            if let Some(seq) = documents::seq_of(conn, document.id)? {
                search_index::remove(conn, seq)?;
            }
            documents::delete(conn, document.id)?;
            report.removed += 1;
        }
    }
    Ok(())
}

/// Index one file, creating or updating its document.
pub fn index_file(
    conn: &Connection,
    vault: &Vault,
    file: &ScannedFile,
    existing: Option<&Document>,
    options: &IndexOptions,
    report: &mut IndexReport,
) -> Result<DocumentId> {
    if file.kind == DocumentKind::Attachment {
        return index_attachment(conn, vault, file, existing, options);
    }

    let contents = files::read_text(&file.absolute)?;
    let parsed = ParsedDocument::parse_with(&contents.text, options.text);
    if let Some(problem) = &parsed.frontmatter_error {
        // The document is still indexed; the user is told what was skipped.
        report
            .problems
            .push((file.path.to_string(), format!("frontmatter: {problem}")));
    }

    let document = build_document(vault, file, existing, &parsed, &contents);
    documents::upsert(conn, &document)?;
    documents::set_aliases(conn, document.id, &parsed.aliases)?;

    apply_file_tags(conn, vault, &document, &parsed)?;
    let unresolved = apply_links(conn, vault, &document, &parsed, options)?;
    report.links_unresolved += unresolved;

    refresh_search_index(conn, &document, &parsed)?;

    if let Some(config) = &options.chunking {
        let drafts = parsed.chunks(&contents.text, config);
        let chunk_rows: Vec<_> = drafts
            .into_iter()
            .map(|draft| draft.into_chunk(document.id))
            .collect();
        chunks::replace(conn, document.id, &chunk_rows)?;
    }

    if options.record_revisions && existing.map(|e| e.content_hash) != Some(contents.hash) {
        let cause = if existing.is_some() {
            // The indexer only ever observes the filesystem, so a change it
            // discovers was made outside the application (KB-005).
            RevisionCause::ExternalChange
        } else {
            RevisionCause::Created
        };
        let mut draft = revisions::RevisionDraft::new(
            cause,
            contents.hash,
            contents.size_bytes,
            file.path.as_str(),
        );
        if contents.size_bytes <= options.revision_body_limit {
            draft = draft.with_body(&contents.text);
        }
        revisions::record(conn, document.id, draft)?;
    }

    Ok(document.id)
}

/// Index a non-text file by its metadata only (KB-013, MD-005).
///
/// An attachment is never read into memory or parsed; it exists in the index so
/// that `![[diagram.png]]` resolves and so a filename is searchable.
fn index_attachment(
    conn: &Connection,
    vault: &Vault,
    file: &ScannedFile,
    existing: Option<&Document>,
    options: &IndexOptions,
) -> Result<DocumentId> {
    let (hash, size) = files::hash_file(&file.absolute)?;
    let now = OffsetDateTime::now_utc();
    let document = Document {
        id: existing.map_or_else(DocumentId::new, |e| e.id),
        vault: vault.id,
        path: file.path.clone(),
        title: file.path.file_name().to_owned(),
        kind: DocumentKind::Attachment,
        authorship: Authorship::Source,
        content_hash: hash,
        size_bytes: size,
        created_at: existing.map_or(now, |e| e.created_at),
        modified_at: file.modified,
        indexed_at: now,
        archived_at: existing.and_then(|e| e.archived_at),
        sensitivity: existing.and_then(|e| e.sensitivity),
        frontmatter: existing.map(|e| e.frontmatter.clone()).unwrap_or_default(),
        external: false,
    };
    documents::upsert(conn, &document)?;
    if let Some(seq) = documents::seq_of(conn, document.id)? {
        search_index::index(
            conn,
            seq,
            &IndexedText {
                title: document.title.clone(),
                path: document.path.to_string(),
                ..Default::default()
            },
        )?;
    }
    let _ = options;
    Ok(document.id)
}

fn build_document(
    vault: &Vault,
    file: &ScannedFile,
    existing: Option<&Document>,
    parsed: &ParsedDocument,
    contents: &files::Contents,
) -> Document {
    let now = OffsetDateTime::now_utc();
    // KB-007's fallback chain: frontmatter title, then the first H1, then the
    // filename — which is what the user sees in their file manager.
    let title = parsed
        .title
        .clone()
        .unwrap_or_else(|| file.path.file_stem().to_owned());

    Document {
        id: existing.map_or_else(DocumentId::new, |e| e.id),
        vault: vault.id,
        path: file.path.clone(),
        title,
        // A note the application generated keeps that identity when re-read, so
        // its provenance banner and filters stay correct (AI-012).
        kind: match existing.map(|e| e.kind) {
            Some(DocumentKind::GeneratedNote) => DocumentKind::GeneratedNote,
            _ => file.kind,
        },
        // Authorship is never downgraded by a re-read: a promoted note stays
        // promoted (AI-014), and a source document stays protected.
        authorship: existing.map_or(Authorship::Source, |e| e.authorship),
        content_hash: contents.hash,
        size_bytes: contents.size_bytes,
        created_at: existing.map_or(now, |e| e.created_at),
        modified_at: file.modified,
        indexed_at: now,
        archived_at: existing.and_then(|e| e.archived_at),
        sensitivity: effective_sensitivity(existing, parsed),
        frontmatter: parsed.frontmatter.values.clone(),
        external: existing.is_some_and(|e| e.external),
    }
}

/// Combine a document's recorded sensitivity with any `sensitivity:` key in its
/// frontmatter, taking whichever is stricter.
///
/// AI-007 forbids automatically lowering a privacy restriction, so re-reading a
/// file can tighten a document's classification but never relax it. Relaxing it
/// remains possible, but only as an explicit user action.
fn effective_sensitivity(
    existing: Option<&Document>,
    parsed: &ParsedDocument,
) -> Option<Sensitivity> {
    let declared = parsed
        .frontmatter
        .get("sensitivity")
        .map(marius_model::MetadataValue::to_index_text)
        .and_then(|text| {
            text.trim()
                .to_lowercase()
                .replace([' ', '-'], "_")
                .parse()
                .ok()
        });
    match (existing.and_then(|e| e.sensitivity), declared) {
        (Some(current), Some(declared)) => Some(current.most_restrictive(declared)),
        (Some(current), None) => Some(current),
        (None, Some(declared)) => Some(declared),
        (None, None) => None,
    }
}

/// Apply the tags found in the file, leaving every other assignment alone.
fn apply_file_tags(
    conn: &Connection,
    vault: &Vault,
    document: &Document,
    parsed: &ParsedDocument,
) -> Result<()> {
    let mut proposed = Vec::with_capacity(parsed.tags.len());
    for found in &parsed.tags {
        let Ok(name) = TagName::new(&found.raw) else {
            // A tag that normalises to nothing — `#///` — is not an error, just
            // not a tag.
            continue;
        };
        let tag = tags::ensure(conn, Some(vault.id), &name)?;
        let source = match found.origin {
            TagOrigin::Inline => TagSource::Inline,
            TagOrigin::Frontmatter => TagSource::Frontmatter,
        };
        proposed.push(DocumentTag::derived(document.id, tag.id, source));
    }
    tags::replace_file_tags(conn, document.id, &proposed)?;
    Ok(())
}

/// Turn the document's links into edges, returning how many did not resolve.
fn apply_links(
    conn: &Connection,
    vault: &Vault,
    document: &Document,
    parsed: &ParsedDocument,
    options: &IndexOptions,
) -> Result<usize> {
    let here = document.global_id();
    let mut edges: Vec<Relationship> = Vec::new();
    let mut unresolved: BTreeSet<(String, RelationshipKind)> = BTreeSet::new();

    let mut consider = |target: &str, kind: RelationshipKind| -> Result<()> {
        let resolved = documents::resolve_relative(
            conn,
            vault.id,
            Some(&document.path),
            target,
            &options.link_vaults,
        )?;
        match resolved {
            // A link to itself is not an edge.
            Some((found, _)) if found.document == document.id => {}
            Some((found, _)) => {
                edges.push(Relationship::new(Endpoints::new(here, found), kind));
            }
            None => {
                unresolved.insert((target.to_owned(), kind));
            }
        }
        Ok(())
    };

    for link in &parsed.scan.wiki_links {
        if link.is_internal() {
            continue;
        }
        let kind = if link.is_embed {
            RelationshipKind::Embed
        } else {
            RelationshipKind::Link
        };
        consider(&link.target, kind)?;
    }
    for link in &parsed.scan.markdown_links {
        if link.is_external() || link.target.trim().is_empty() {
            continue;
        }
        let kind = if link.is_image {
            RelationshipKind::Embed
        } else {
            RelationshipKind::Link
        };
        consider(&link.target, kind)?;
    }

    relationships::replace_link_edges(conn, document.id, &edges)?;

    relationships::clear_pending_links(conn, document.id)?;
    for (target, kind) in &unresolved {
        relationships::add_pending_link(conn, document.id, vault.id, target, *kind)?;
    }
    Ok(unresolved.len())
}

/// Try again to resolve every link that previously pointed at nothing.
///
/// Run once at the end of a pass rather than per document, because a link's
/// target may be spelled in any of the ways `resolve_relative` understands — a
/// bare name, a vault path, a path relative to the linking file, an alias —
/// and only re-running the real resolver gets all of them right. Matching
/// pending links by name alone silently missed `[b](b.md)` written inside
/// `notes/a.md` (MD-007).
pub fn resolve_pending_links(
    conn: &Connection,
    vault: &Vault,
    options: &IndexOptions,
) -> Result<usize> {
    let mut completed = 0usize;
    for (source_id, raw_target, kind) in relationships::pending_in_vault(conn, vault.id)? {
        let Some(source) = documents::get(conn, source_id)? else {
            continue;
        };
        let resolved = documents::resolve_relative(
            conn,
            vault.id,
            Some(&source.path),
            &raw_target,
            &options.link_vaults,
        )?;
        let Some((target, _)) = resolved else {
            continue;
        };
        if target.document != source_id {
            let edge = Relationship::new(Endpoints::new(source.global_id(), target), kind);
            if relationships::upsert(conn, &edge)?.changed() {
                completed += 1;
            }
        }
        relationships::remove_pending_link(conn, source_id, &raw_target, kind)?;
    }
    Ok(completed)
}

fn refresh_search_index(
    conn: &Connection,
    document: &Document,
    parsed: &ParsedDocument,
) -> Result<()> {
    let Some(seq) = documents::seq_of(conn, document.id)? else {
        return Ok(());
    };
    // Every tag, including a model's, so a document is findable by what the
    // application concluded about it as well as by what it says (TAG-005).
    let tag_text = tags::of_document(conn, document.id)?
        .iter()
        .map(|(tag, _)| tag.canonical_name.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let alias_text = documents::aliases(conn, document.id)?.join(" ");

    search_index::index(
        conn,
        seq,
        &IndexedText {
            title: document.title.clone(),
            body: parsed.plain_text.clone(),
            tags: tag_text,
            path: document.path.to_string(),
            aliases: alias_text,
        },
    )?;
    Ok(())
}

/// Re-index a single path, for a filesystem watcher or an in-app save.
pub fn index_path(
    db: &mut Database,
    vault: &Vault,
    path: &VaultPath,
    options: &IndexOptions,
) -> Result<Option<DocumentId>> {
    let absolute = path.to_fs(&vault.root);
    if !absolute.exists() {
        // The file is gone; remove its record if we had one.
        if let Some(document) = documents::by_path(db.conn(), vault.id, path)? {
            let tx = db.transaction()?;
            if let Some(seq) = documents::seq_of(&tx, document.id)? {
                search_index::remove(&tx, seq)?;
            }
            documents::delete(&tx, document.id)?;
            marius_storage::commit(tx)?;
        }
        return Ok(None);
    }

    let metadata = std::fs::metadata(&absolute).map_err(|source| FileError::Read {
        path: absolute.clone(),
        source,
    })?;
    let file = ScannedFile {
        path: path.clone(),
        absolute,
        size_bytes: metadata.len(),
        modified: metadata
            .modified()
            .ok()
            .map(OffsetDateTime::from)
            .unwrap_or_else(OffsetDateTime::now_utc),
        kind: DocumentKind::from_extension(path.extension().as_deref()),
    };
    let existing = documents::by_path(db.conn(), vault.id, path)?;
    let mut report = IndexReport::default();
    let tx = db.transaction()?;
    let id = index_file(&tx, vault, &file, existing.as_ref(), options, &mut report)?;
    resolve_pending_links(&tx, vault, options)?;
    marius_storage::commit(tx)?;
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marius_model::{Confidence, Provenance, RelationshipKind, VaultPolicy};
    use marius_storage::vaults;

    struct Fixture {
        db: Database,
        vault: Vault,
        dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::new_local("Research", dir.path());
        vaults::upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        Fixture { db, vault, dir }
    }

    impl Fixture {
        fn write(&self, relative: &str, text: &str) {
            files::write_atomic(&self.dir.path().join(relative), text).unwrap();
        }

        fn index(&mut self) -> IndexReport {
            let options = IndexOptions::default();
            let vault = self.vault.clone();
            index_vault(&mut self.db, &vault, &options).unwrap()
        }

        fn index_with(&mut self, options: IndexOptions) -> IndexReport {
            let vault = self.vault.clone();
            index_vault(&mut self.db, &vault, &options).unwrap()
        }

        fn doc(&self, relative: &str) -> Document {
            documents::by_path(
                self.db.conn(),
                self.vault.id,
                &VaultPath::new(relative).unwrap(),
            )
            .unwrap()
            .unwrap_or_else(|| panic!("{relative} is not indexed"))
        }

        fn tag_names(&self, relative: &str) -> Vec<String> {
            let mut names: Vec<String> = tags::of_document(self.db.conn(), self.doc(relative).id)
                .unwrap()
                .iter()
                .map(|(t, _)| t.canonical_name.to_string())
                .collect();
            names.sort();
            names
        }
    }

    #[test]
    fn opening_a_directory_of_markdown_indexes_it_in_place() {
        // AC-001.
        let mut f = fixture();
        f.write(
            "notes/alexandria.md",
            "---\ntitle: The Pharos\ntags: [history]\n---\n# Pharos\n\nIt stood at the harbour. #antiquity\n",
        );
        let report = f.index();
        assert_eq!(report.added, 1, "{}", report.summary());
        assert!(report.problems.is_empty(), "{:?}", report.problems);

        let document = f.doc("notes/alexandria.md");
        assert_eq!(document.title, "The Pharos");
        assert_eq!(document.kind, DocumentKind::Markdown);
        assert_eq!(document.authorship, Authorship::Source);

        // The file is untouched.
        let on_disk = files::read_text(&f.dir.path().join("notes/alexandria.md")).unwrap();
        assert!(on_disk.text.starts_with("---\ntitle: The Pharos"));
        assert_eq!(on_disk.hash, document.content_hash);
    }

    #[test]
    fn titles_fall_back_from_frontmatter_to_heading_to_filename() {
        // KB-007.
        let mut f = fixture();
        f.write(
            "a.md",
            "---\ntitle: From Frontmatter\n---\n# From Heading\n",
        );
        f.write("b.md", "# From Heading\n");
        f.write("c.md", "no heading here\n");
        f.index();
        assert_eq!(f.doc("a.md").title, "From Frontmatter");
        assert_eq!(f.doc("b.md").title, "From Heading");
        assert_eq!(f.doc("c.md").title, "c");
    }

    #[test]
    fn unknown_frontmatter_is_preserved_in_the_index() {
        // MD-008.
        let mut f = fixture();
        f.write(
            "a.md",
            "---\ncssclass: wide\nmy-plugin-field: 7\n---\nbody\n",
        );
        f.index();
        let document = f.doc("a.md");
        assert_eq!(
            document.frontmatter.get("cssclass"),
            Some(&marius_model::MetadataValue::Text("wide".into()))
        );
        assert!(document.frontmatter.contains_key("my-plugin-field"));
    }

    #[test]
    fn broken_frontmatter_is_reported_but_the_note_is_still_indexed() {
        // AI-004's principle: nothing optional may break the core.
        let mut f = fixture();
        f.write(
            "a.md",
            "---\ntitle: [unclosed\n---\n# Body\n\nstill searchable\n",
        );
        let report = f.index();
        assert_eq!(report.added, 1);
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].1.contains("frontmatter"));
        assert_eq!(f.doc("a.md").title, "Body");
    }

    #[test]
    fn inline_and_frontmatter_tags_are_indexed_with_their_source() {
        // TAG-004.
        let mut f = fixture();
        f.write("a.md", "---\ntags: [history]\n---\nAbout #lighthouses.\n");
        f.index();
        assert_eq!(f.tag_names("a.md"), vec!["history", "lighthouses"]);

        let assignments = tags::of_document(f.db.conn(), f.doc("a.md").id).unwrap();
        let sources: Vec<TagSource> = assignments.iter().map(|(_, a)| a.source).collect();
        assert!(sources.contains(&TagSource::Inline));
        assert!(sources.contains(&TagSource::Frontmatter));
        assert!(assignments
            .iter()
            .all(|(_, a)| a.provenance == Provenance::Derived));
    }

    #[test]
    fn removing_an_inline_tag_from_the_file_removes_the_assignment() {
        let mut f = fixture();
        f.write("a.md", "About #lighthouses and #history.\n");
        f.index();
        assert_eq!(f.tag_names("a.md"), vec!["history", "lighthouses"]);

        f.write("a.md", "About #lighthouses only.\n");
        f.index();
        assert_eq!(f.tag_names("a.md"), vec!["lighthouses"]);
    }

    #[test]
    fn reindexing_never_discards_a_manual_or_ai_tag() {
        // TAG-006 and TAG-009 — the central protection, end to end.
        let mut f = fixture();
        f.write("a.md", "About #inline.\n");
        f.index();
        let id = f.doc("a.md").id;

        let manual = tags::ensure(
            f.db.conn(),
            Some(f.vault.id),
            &TagName::new("manual").unwrap(),
        )
        .unwrap();
        let from_model = tags::ensure(
            f.db.conn(),
            Some(f.vault.id),
            &TagName::new("from-model").unwrap(),
        )
        .unwrap();
        tags::assign(f.db.conn(), &DocumentTag::manual(id, manual.id)).unwrap();
        tags::assign(
            f.db.conn(),
            &DocumentTag::ai_generated(id, from_model.id, "llama3.2", Confidence::new(0.9)),
        )
        .unwrap();

        f.write("a.md", "About #inline, edited.\n");
        f.index();

        assert_eq!(f.tag_names("a.md"), vec!["from-model", "inline", "manual"]);
    }

    #[test]
    fn a_rejected_tag_stays_rejected_across_a_reindex() {
        // AC-006, integrity rule 7, end to end.
        let mut f = fixture();
        f.write("a.md", "About #history.\n");
        f.index();
        let id = f.doc("a.md").id;
        let history = tags::by_name(
            f.db.conn(),
            Some(f.vault.id),
            &TagName::new("history").unwrap(),
        )
        .unwrap()
        .unwrap();
        tags::reject(f.db.conn(), id, history.id, Some("not about history")).unwrap();
        assert!(f.tag_names("a.md").is_empty());

        // The `#history` text is still in the file; the rejection must win.
        f.write("a.md", "About #history, edited.\n");
        f.index();
        assert!(
            f.tag_names("a.md").is_empty(),
            "a rejection must survive reindexing"
        );
    }

    #[test]
    fn wiki_links_become_edges_and_backlinks() {
        // GRAPH-002, MD-006.
        let mut f = fixture();
        f.write("a.md", "See [[Alexandria]] and ![[diagram.png]].\n");
        f.write("Alexandria.md", "# Alexandria\n");
        f.write("diagram.png", "x");
        f.index();

        let a = f.doc("a.md");
        let target = f.doc("Alexandria.md");
        let out = relationships::outgoing(f.db.conn(), a.id).unwrap();
        let kinds: Vec<RelationshipKind> = out.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&RelationshipKind::Link), "{kinds:?}");
        assert!(kinds.contains(&RelationshipKind::Embed), "{kinds:?}");

        let back = relationships::incoming(f.db.conn(), target.id).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].endpoints.source.document, a.id);
    }

    #[test]
    fn links_resolve_through_aliases() {
        // KB-010.
        let mut f = fixture();
        f.write("a.md", "See [[Pharos]].\n");
        f.write(
            "Alexandria.md",
            "---\naliases:\n  - Pharos\n---\n# Alexandria\n",
        );
        f.index();
        // Indexing order is alphabetical, so `a.md` is read before the alias
        // exists; the pending link completes when `Alexandria.md` is indexed.
        let out = relationships::outgoing(f.db.conn(), f.doc("a.md").id).unwrap();
        assert_eq!(out.len(), 1, "{out:#?}");
        assert_eq!(out[0].endpoints.target.document, f.doc("Alexandria.md").id);
    }

    #[test]
    fn a_relative_markdown_link_resolves_against_its_own_folder() {
        // The spelling an exported or hand-written vault actually uses.
        let mut f = fixture();
        f.write("notes/a.md", "See [b](b.md) and [deep](sub/c.md).\n");
        f.write("notes/b.md", "# B\n");
        f.write("notes/sub/c.md", "# C\n");
        f.index();

        let targets: Vec<DocumentId> = relationships::outgoing(f.db.conn(), f.doc("notes/a.md").id)
            .unwrap()
            .iter()
            .map(|e| e.endpoints.target.document)
            .collect();
        assert!(targets.contains(&f.doc("notes/b.md").id), "{targets:?}");
        assert!(targets.contains(&f.doc("notes/sub/c.md").id), "{targets:?}");
    }

    #[test]
    fn a_link_to_a_document_that_does_not_exist_yet_is_remembered() {
        // MD-007.
        let mut f = fixture();
        f.write("a.md", "See [[Future Note]].\n");
        let report = f.index();
        assert_eq!(report.links_unresolved, 1);
        assert_eq!(
            relationships::pending_links(f.db.conn(), f.doc("a.md").id).unwrap(),
            vec!["Future Note"]
        );
    }

    #[test]
    fn creating_the_target_completes_the_waiting_link() {
        // MD-007: without a full reindex of the linking document.
        let mut f = fixture();
        f.write("a.md", "See [[Future Note]].\n");
        f.index();
        assert!(relationships::outgoing(f.db.conn(), f.doc("a.md").id)
            .unwrap()
            .is_empty());

        f.write("Future Note.md", "# Future Note\n");
        let report = f.index();
        assert_eq!(report.links_completed, 1, "{}", report.summary());

        let out = relationships::outgoing(f.db.conn(), f.doc("a.md").id).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].endpoints.target.document, f.doc("Future Note.md").id);
        assert!(relationships::pending_links(f.db.conn(), f.doc("a.md").id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_link_removed_from_a_file_removes_its_edge() {
        let mut f = fixture();
        f.write("a.md", "See [[b]] and [[c]].\n");
        f.write("b.md", "# b\n");
        f.write("c.md", "# c\n");
        f.index();
        assert_eq!(
            relationships::outgoing(f.db.conn(), f.doc("a.md").id)
                .unwrap()
                .len(),
            2
        );

        f.write("a.md", "See [[b]] only.\n");
        f.index();
        let out = relationships::outgoing(f.db.conn(), f.doc("a.md").id).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].endpoints.target.document, f.doc("b.md").id);
    }

    #[test]
    fn reindexing_does_not_delete_a_confirmed_inferred_edge() {
        // GRAPH-012, end to end.
        let mut f = fixture();
        f.write("a.md", "# A\n");
        f.write("b.md", "# B\n");
        f.index();
        let a = f.doc("a.md");
        let b = f.doc("b.md");

        let inferred = Relationship::semantic(
            Endpoints::new(a.global_id(), b.global_id()),
            "llama3.2",
            Confidence::new(0.8),
            None,
        );
        relationships::upsert(f.db.conn(), &inferred).unwrap();
        relationships::confirm(f.db.conn(), inferred.id).unwrap();

        f.write("a.md", "# A, edited\n");
        f.index();
        assert!(
            relationships::get(f.db.conn(), inferred.id)
                .unwrap()
                .is_some(),
            "a confirmed edge must survive reindexing"
        );
    }

    #[test]
    fn a_self_link_is_not_an_edge() {
        let mut f = fixture();
        f.write("a.md", "# A\n\nSee [[a]] and [[#a-heading]].\n");
        f.index();
        assert!(relationships::outgoing(f.db.conn(), f.doc("a.md").id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn external_links_are_not_edges() {
        let mut f = fixture();
        f.write(
            "a.md",
            "See [site](https://example.com) and <https://x.y>.\n",
        );
        let report = f.index();
        assert_eq!(report.links_unresolved, 0);
        assert!(relationships::outgoing(f.db.conn(), f.doc("a.md").id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn documents_are_findable_by_text_title_tag_and_alias() {
        // SRCH-001, SRCH-002.
        let mut f = fixture();
        f.write(
            "notes/alexandria.md",
            "---\ntitle: The Pharos\naliases: [Wonder]\ntags: [history]\n---\nIt stood at the harbour mouth.\n",
        );
        f.index();

        for query in [
            "harbour",
            "\"harbour mouth\"",
            "Pharos",
            "tags:history",
            "aliases:Wonder",
            "path:alexandria",
        ] {
            let hits = search_index::search(f.db.conn(), query, 10, 0).unwrap();
            assert_eq!(hits.len(), 1, "{query} found {} hits", hits.len());
        }
    }

    #[test]
    fn the_search_index_is_updated_when_a_file_changes() {
        // SRCH-013.
        let mut f = fixture();
        f.write("a.md", "original wording\n");
        f.index();
        assert_eq!(
            search_index::search(f.db.conn(), "original", 10, 0)
                .unwrap()
                .len(),
            1
        );

        f.write("a.md", "replacement wording\n");
        f.index();
        assert!(search_index::search(f.db.conn(), "original", 10, 0)
            .unwrap()
            .is_empty());
        assert_eq!(
            search_index::search(f.db.conn(), "replacement", 10, 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn code_in_fences_is_searchable_but_its_hashes_are_not_tags() {
        let mut f = fixture();
        f.write(
            "a.md",
            "Run it:\n\n```c\n#include <stdio.h>\n```\n\nTagged #real.\n",
        );
        f.index();
        assert_eq!(f.tag_names("a.md"), vec!["real"]);
        assert_eq!(
            search_index::search(f.db.conn(), "stdio", 10, 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_file_moved_outside_the_app_keeps_its_identity_and_its_backlinks() {
        // AC-003 end to end — the guarantee the whole design turns on.
        let mut f = fixture();
        f.write("notes/target.md", "# Target\n\ncontent that will move\n");
        f.write("notes/source.md", "See [[target]].\n");
        f.index();

        let original = f.doc("notes/target.md");
        let source = f.doc("notes/source.md");
        assert_eq!(
            relationships::outgoing(f.db.conn(), source.id)
                .unwrap()
                .len(),
            1
        );

        std::fs::create_dir_all(f.dir.path().join("archive")).unwrap();
        std::fs::rename(
            f.dir.path().join("notes/target.md"),
            f.dir.path().join("archive/target.md"),
        )
        .unwrap();
        let report = f.index();
        assert_eq!(report.moved, 1, "{}", report.summary());

        let moved = f.doc("archive/target.md");
        assert_eq!(moved.id, original.id, "the identity must survive");
        let back = relationships::incoming(f.db.conn(), moved.id).unwrap();
        assert_eq!(back.len(), 1, "the backlink must survive");
        assert_eq!(back[0].endpoints.source.document, source.id);

        // And the move is in the document's history.
        let history = revisions::list(f.db.conn(), moved.id).unwrap();
        assert!(history.iter().any(|r| r.cause == RevisionCause::Relocation));
    }

    #[test]
    fn a_deleted_file_is_removed_from_the_index_and_from_search() {
        let mut f = fixture();
        f.write("a.md", "findable text\n");
        f.index();
        assert_eq!(search_index::count(f.db.conn()).unwrap(), 1);

        std::fs::remove_file(f.dir.path().join("a.md")).unwrap();
        let report = f.index();
        assert_eq!(report.removed, 1);
        assert_eq!(search_index::count(f.db.conn()).unwrap(), 0);
        assert!(search_index::search(f.db.conn(), "findable", 10, 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_unchanged_vault_reindexes_to_nothing() {
        let mut f = fixture();
        f.write("a.md", "# A\n");
        f.write("b.md", "# B\n");
        f.index();

        let report = f.index();
        assert_eq!(report.touched(), 0, "{}", report.summary());
        assert_eq!(report.unchanged, 2);
    }

    #[test]
    fn revisions_accumulate_for_external_edits() {
        // KB-011.
        let mut f = fixture();
        f.write("a.md", "first\n");
        f.index();
        f.write("a.md", "second\n");
        f.index();

        let history = revisions::list(f.db.conn(), f.doc("a.md").id).unwrap();
        let causes: Vec<RevisionCause> = history.iter().map(|r| r.cause).collect();
        assert!(causes.contains(&RevisionCause::Created));
        assert!(causes.contains(&RevisionCause::ExternalChange));
        let newest = history.first().unwrap();
        assert_eq!(
            revisions::body(f.db.conn(), newest.id).unwrap().as_deref(),
            Some("second\n")
        );
    }

    #[test]
    fn attachments_are_indexed_by_name_without_being_read_as_text() {
        // KB-013, MD-005.
        let mut f = fixture();
        std::fs::write(
            f.dir.path().join("diagram.png"),
            [0x89, 0x50, 0x4E, 0x47, 0xFF, 0xFE],
        )
        .unwrap();
        let report = f.index();
        assert_eq!(report.added, 1);
        assert!(report.problems.is_empty(), "{:?}", report.problems);

        let document = f.doc("diagram.png");
        assert_eq!(document.kind, DocumentKind::Attachment);
        assert_eq!(document.title, "diagram.png");
        assert_eq!(
            search_index::search(f.db.conn(), "diagram", 10, 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_sensitivity_declared_in_frontmatter_is_honoured_and_only_tightens() {
        // SEC-005 with AI-007.
        let mut f = fixture();
        f.write("a.md", "---\nsensitivity: confidential\n---\nprivate\n");
        f.index();
        assert_eq!(f.doc("a.md").sensitivity, Some(Sensitivity::Confidential));

        // Relaxing it in the file does not relax the document.
        f.write("a.md", "---\nsensitivity: unrestricted\n---\nprivate\n");
        f.index();
        assert_eq!(
            f.doc("a.md").sensitivity,
            Some(Sensitivity::Confidential),
            "a restriction must never be lowered automatically"
        );
    }

    #[test]
    fn chunks_are_produced_only_when_asked_for() {
        // SRCH-008.
        let mut f = fixture();
        f.write("a.md", "# Heading\n\nA passage of text.\n");
        f.index();
        assert_eq!(chunks::counts(f.db.conn()).unwrap().0, 0);

        f.write("a.md", "# Heading\n\nA passage of text, edited.\n");
        f.index_with(IndexOptions {
            chunking: Some(ChunkConfig::default()),
            ..IndexOptions::default()
        });
        let stored = chunks::of_document(f.db.conn(), f.doc("a.md").id).unwrap();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].text.contains("passage"));
        assert_eq!(stored[0].heading_path, vec!["Heading"]);
    }

    #[test]
    fn an_unreadable_file_does_not_stop_the_pass() {
        // NFR-006.
        let mut f = fixture();
        f.write("good.md", "# Good\n");
        std::fs::write(f.dir.path().join("bad.md"), [0x48, 0xFF, 0x6C]).unwrap();

        let report = f.index();
        assert_eq!(report.added, 1, "{}", report.summary());
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].0.contains("bad.md"));
        assert_eq!(f.doc("good.md").title, "Good");
    }

    #[test]
    fn indexing_a_single_path_works_for_a_watcher() {
        let mut f = fixture();
        f.write("a.md", "# A\n");
        let path = VaultPath::new("a.md").unwrap();
        let vault = f.vault.clone();
        let id = index_path(&mut f.db, &vault, &path, &IndexOptions::default())
            .unwrap()
            .unwrap();
        assert_eq!(f.doc("a.md").id, id);

        std::fs::remove_file(f.dir.path().join("a.md")).unwrap();
        let gone = index_path(&mut f.db, &vault, &path, &IndexOptions::default()).unwrap();
        assert_eq!(gone, None);
        assert!(documents::by_path(f.db.conn(), f.vault.id, &path)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_large_vault_indexes_in_resumable_batches() {
        // NFR-006: batching is what makes an interrupted pass keep its work.
        let mut f = fixture();
        for i in 0..200 {
            f.write(
                &format!("notes/note-{i:03}.md"),
                &format!("# Note {i}\n\ncontent #bulk\n"),
            );
        }
        let report = f.index_with(IndexOptions {
            batch_size: 16,
            ..IndexOptions::default()
        });
        assert_eq!(report.added, 200, "{}", report.summary());
        assert_eq!(documents::count(f.db.conn(), f.vault.id).unwrap(), 200);
        assert_eq!(search_index::count(f.db.conn()).unwrap(), 200);
    }
}
