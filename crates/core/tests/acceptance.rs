//! End-to-end acceptance tests, named after the scenarios in the
//! specification's section 8.
//!
//! These exercise the real stack — files on disk, SQLite, the indexer, the
//! query language — through the public service API, with no test doubles. Each
//! one states the acceptance condition it is checking.

use std::fs;
use std::path::Path;

use marius_core::{Marius, SearchOptions};
use marius_model::{
    Authorship, DocumentKind, Provenance, RelationshipKind, Sensitivity, VaultPath,
};

/// An installation with a vault directory, both in temporary space.
struct Harness {
    app: Marius,
    vault: marius_model::Vault,
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::named("Research")
    }

    fn named(name: &str) -> Self {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let mut app = Marius::open(data.path()).unwrap();
        let vault = app.open_vault(name, files.path()).unwrap();
        Self {
            app,
            vault,
            _data: data,
            files,
        }
    }

    fn root(&self) -> &Path {
        self.files.path()
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.root().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.root().join(relative)).unwrap()
    }

    fn index(&mut self) -> marius_indexing::indexer::IndexReport {
        let vault = self.vault.clone();
        self.app.index(&vault).unwrap()
    }

    fn doc(&self, relative: &str) -> marius_model::Document {
        self.app
            .document_at(self.vault.id, &VaultPath::new(relative).unwrap())
            .unwrap()
            .unwrap_or_else(|| panic!("{relative} is not indexed"))
    }

    fn find(&self, query: &str) -> Vec<String> {
        self.app
            .search(query, SearchOptions::default())
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.document.path.to_string())
            .collect()
    }

    fn tag_names(&self, relative: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .app
            .document_tags(self.doc(relative).id)
            .unwrap()
            .into_iter()
            .map(|(tag, _)| tag.canonical_name.to_string())
            .collect();
        names.sort();
        names
    }
}

/// AC-001 — Open an existing directory containing Markdown files.
///
/// Acceptance: documents render and edit in place; files remain standard
/// Markdown.
#[test]
fn ac_001_an_existing_directory_opens_in_place() {
    let mut h = Harness::new();
    let original = "---\ntitle: The Pharos\ntags: [history, lighthouse]\ncssclass: wide\n---\n\
                    # Pharos\n\nIt stood at the harbour mouth. #antiquity\n";
    h.write("notes/alexandria.md", original);

    let report = h.index();
    assert_eq!(report.added, 1, "{}", report.summary());
    assert!(report.problems.is_empty(), "{:?}", report.problems);

    let document = h.doc("notes/alexandria.md");
    assert_eq!(document.title, "The Pharos");
    assert_eq!(document.kind, DocumentKind::Markdown);

    // The file is byte-for-byte what the user wrote.
    assert_eq!(h.read("notes/alexandria.md"), original);

    // And it is readable through the service, from the file itself.
    assert_eq!(h.app.read(&document).unwrap(), original);

    // Unknown frontmatter survived (MD-008).
    assert!(document.frontmatter.contains_key("cssclass"));
}

/// AC-002 — Import an Obsidian vault.
///
/// Acceptance: files, links, attachments and supported frontmatter are
/// preserved. Opening in place is the import: no conversion step exists.
#[test]
fn ac_002_an_obsidian_style_vault_opens_without_conversion() {
    let mut h = Harness::new();
    // A shape an Obsidian vault actually has.
    h.write(".obsidian/app.json", "{\"promptDelete\":false}");
    h.write(".obsidian/plugins/dataview/main.js", "// plugin code");
    h.write(
        "Daily/2024-03-05.md",
        "---\ntags:\n  - daily\n---\n# 2024-03-05\n\nSaw [[Pharos of Alexandria]] today.\n\
         ![[attachments/plan.png]]\n",
    );
    h.write(
        "Pharos of Alexandria.md",
        "---\naliases: [Pharos]\n---\n# Pharos of Alexandria\n\nA wonder. #history\n",
    );
    h.write("attachments/plan.png", "\u{89}PNG fake");

    let report = h.index();
    assert_eq!(report.added, 3, "{}", report.summary());
    assert!(report.problems.is_empty(), "{:?}", report.problems);

    // Obsidian's own config is not indexed as knowledge.
    assert!(h
        .app
        .document_at(h.vault.id, &VaultPath::new(".obsidian/app.json").unwrap())
        .unwrap()
        .is_none());

    // Links, embeds and frontmatter came across.
    let daily = h.doc("Daily/2024-03-05.md");
    let neighbourhood = h.app.neighbourhood(daily.id).unwrap();
    let kinds: Vec<RelationshipKind> = neighbourhood.outgoing.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&RelationshipKind::Link), "{kinds:?}");
    assert!(kinds.contains(&RelationshipKind::Embed), "{kinds:?}");
    assert_eq!(h.tag_names("Daily/2024-03-05.md"), vec!["daily"]);

    // The attachment is indexed but never parsed as text.
    assert_eq!(h.doc("attachments/plan.png").kind, DocumentKind::Attachment);

    // And nothing on disk changed.
    assert!(h
        .read("Pharos of Alexandria.md")
        .starts_with("---\naliases: [Pharos]"));
}

/// AC-003 — Rename a linked document.
///
/// Acceptance: stable identity and internal relationships remain valid.
#[test]
fn ac_003_renaming_a_linked_document_keeps_identity_and_fixes_links() {
    let mut h = Harness::new();
    h.write(
        "notes/source.md",
        "See [[Alexandria]], [[Alexandria|the Pharos]] and [[notes/Alexandria#Construction]].\n\
         Also [a link](Alexandria.md) and [absolute](notes/Alexandria.md).\n",
    );
    // The heading deliberately differs from the filename. If link targets were
    // resolved *after* the move, the document's stale title could still match
    // `Alexandria` by accident and the test would pass while the feature was
    // broken — which is exactly what happened before this was pinned down.
    h.write(
        "notes/Alexandria.md",
        "# A completely different heading\n\n## Construction\n\nBuilt.\n",
    );
    h.index();

    let target = h.doc("notes/Alexandria.md");
    let source = h.doc("notes/source.md");
    let original_id = target.id;
    assert_eq!(
        h.app.neighbourhood(source.id).unwrap().outgoing.len(),
        1,
        "all five links point at one document, so there is one edge"
    );

    let report = h.app.rename_document(target.id, "Pharos").unwrap();
    assert_eq!(report.to.as_str(), "notes/Pharos.md");
    assert_eq!(report.files_rewritten.len(), 1, "{report:?}");
    assert!(report.unwritable.is_empty(), "{:?}", report.unwritable);
    assert_eq!(report.links_rewritten, 5, "{report:?}");

    // Identity survived.
    let renamed = h.doc("notes/Pharos.md");
    assert_eq!(renamed.id, original_id);

    // The referring file was rewritten, preserving each author's spelling.
    let rewritten = h.read("notes/source.md");
    assert_eq!(
        rewritten,
        "See [[Pharos]], [[Pharos|the Pharos]] and [[notes/Pharos#Construction]].\n\
         Also [a link](Pharos.md) and [absolute](notes/Pharos.md).\n"
    );

    // The relationship still resolves after a reindex.
    h.index();
    let back = h.app.neighbourhood(renamed.id).unwrap();
    assert_eq!(back.incoming.len(), 1);
    assert_eq!(back.incoming[0].endpoints.source.document, source.id);
    assert!(back.unresolved.is_empty());
}

/// AC-003 (continued) — a move, rather than a rename.
#[test]
fn ac_003_moving_a_document_keeps_identity_and_fixes_links() {
    let mut h = Harness::new();
    h.write("notes/source.md", "See [[target]].\n");
    h.write("notes/target.md", "# Target\n");
    h.index();
    let original = h.doc("notes/target.md").id;

    let folder = VaultPath::new("archive/2024").unwrap();
    let report = h.app.move_document(original, Some(&folder)).unwrap();
    assert_eq!(report.to.as_str(), "archive/2024/target.md");

    assert_eq!(h.doc("archive/2024/target.md").id, original);
    // A bare wiki name needs no rewriting when the name itself is unchanged.
    assert_eq!(h.read("notes/source.md"), "See [[target]].\n");

    h.index();
    assert_eq!(
        h.app.neighbourhood(original).unwrap().incoming.len(),
        1,
        "the link must still resolve after the move"
    );
}

/// AC-004 — Manually add and remove a tag.
///
/// Acceptance: tag changes are immediately reflected in browse, search and
/// graph views.
#[test]
fn ac_004_manual_tag_changes_are_reflected_immediately() {
    let mut h = Harness::new();
    h.write("a.md", "# A\n\nAbout harbours.\n");
    h.write("b.md", "# B\n\nAlso about harbours.\n");
    h.index();
    let a = h.doc("a.md").id;
    let b = h.doc("b.md").id;

    h.app.add_tag(a, "Lighthouse Engineering").unwrap();
    assert_eq!(h.tag_names("a.md"), vec!["lighthouse-engineering"]);

    // Browse: the tag appears in the tag browser with a count.
    let counts = h.app.tag_counts(Some(h.vault.id)).unwrap();
    let entry = counts
        .iter()
        .find(|(tag, _)| tag.canonical_name.as_str() == "lighthouse-engineering")
        .expect("the tag must be browsable");
    assert_eq!(entry.1, 1);

    // Search: findable by the tag, with no reindex.
    assert_eq!(h.find("tag:lighthouse-engineering"), vec!["a.md"]);
    assert_eq!(h.find("tag:\"Lighthouse Engineering\""), vec!["a.md"]);

    // Graph: shared-tag kinship appears once both documents carry it.
    h.app.add_tag(b, "lighthouse-engineering").unwrap();
    let kin = h.app.neighbourhood(a).unwrap().shared_tags;
    assert_eq!(kin.len(), 1);
    assert_eq!(kin[0].document.document, b);

    // And removal takes effect just as immediately.
    assert!(h.app.remove_tag(a, "lighthouse-engineering").unwrap());
    assert!(h.tag_names("a.md").is_empty());
    assert!(h
        .find("tag:lighthouse-engineering")
        .iter()
        .all(|p| p != "a.md"));
    assert!(h.app.neighbourhood(a).unwrap().shared_tags.is_empty());
}

/// AC-005 — Run AI auto-tagging on an imported note.
///
/// Acceptance: metadata tags appear with provenance; the original Markdown body
/// and frontmatter are unchanged without a user-authorised writeback.
///
/// The classifier itself is a later phase; what is verified here is the
/// guarantee it will be held to — an AI tag is index-only and the file is not
/// touched.
#[test]
fn ac_005_ai_tags_carry_provenance_and_never_touch_the_file() {
    use marius_model::{Confidence, DocumentTag, TagName};
    use marius_storage::tags as tag_store;

    let mut h = Harness::new();
    let original = "---\ntitle: Imported\n---\n# Imported\n\nText about harbours.\n";
    h.write("imported.md", original);
    h.index();
    let id = h.doc("imported.md").id;

    let tag = tag_store::ensure(
        h.app.database().conn(),
        Some(h.vault.id),
        &TagName::new("maritime-infrastructure").unwrap(),
    )
    .unwrap();
    tag_store::assign(
        h.app.database().conn(),
        &DocumentTag::ai_generated(id, tag.id, "llama3.2", Confidence::new(0.82)),
    )
    .unwrap();

    // Provenance is recorded and visible.
    let (_, assignment) = h
        .app
        .document_tags(id)
        .unwrap()
        .into_iter()
        .find(|(t, _)| t.id == tag.id)
        .unwrap();
    assert_eq!(assignment.provenance, Provenance::AiGenerated);
    assert_eq!(assignment.model.as_deref(), Some("llama3.2"));
    assert_eq!(assignment.confidence.unwrap().get(), 0.82);
    assert_eq!(assignment.source, marius_model::TagSource::Index);

    // The file is untouched.
    assert_eq!(h.read("imported.md"), original);

    // A reindex does not discard it, and still does not write to the file.
    h.index();
    assert!(h
        .tag_names("imported.md")
        .contains(&"maritime-infrastructure".to_string()));
    assert_eq!(h.read("imported.md"), original);
}

/// AC-006 — Reject an AI tag or inferred relationship.
///
/// Acceptance: the rejection persists across reindexing and synchronisation.
#[test]
fn ac_006_a_rejection_persists_across_reindexing() {
    use marius_model::{Confidence, DocumentTag, Endpoints, Relationship, TagName};
    use marius_storage::{relationships as edge_store, tags as tag_store};

    let mut h = Harness::new();
    h.write("a.md", "# A\n\nAbout #history.\n");
    h.write("b.md", "# B\n\nSomething else.\n");
    h.index();
    let a = h.doc("a.md");
    let b = h.doc("b.md");

    // Reject a tag the file itself asserts.
    h.app
        .reject_tag(a.id, "history", Some("not the right sense"))
        .unwrap();
    assert!(h.tag_names("a.md").is_empty());

    // Reject an inferred relationship.
    let inferred = Relationship::semantic(
        Endpoints::new(a.global_id(), b.global_id()),
        "llama3.2",
        Confidence::new(0.8),
        Some("both mention harbours".into()),
    );
    edge_store::upsert(h.app.database().conn(), &inferred).unwrap();
    assert!(edge_store::reject(h.app.database().conn(), inferred.id, Some("unrelated")).unwrap());

    // A full reindex, with the `#history` text still in the file.
    let vault = h.vault.clone();
    h.app.reindex(&vault).unwrap();
    assert!(
        h.tag_names("a.md").is_empty(),
        "a rejected tag must not come back on reindex"
    );

    // And a fresh proposal from a different model is still suppressed.
    let reproposed = Relationship::semantic(
        Endpoints::new(b.global_id(), a.global_id()),
        "a-better-model",
        Confidence::CERTAIN,
        None,
    );
    assert!(!edge_store::upsert(h.app.database().conn(), &reproposed)
        .unwrap()
        .changed());

    let history_tag = tag_store::by_name(
        h.app.database().conn(),
        Some(h.vault.id),
        &TagName::new("history").unwrap(),
    )
    .unwrap()
    .unwrap();
    let still_rejected = tag_store::assign(
        h.app.database().conn(),
        &DocumentTag::ai_generated(a.id, history_tag.id, "m", Confidence::CERTAIN),
    )
    .unwrap();
    assert!(!still_rejected.changed());

    // Even a rebuild of every derived index leaves the rejection standing.
    h.app.rebuild_indexes().unwrap();
    assert!(h.tag_names("a.md").is_empty());
}

/// AC-009 — Two vaults with documents linked across them.
///
/// Acceptance: cross-vault graph and global search work; removing a temporarily
/// unavailable vault does not destroy link metadata.
#[test]
fn ac_009_cross_vault_links_and_global_search() {
    let data = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let personal_dir = tempfile::tempdir().unwrap();
    let mut app = Marius::open(data.path()).unwrap();

    // Cross-vault link resolution is opt-in (a link should mean what its own
    // vault says), so it is switched on explicitly.
    let mut config = app.config().clone();
    config.cross_vault_links = true;
    app.set_config(config).unwrap();

    let work = app.open_vault("Work", work_dir.path()).unwrap();
    let personal = app.open_vault("Personal", personal_dir.path()).unwrap();

    fs::write(
        work_dir.path().join("project.md"),
        "# Project\n\nBackground in [[Lighthouse Reading]].\n",
    )
    .unwrap();
    fs::write(
        personal_dir.path().join("Lighthouse Reading.md"),
        "# Lighthouse Reading\n\nNotes on harbour engineering.\n",
    )
    .unwrap();

    app.index(&work).unwrap();
    app.index(&personal).unwrap();
    // A second pass over the first vault resolves the link now that the other
    // vault's documents exist.
    app.index(&work).unwrap();

    let project = app
        .document_at(work.id, &VaultPath::new("project.md").unwrap())
        .unwrap()
        .unwrap();
    let reading = app
        .document_at(
            personal.id,
            &VaultPath::new("Lighthouse Reading.md").unwrap(),
        )
        .unwrap()
        .unwrap();

    // The cross-vault edge exists.
    let edges = app.neighbourhood(project.id).unwrap().outgoing;
    assert_eq!(edges.len(), 1, "{edges:#?}");
    assert_eq!(edges[0].endpoints.target.document, reading.id);
    assert!(edges[0].endpoints.is_cross_vault());
    let edge_id = edges[0].id;

    // Global search spans both vaults and names the vault of each result.
    let hits = app
        .search("harbour OR Background", SearchOptions::default())
        .unwrap()
        .hits;
    let mut vault_names: Vec<String> = hits.iter().map(|h| h.vault_name.clone()).collect();
    vault_names.sort();
    vault_names.dedup();
    assert_eq!(vault_names, vec!["Personal", "Work"]);
    assert!(hits.iter().all(|h| !h.reference().is_empty()));

    // A vault becoming unavailable hides its documents from search but keeps
    // the edge metadata.
    app.set_vault_available(personal.id, false).unwrap();
    let scoped = app
        .search("harbour", SearchOptions::default())
        .unwrap()
        .hits;
    assert!(scoped.iter().all(|h| h.vault_name != "Personal"));
    assert!(
        marius_storage::relationships::get(app.database().conn(), edge_id)
            .unwrap()
            .is_some()
    );

    // And removing the vault registration still does not destroy it.
    app.close_vault(personal.id).unwrap();
    let orphaned = marius_storage::relationships::get(app.database().conn(), edge_id)
        .unwrap()
        .expect("link metadata must survive an unavailable vault being removed");
    assert_eq!(orphaned.endpoints.target.document, reading.id);
    assert_eq!(orphaned.endpoints.target.vault, personal.id);
}

/// AC-011 — Paste an email into a vault.
///
/// Acceptance: its text becomes searchable, taggable and graph-connectable
/// without email account integration.
#[test]
fn ac_011_a_pasted_email_becomes_ordinary_knowledge() {
    let mut h = Harness::new();
    // What a paste from a mail client looks like, saved as a note.
    h.write(
        "inbox/harbour-survey.md",
        "---\ntitle: Re: harbour survey\nfrom: a colleague\ndate: 2024-03-05\n---\n\
         # Re: harbour survey\n\n\
         The 1869 survey of the harbour entrance is attached. See [[Alexandria]].\n\n#email\n",
    );
    h.write("Alexandria.md", "# Alexandria\n");
    h.index();

    // Searchable by its text and by its headers.
    assert_eq!(
        h.find("\"harbour entrance\""),
        vec!["inbox/harbour-survey.md"]
    );
    assert!(h
        .find("survey")
        .contains(&"inbox/harbour-survey.md".to_string()));

    // Taggable, including the tag written in the pasted text.
    let id = h.doc("inbox/harbour-survey.md").id;
    assert_eq!(h.tag_names("inbox/harbour-survey.md"), vec!["email"]);
    h.app.add_tag(id, "correspondence").unwrap();
    assert_eq!(
        h.find("tag:correspondence"),
        vec!["inbox/harbour-survey.md"]
    );

    // Graph-connectable.
    let edges = h.app.neighbourhood(id).unwrap().outgoing;
    assert_eq!(edges.len(), 1);
    assert_eq!(
        edges[0].endpoints.target.document,
        h.doc("Alexandria.md").id
    );

    // Its unrecognised headers were kept as metadata (MD-008).
    let document = h.doc("inbox/harbour-survey.md");
    assert!(document.frontmatter.contains_key("from"));
    assert_eq!(
        document.frontmatter.get("date").unwrap().type_name(),
        "date"
    );
}

/// AC-016 — Set a vault to local-only AI processing.
///
/// Acceptance: no content from that vault is transmitted to external providers,
/// including during fallback. The policy engine is a later phase; what is
/// verified here is that the setting is stored, durable, and cannot be
/// automatically relaxed.
#[test]
fn ac_016_a_local_only_policy_is_stored_and_cannot_be_relaxed_automatically() {
    let mut h = Harness::new();
    let mut policy = h.app.vault_policy(h.vault.id).unwrap();
    policy.default_sensitivity = Sensitivity::Confidential;
    h.app.set_vault_policy(policy).unwrap();

    let stored = h.app.vault_policy(h.vault.id).unwrap();
    assert_eq!(stored.default_sensitivity, Sensitivity::Confidential);
    assert!(!stored.default_sensitivity.allows_remote_inference());

    // A document declaring itself unrestricted cannot escape the vault floor.
    h.write(
        "a.md",
        "---\nsensitivity: unrestricted\n---\nprivate matters\n",
    );
    h.index();
    let document = h.doc("a.md");
    assert_eq!(
        stored.effective(document.sensitivity),
        Sensitivity::Confidential,
        "a document override may only tighten"
    );

    // And a document may tighten further.
    h.write("b.md", "---\nsensitivity: no_inference\n---\nnever\n");
    h.index();
    assert_eq!(
        stored.effective(h.doc("b.md").sensitivity),
        Sensitivity::NoInference
    );
}

/// AC-017 — Disconnect all networks.
///
/// Acceptance: source documents, manual tags, full-text search and existing
/// graph data remain available. Nothing in the stack exercised here opens a
/// socket, which is the structural form of the guarantee.
#[test]
fn ac_017_everything_essential_works_with_no_network() {
    let mut h = Harness::new();
    h.write("a.md", "# A\n\nHarbour notes. See [[b]].\n");
    h.write("b.md", "# B\n\nMore harbour notes. #history\n");
    h.index();
    let a = h.doc("a.md").id;
    h.app.add_tag(a, "manual-tag").unwrap();

    // Reading, searching, browsing tags and traversing the graph all work.
    assert_eq!(h.app.read(&h.doc("a.md")).unwrap().lines().count(), 3);
    assert_eq!(h.find("harbour").len(), 2);
    assert_eq!(h.find("tag:manual-tag"), vec!["a.md"]);
    assert_eq!(h.find("tag:history"), vec!["b.md"]);
    assert_eq!(h.app.neighbourhood(a).unwrap().outgoing.len(), 1);
    assert!(!h.app.tag_counts(Some(h.vault.id)).unwrap().is_empty());

    // Editing works and is immediately searchable.
    h.app
        .save_document(a, "# A\n\nHarbour notes, revised. See [[b]].\n")
        .unwrap();
    assert_eq!(h.find("revised"), vec!["a.md"]);
}

/// NFR-001 — no loss of committed Markdown data after an ordinary crash.
///
/// The structural guarantee: a save never truncates the destination, and leaves
/// no partial file behind.
#[test]
fn nfr_001_saving_is_atomic_and_leaves_no_debris() {
    let mut h = Harness::new();
    h.write("a.md", "original content that is quite long\n");
    h.index();
    let id = h.doc("a.md").id;

    h.app.save_document(id, "short\n").unwrap();
    assert_eq!(h.read("a.md"), "short\n");

    let leftovers: Vec<String> = fs::read_dir(h.root())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "a.md")
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // The previous content is recoverable from history (KB-011).
    let history = marius_storage::revisions::list(h.app.database().conn(), id).unwrap();
    assert!(history.len() >= 2, "{history:#?}");
    let oldest = history.last().unwrap();
    assert_eq!(
        marius_storage::revisions::body(h.app.database().conn(), oldest.id)
            .unwrap()
            .as_deref(),
        Some("original content that is quite long\n")
    );
}

/// NFR-007 — indexed functionality is restored after a rebuild, without losing
/// authoritative content or metadata.
#[test]
fn nfr_007_a_rebuild_restores_search_without_losing_metadata() {
    let mut h = Harness::new();
    h.write("a.md", "# A\n\nHarbour notes.\n");
    h.index();
    let id = h.doc("a.md").id;
    h.app.add_tag(id, "manual-tag").unwrap();
    h.app.reject_tag(id, "wrong-tag", Some("no")).unwrap();

    assert_eq!(h.find("harbour"), vec!["a.md"]);

    h.app.rebuild_indexes().unwrap();

    // Search works again.
    assert_eq!(h.find("harbour"), vec!["a.md"]);
    // The identity is the same one.
    assert_eq!(h.doc("a.md").id, id);
    // The manual tag and the rejection survived.
    assert_eq!(h.tag_names("a.md"), vec!["manual-tag"]);
    assert_eq!(
        marius_storage::tags::rejections(h.app.database().conn(), id)
            .unwrap()
            .len(),
        1
    );
}

/// KB-001 — removing a vault does not delete its files.
#[test]
fn kb_001_closing_a_vault_leaves_the_users_files_alone() {
    let mut h = Harness::new();
    h.write("a.md", "# A\n");
    h.write("notes/b.md", "# B\n");
    h.index();

    h.app.close_vault(h.vault.id).unwrap();
    assert!(h.app.vaults().unwrap().is_empty());
    assert!(h.root().join("a.md").exists(), "files must not be deleted");
    assert!(h.root().join("notes/b.md").exists());
    assert_eq!(h.read("a.md"), "# A\n");
}

/// KB-002 — opening the same directory twice reuses its vault identity.
#[test]
fn kb_002_reopening_a_directory_does_not_duplicate_it() {
    let mut h = Harness::new();
    let root = h.root().to_path_buf();
    let again = h.app.open_vault("Different Name", &root).unwrap();
    assert_eq!(again.id, h.vault.id);
    assert_eq!(h.app.vaults().unwrap().len(), 1);
}

/// KB-003 — a deleted document can be restored.
#[test]
fn kb_003_a_deleted_document_can_be_restored() {
    let mut h = Harness::new();
    h.write("notes/important.md", "# Important\n\nYears of work.\n");
    h.index();
    let id = h.doc("notes/important.md").id;

    let trashed = h.app.delete_document(id).unwrap();
    assert!(!h.root().join("notes/important.md").exists());
    assert!(h
        .app
        .document_at(h.vault.id, &VaultPath::new("notes/important.md").unwrap())
        .unwrap()
        .is_none());

    let vault = h.vault.clone();
    let restored = h.app.restore_document(&vault, &trashed).unwrap();
    assert_eq!(restored.path.as_str(), "notes/important.md");
    assert_eq!(
        h.read("notes/important.md"),
        "# Important\n\nYears of work.\n"
    );
}

/// KB-012 — archived documents stay searchable unless filtered out.
#[test]
fn kb_012_archived_documents_stay_searchable_unless_filtered() {
    let mut h = Harness::new();
    h.write("a.md", "# A\n\nharbour\n");
    h.write("b.md", "# B\n\nharbour\n");
    h.index();
    h.app.set_archived(h.doc("b.md").id, true).unwrap();

    assert_eq!(h.find("harbour").len(), 2, "archived stays in results");
    assert_eq!(h.find("harbour -is:archived"), vec!["a.md"]);
    assert_eq!(h.find("harbour is:archived"), vec!["b.md"]);
}

/// SRCH-003 and SRCH-005 — phrases, booleans, snippets and result identity.
#[test]
fn srch_003_and_005_query_features_and_result_identity() {
    let mut h = Harness::new();
    h.write("a.md", "# Pharos\n\nIt stood at the harbour mouth.\n");
    h.write("b.md", "# Rhodes\n\nThe mouth of the harbour at Rhodes.\n");
    h.index();

    // Phrase versus conjunction.
    assert_eq!(h.find("\"harbour mouth\""), vec!["a.md"]);
    assert_eq!(h.find("harbour mouth").len(), 2);
    assert_eq!(h.find("harbour -Rhodes"), vec!["a.md"]);
    assert_eq!(h.find("Pharos OR Rhodes").len(), 2);

    // Every result identifies its vault and carries a snippet.
    let results = h.app.search("harbour", SearchOptions::default()).unwrap();
    assert!(!results.hits.is_empty());
    for hit in &results.hits {
        assert_eq!(hit.vault_name, "Research");
        assert!(!hit.document.id.is_nil());
        assert!(hit.reference().starts_with("Research:"));
        assert!(
            hit.snippet.as_ref().is_some_and(|s| s.contains("harbour")),
            "{:?}",
            hit.snippet
        );
    }
}

/// A title match should outrank a passing mention in a long note.
#[test]
fn search_ranks_title_matches_above_body_mentions() {
    let mut h = Harness::new();
    h.write(
        "mention.md",
        &format!("# Unrelated\n\n{}\nlighthouse\n", "filler ".repeat(200)),
    );
    h.write("lighthouse.md", "# Lighthouse\n\nThe subject itself.\n");
    h.index();
    assert_eq!(
        h.find("lighthouse").first().map(String::as_str),
        Some("lighthouse.md")
    );
}

/// AI-012 — generated notes are visible but filterable.
#[test]
fn ai_012_generated_notes_are_filterable() {
    use marius_storage::documents as doc_store;

    let mut h = Harness::new();
    h.write("source.md", "# Source\n\nharbour\n");
    h.write("summary.md", "# Summary\n\nharbour\n");
    h.index();

    // Mark one as generated, as the summarisation pipeline will.
    let mut generated = h.doc("summary.md");
    generated.authorship = Authorship::Derived;
    generated.kind = DocumentKind::GeneratedNote;
    doc_store::upsert(h.app.database().conn(), &generated).unwrap();

    assert_eq!(
        h.find("harbour").len(),
        2,
        "generated notes are first-class"
    );
    assert_eq!(h.find("harbour -is:generated"), vec!["source.md"]);
    assert_eq!(h.find("harbour is:generated"), vec!["summary.md"]);

    // And a reindex does not demote it back to a source document.
    h.index();
    assert_eq!(h.doc("summary.md").authorship, Authorship::Derived);
}

/// KB-003 — move a whole folder, with every kind of link surviving.
///
/// Three link directions are exercised: into the moved folder from outside,
/// out of it where a moving note used a folder-relative destination, and
/// within it where one moving note names another by vault path. Bare wiki
/// names must come through untouched.
#[test]
fn kb_003_moving_a_folder_rewrites_links_in_all_three_directions() {
    let mut h = Harness::new();
    h.write(
        "notes/outside.md",
        "Into the folder: [[research/deep/paper]] and [survey](../research/survey.md).\n",
    );
    h.write(
        "research/survey.md",
        "# Survey\n\nOut of the folder: [outside](../notes/outside.md).\n",
    );
    h.write(
        "research/deep/paper.md",
        "# Paper\n\nWithin the moved set: [[research/survey]].\n",
    );
    std::fs::write(h.root().join("research/deep/data.png"), [0x89, 0x50]).unwrap();
    h.write("bare.md", "By name only: [[paper]].\n");
    h.index();

    let paper_id = h.doc("research/deep/paper.md").id;
    let survey_id = h.doc("research/survey.md").id;

    let vault = h.vault.clone();
    let report = h
        .app
        .move_folder(
            &vault,
            &VaultPath::new("research").unwrap(),
            Some(&VaultPath::new("archive").unwrap()),
        )
        .unwrap();
    assert_eq!(report.to.as_str(), "archive/research");
    assert_eq!(report.documents_moved, 3, "{report:?}");
    assert!(report.unwritable.is_empty(), "{report:?}");

    // Structure preserved, identities kept, attachment moved too.
    assert_eq!(h.doc("archive/research/deep/paper.md").id, paper_id);
    assert_eq!(h.doc("archive/research/survey.md").id, survey_id);
    assert!(h.root().join("archive/research/deep/data.png").exists());

    // Direction 1: into the folder.
    let outside = h.read("notes/outside.md");
    assert!(
        outside.contains("[[archive/research/deep/paper]]"),
        "{outside}"
    );
    assert!(
        outside.contains("(archive/research/survey.md)"),
        "{outside}"
    );

    // Direction 2: out of the folder — the moved note's own relative link.
    let survey = h.read("archive/research/survey.md");
    assert!(survey.contains("(notes/outside.md)"), "{survey}");

    // Direction 3: within the moved set.
    let paper = h.read("archive/research/deep/paper.md");
    assert!(paper.contains("[[archive/research/survey]]"), "{paper}");

    // Bare names never needed rewriting.
    assert_eq!(h.read("bare.md"), "By name only: [[paper]].\n");

    // The vacated directories are gone from disk.
    assert!(!h.root().join("research").exists(), "empty chain swept");

    // And after a fresh reindex, every link still resolves to the same ids.
    h.index();
    let outside_doc = h.doc("notes/outside.md");
    let targets: Vec<_> = h
        .app
        .neighbourhood(outside_doc.id)
        .unwrap()
        .outgoing
        .iter()
        .map(|e| e.endpoints.target.document)
        .collect();
    assert!(targets.contains(&paper_id), "{targets:?}");
    assert!(targets.contains(&survey_id), "{targets:?}");
    let bare_targets: Vec<_> = h
        .app
        .neighbourhood(h.doc("bare.md").id)
        .unwrap()
        .outgoing
        .iter()
        .map(|e| e.endpoints.target.document)
        .collect();
    assert_eq!(bare_targets, vec![paper_id]);
}

/// Folder moves refuse anything that would lose or collide data, before
/// touching a single file.
#[test]
fn kb_003_folder_move_guards() {
    let mut h = Harness::new();
    h.write("research/a.md", "# A\n");
    h.write("research/deep/b.md", "# B\n");
    h.write("elsewhere/research/a.md", "# Clash\n");
    h.index();
    let vault = h.vault.clone();
    let research = VaultPath::new("research").unwrap();

    // Into its own subtree: refused.
    let err = h
        .app
        .move_folder(
            &vault,
            &research,
            Some(&VaultPath::new("research/deep").unwrap()),
        )
        .unwrap_err();
    assert!(err.to_string().contains("own subtree"), "{err}");

    // Onto a destination that already has one of the paths: refused whole.
    let err = h
        .app
        .move_folder(
            &vault,
            &research,
            Some(&VaultPath::new("elsewhere").unwrap()),
        )
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert!(h.root().join("research/a.md").exists(), "nothing moved");
    assert!(h.root().join("research/deep/b.md").exists());

    // A folder with nothing in it does not exist to move.
    let err = h
        .app
        .move_folder(&vault, &VaultPath::new("ghost").unwrap(), None)
        .unwrap_err();
    assert!(err.to_string().contains("no documents"), "{err}");

    // Moving to where it already is: a clean no-op.
    let report = h.app.move_folder(&vault, &research, None).unwrap();
    assert_eq!(report.documents_moved, 0);
    assert_eq!(report.from, report.to);
}
