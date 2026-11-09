//! Deciding what changed between the disk and the index.
//!
//! Requirements: KB-005 (**detect external filesystem changes and reconcile
//! them with the local index**), KB-006 (identity survives a move), AC-003,
//! NFR-003, integrity rule 1.
//!
//! ## Why a move is detected, not inferred from the path
//!
//! If a user moves `notes/a.md` to `archive/a.md` in Finder, a path-based
//! index sees a deletion and a creation. That would mint a new document ID and
//! break every `[[link]]`, backlink, tag assignment and graph edge pointing at
//! it — exactly what KB-006 and integrity rule 1 forbid. So a disappeared path
//! and an appeared path with the *same content hash* are reconciled as one
//! document that moved, and its identity is preserved.
//!
//! ## Why the hash is the arbiter
//!
//! Modification times are compared first because they are nearly free, but
//! they are never trusted on their own: restores, sync tools, archive
//! extraction and clock changes all produce times that lie in both directions.
//! A timestamp or size difference therefore only decides *whether to hash*.
//!
//! ## The one case the quick path cannot see
//!
//! [`Thoroughness::Quick`] skips the hash when size *and* modification time
//! both match, which is what keeps NFR-003's two-second vault open reachable:
//! a full hash means reading every byte of every note on every open, which on a
//! network drive or a 100,000-document vault (NFR-005) is far too slow to do
//! routinely.
//!
//! The cost is that an edit which changes content while preserving both size
//! and mtime to the second is invisible — a `touch -r`, a tool that restores
//! timestamps after writing, or a same-length in-place edit. This is the same
//! trade-off `git status` and `rsync` make by default. [`Thoroughness::Full`]
//! hashes unconditionally and is what an explicit reindex uses, so the
//! situation is always recoverable without losing anything.

use std::collections::HashMap;

use marius_model::{ContentHash, Document, DocumentId, VaultId, VaultPath};
use marius_storage::{documents, Connection, Result as StorageResult};

use crate::files;
use crate::scan::ScannedFile;

/// One reconciliation action.
#[derive(Debug, Clone)]
pub enum Change {
    /// A file with no corresponding document.
    Added {
        file: ScannedFile,
        hash: Option<ContentHash>,
    },
    /// A known document whose content changed on disk.
    Modified {
        document: Document,
        file: ScannedFile,
    },
    /// A known document that appeared at a new path with identical content.
    /// Its identity is preserved (AC-003).
    Moved {
        document: Document,
        file: ScannedFile,
    },
    /// An indexed document whose file is gone.
    Removed { document: Document },
}

impl Change {
    /// A short verb for progress output.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Added { .. } => "added",
            Self::Modified { .. } => "modified",
            Self::Moved { .. } => "moved",
            Self::Removed { .. } => "removed",
        }
    }

    /// The document this change concerns, when it already has one.
    pub fn document_id(&self) -> Option<DocumentId> {
        match self {
            Self::Added { .. } => None,
            Self::Modified { document, .. }
            | Self::Moved { document, .. }
            | Self::Removed { document } => Some(document.id),
        }
    }
}

/// How hard to look for changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Thoroughness {
    /// Trust matching size and modification time; hash only when they differ.
    /// The default, and what opening a vault uses.
    #[default]
    Quick,
    /// Hash every file regardless of its metadata. Slower, and the answer to
    /// anything the quick path could have missed.
    Full,
    /// Treat every file as changed, whatever its content.
    ///
    /// Used after the derived indexes have been dropped (NFR-007): the
    /// documents are genuinely unchanged, so change detection correctly reports
    /// nothing to do — but the search index and passages have to be written
    /// again from scratch, so every document must pass through the pipeline.
    Rebuild,
}

/// Everything that needs doing, plus what was already current.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub changes: Vec<Change>,
    /// Documents that are already up to date. Counted, not listed, because on
    /// a re-index of a large vault this is nearly everything.
    pub unchanged: usize,
    /// Files that could not be hashed, with why (NFR-006).
    pub problems: Vec<(VaultPath, String)>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn count_of(&self, verb: &str) -> usize {
        self.changes.iter().filter(|c| c.verb() == verb).count()
    }

    /// One-line summary for a progress report.
    pub fn summary(&self) -> String {
        format!(
            "{} added, {} modified, {} moved, {} removed, {} unchanged",
            self.count_of("added"),
            self.count_of("modified"),
            self.count_of("moved"),
            self.count_of("removed"),
            self.unchanged
        )
    }
}

/// Work out what changed between `scanned` and what the index holds, trusting
/// filesystem metadata where it can.
pub fn plan(conn: &Connection, vault: VaultId, scanned: &[ScannedFile]) -> StorageResult<Plan> {
    plan_with(conn, vault, scanned, Thoroughness::Quick)
}

/// Work out what changed, at the given thoroughness.
pub fn plan_with(
    conn: &Connection,
    vault: VaultId,
    scanned: &[ScannedFile],
    thoroughness: Thoroughness,
) -> StorageResult<Plan> {
    let mut indexed: HashMap<VaultPath, Document> = HashMap::new();
    for document in documents::all_in_vault(conn, vault)? {
        indexed.insert(document.path.clone(), document);
    }

    let mut plan = Plan::default();
    // Files present on disk with no indexed document at that path. Resolved
    // into moves or additions once the removals are known.
    let mut appeared: Vec<&ScannedFile> = Vec::new();

    for file in scanned {
        match indexed.remove(&file.path) {
            Some(document) => {
                if thoroughness == Thoroughness::Rebuild {
                    plan.changes.push(Change::Modified {
                        document,
                        file: file.clone(),
                    });
                    continue;
                }
                // Cheap comparison first; hash only when it disagrees.
                if thoroughness == Thoroughness::Quick
                    && document.size_bytes == file.size_bytes
                    && same_instant(document.modified_at, file.modified)
                {
                    plan.unchanged += 1;
                    continue;
                }
                match hash_of(file) {
                    Ok(hash) if hash == document.content_hash => {
                        // Touched but not changed — a sync tool or a restore.
                        plan.unchanged += 1;
                    }
                    Ok(_) => plan.changes.push(Change::Modified {
                        document,
                        file: file.clone(),
                    }),
                    Err(error) => plan.problems.push((file.path.clone(), error)),
                }
            }
            None => appeared.push(file),
        }
    }

    // Whatever is still in `indexed` has no file on disk.
    let disappeared: Vec<Document> = indexed.into_values().collect();
    let mut unmatched_removals: HashMap<ContentHash, Vec<Document>> = HashMap::new();
    for document in disappeared {
        unmatched_removals
            .entry(document.content_hash)
            .or_default()
            .push(document);
    }

    for file in appeared {
        // Only hash a new file when there is a removal it could be matched
        // against; on a first index of 10,000 files there are none, and this
        // keeps NFR-003 reachable.
        let hash = if unmatched_removals.is_empty() && thoroughness != Thoroughness::Full {
            None
        } else {
            match hash_of(file) {
                Ok(hash) => Some(hash),
                Err(error) => {
                    plan.problems.push((file.path.clone(), error));
                    continue;
                }
            }
        };

        let moved = hash.and_then(|hash| {
            unmatched_removals.get_mut(&hash).and_then(|candidates| {
                // Prefer the candidate in the same folder: a file moved within
                // a directory is more likely a rename of that one than of an
                // identical file elsewhere.
                let pick = candidates
                    .iter()
                    .position(|d| d.path.parent() == file.path.parent())
                    .unwrap_or(0);
                (!candidates.is_empty()).then(|| candidates.remove(pick))
            })
        });

        match moved {
            Some(document) => plan.changes.push(Change::Moved {
                document,
                file: file.clone(),
            }),
            None => plan.changes.push(Change::Added {
                file: file.clone(),
                hash,
            }),
        }
    }

    for document in unmatched_removals.into_values().flatten() {
        plan.changes.push(Change::Removed { document });
    }

    // Removals last, additions and moves first: completing a link to a new
    // document should not race a deletion of the old one.
    plan.changes.sort_by_key(|c| match c {
        Change::Moved { .. } => 0,
        Change::Added { .. } => 1,
        Change::Modified { .. } => 2,
        Change::Removed { .. } => 3,
    });
    Ok(plan)
}

fn hash_of(file: &ScannedFile) -> std::result::Result<ContentHash, String> {
    files::hash_file(&file.absolute)
        .map(|(hash, _)| hash)
        .map_err(|e| e.to_string())
}

/// Compare timestamps at one-second resolution.
///
/// Filesystems disagree about sub-second precision — HFS+ stored whole
/// seconds, some network filesystems round — so finer comparison would report
/// spurious modifications on every scan.
fn same_instant(a: time::OffsetDateTime, b: time::OffsetDateTime) -> bool {
    a.unix_timestamp() == b.unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{self, ScanOptions};
    use marius_model::{Authorship, Vault, VaultPolicy};
    use marius_storage::{vaults, Database};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use time::OffsetDateTime;

    struct Fixture {
        db: Database,
        vault: VaultId,
        dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::new_local("V", dir.path());
        vaults::upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        Fixture {
            db,
            vault: vault.id,
            dir,
        }
    }

    impl Fixture {
        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, relative: &str, text: &str) {
            files::write_atomic(&self.root().join(relative), text).unwrap();
        }

        fn scanned(&self) -> Vec<ScannedFile> {
            scan::scan(self.root(), &ScanOptions::default()).files
        }

        fn plan(&self) -> Plan {
            plan(self.db.conn(), self.vault, &self.scanned()).unwrap()
        }

        /// Record a document exactly as the indexer would, so later plans see
        /// it as current.
        fn record(&self, relative: &str) -> Document {
            let file = self
                .scanned()
                .into_iter()
                .find(|f| f.path.as_str() == relative)
                .expect("file must exist on disk");
            let contents = files::read_text(&file.absolute).unwrap();
            let now = OffsetDateTime::now_utc();
            let document = Document {
                id: DocumentId::new(),
                vault: self.vault,
                path: file.path.clone(),
                title: file.path.file_stem().to_owned(),
                kind: file.kind,
                authorship: Authorship::Source,
                content_hash: contents.hash,
                size_bytes: contents.size_bytes,
                created_at: now,
                modified_at: file.modified,
                indexed_at: now,
                archived_at: None,
                sensitivity: None,
                frontmatter: BTreeMap::new(),
                external: false,
            };
            documents::upsert(self.db.conn(), &document).unwrap();
            document
        }
    }

    #[test]
    fn a_first_index_sees_everything_as_added() {
        let f = fixture();
        f.write("a.md", "# A\n");
        f.write("notes/b.md", "# B\n");

        let plan = f.plan();
        assert_eq!(plan.count_of("added"), 2);
        assert_eq!(plan.unchanged, 0);
        assert!(plan.problems.is_empty());
    }

    #[test]
    fn a_first_index_does_not_hash_files_it_has_no_reason_to() {
        // NFR-003: with no removals there is nothing a hash could match.
        let f = fixture();
        f.write("a.md", "# A\n");
        let plan = f.plan();
        match &plan.changes[0] {
            Change::Added { hash, .. } => assert!(hash.is_none()),
            other => panic!("expected an addition, got {other:?}"),
        }
    }

    #[test]
    fn an_unchanged_file_is_recognised_without_rehashing() {
        let f = fixture();
        f.write("a.md", "# A\n");
        f.record("a.md");

        let plan = f.plan();
        assert!(plan.is_empty(), "{}", plan.summary());
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn an_external_edit_is_detected() {
        // KB-005 — the headline requirement.
        let f = fixture();
        f.write("a.md", "# A\n");
        f.record("a.md");
        f.write("a.md", "# A\n\nedited in another editor\n");

        let plan = f.plan();
        assert_eq!(plan.count_of("modified"), 1);
        assert_eq!(plan.unchanged, 0);
    }

    #[test]
    fn a_touched_but_unchanged_file_is_not_reindexed() {
        // A sync tool or a restore updates mtime without changing content.
        let f = fixture();
        f.write("a.md", "# A\n");
        let document = f.record("a.md");

        // Simulate a stale recorded mtime while content is identical.
        let mut stale = document.clone();
        stale.modified_at = document.modified_at - time::Duration::hours(3);
        documents::upsert(f.db.conn(), &stale).unwrap();

        let plan = f.plan();
        assert!(plan.is_empty(), "{}", plan.summary());
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn a_same_size_same_mtime_edit_needs_a_full_pass() {
        // The documented limit of the quick path, asserted in both directions
        // so a future change to either cannot pass unnoticed.
        let f = fixture();
        f.write("a.md", "aaaa");
        let document = f.record("a.md");
        f.write("a.md", "bbbb");

        // Make metadata agree exactly, so only a hash could tell the difference.
        let scanned = f.scanned();
        let mut indistinguishable = document.clone();
        indistinguishable.modified_at = scanned[0].modified;
        indistinguishable.size_bytes = scanned[0].size_bytes;
        documents::upsert(f.db.conn(), &indistinguishable).unwrap();

        let quick = plan_with(f.db.conn(), f.vault, &scanned, Thoroughness::Quick).unwrap();
        assert_eq!(quick.count_of("modified"), 0, "quick trusts metadata");
        assert_eq!(quick.unchanged, 1);

        let full = plan_with(f.db.conn(), f.vault, &scanned, Thoroughness::Full).unwrap();
        assert_eq!(full.count_of("modified"), 1, "a full pass always hashes");
    }

    #[test]
    fn a_rebuild_pass_reports_every_document_as_changed() {
        // NFR-007: the derived indexes are gone, so each document has to be
        // written again even though its content is identical.
        let f = fixture();
        f.write("a.md", "# A\n");
        f.write("b.md", "# B\n");
        f.record("a.md");
        f.record("b.md");

        assert!(f.plan().is_empty(), "nothing has actually changed");
        let rebuild = plan_with(f.db.conn(), f.vault, &f.scanned(), Thoroughness::Rebuild).unwrap();
        assert_eq!(rebuild.count_of("modified"), 2, "{}", rebuild.summary());
        assert_eq!(rebuild.unchanged, 0);
    }

    #[test]
    fn a_size_changing_edit_is_caught_by_the_quick_path() {
        let f = fixture();
        f.write("a.md", "short");
        let document = f.record("a.md");
        f.write("a.md", "considerably longer than before");

        // Even with a preserved mtime, the size difference forces a hash.
        let scanned = f.scanned();
        let mut preserved_mtime = document.clone();
        preserved_mtime.modified_at = scanned[0].modified;
        documents::upsert(f.db.conn(), &preserved_mtime).unwrap();

        let plan = plan(f.db.conn(), f.vault, &scanned).unwrap();
        assert_eq!(plan.count_of("modified"), 1, "{}", plan.summary());
    }

    #[test]
    fn a_file_moved_outside_the_app_keeps_its_identity() {
        // AC-003, KB-006, integrity rule 1 — the reason move detection exists.
        let f = fixture();
        f.write("notes/a.md", "# A\n\nstable content\n");
        let original = f.record("notes/a.md");

        fs::create_dir_all(f.root().join("archive")).unwrap();
        fs::rename(f.root().join("notes/a.md"), f.root().join("archive/a.md")).unwrap();

        let plan = f.plan();
        assert_eq!(plan.count_of("moved"), 1, "{}", plan.summary());
        assert_eq!(plan.count_of("added"), 0);
        assert_eq!(plan.count_of("removed"), 0);
        match &plan.changes[0] {
            Change::Moved { document, file } => {
                assert_eq!(document.id, original.id, "identity must be preserved");
                assert_eq!(file.path.as_str(), "archive/a.md");
            }
            other => panic!("expected a move, got {other:?}"),
        }
    }

    #[test]
    fn a_rename_within_a_folder_is_a_move() {
        let f = fixture();
        f.write("notes/old.md", "# Content\n");
        let original = f.record("notes/old.md");
        fs::rename(f.root().join("notes/old.md"), f.root().join("notes/new.md")).unwrap();

        let plan = f.plan();
        assert_eq!(plan.count_of("moved"), 1);
        assert_eq!(plan.changes[0].document_id(), Some(original.id));
    }

    #[test]
    fn a_rename_prefers_the_candidate_in_the_same_folder() {
        // Two identical files; the one in the same directory is the likelier
        // match for a rename.
        let f = fixture();
        f.write("notes/a.md", "identical\n");
        f.write("other/a.md", "identical\n");
        let in_notes = f.record("notes/a.md");
        f.record("other/a.md");

        fs::rename(
            f.root().join("notes/a.md"),
            f.root().join("notes/renamed.md"),
        )
        .unwrap();

        let plan = f.plan();
        let moved = plan
            .changes
            .iter()
            .find(|c| matches!(c, Change::Moved { .. }))
            .expect("a move");
        assert_eq!(moved.document_id(), Some(in_notes.id));
    }

    #[test]
    fn a_deletion_is_detected() {
        let f = fixture();
        f.write("a.md", "# A\n");
        let document = f.record("a.md");
        fs::remove_file(f.root().join("a.md")).unwrap();

        let plan = f.plan();
        assert_eq!(plan.count_of("removed"), 1);
        assert_eq!(plan.changes[0].document_id(), Some(document.id));
    }

    #[test]
    fn a_deletion_and_an_unrelated_creation_are_not_mistaken_for_a_move() {
        let f = fixture();
        f.write("gone.md", "original content\n");
        let removed = f.record("gone.md");
        fs::remove_file(f.root().join("gone.md")).unwrap();
        f.write("new.md", "completely different content\n");

        let plan = f.plan();
        assert_eq!(plan.count_of("moved"), 0, "{}", plan.summary());
        assert_eq!(plan.count_of("added"), 1);
        assert_eq!(plan.count_of("removed"), 1);
        assert!(plan
            .changes
            .iter()
            .any(|c| c.document_id() == Some(removed.id)));
    }

    #[test]
    fn a_copy_leaves_the_original_in_place_and_adds_the_copy() {
        // Identical content, but the original still exists, so nothing moved.
        let f = fixture();
        f.write("a.md", "identical\n");
        let original = f.record("a.md");
        f.write("b.md", "identical\n");

        let plan = f.plan();
        assert_eq!(plan.count_of("moved"), 0);
        assert_eq!(plan.count_of("added"), 1);
        assert_eq!(plan.unchanged, 1);
        assert!(documents::get(f.db.conn(), original.id).unwrap().is_some());
    }

    #[test]
    fn moves_are_applied_before_removals() {
        // Otherwise a link completed to the moved document could race the
        // deletion of its old row.
        let f = fixture();
        f.write("a.md", "moving content\n");
        f.write("gone.md", "vanishing content\n");
        f.record("a.md");
        f.record("gone.md");
        fs::rename(f.root().join("a.md"), f.root().join("b.md")).unwrap();
        fs::remove_file(f.root().join("gone.md")).unwrap();

        let plan = f.plan();
        let verbs: Vec<&str> = plan.changes.iter().map(Change::verb).collect();
        assert_eq!(verbs, vec!["moved", "removed"]);
    }

    #[test]
    fn an_unreadable_file_is_reported_rather_than_dropped_silently() {
        let f = fixture();
        f.write("a.md", "# A\n");
        let document = f.record("a.md");

        // Point the index at content that no longer matches, then make the file
        // unreadable by removing it between scan and hash.
        let scanned = f.scanned();
        let mut changed = document.clone();
        changed.size_bytes += 10;
        documents::upsert(f.db.conn(), &changed).unwrap();
        fs::remove_file(f.root().join("a.md")).unwrap();

        let plan = plan(f.db.conn(), f.vault, &scanned).unwrap();
        assert_eq!(plan.problems.len(), 1, "{plan:#?}");
        assert_eq!(plan.problems[0].0.as_str(), "a.md");
    }

    #[test]
    fn the_summary_reads_correctly() {
        let f = fixture();
        f.write("a.md", "# A\n");
        assert_eq!(
            f.plan().summary(),
            "1 added, 0 modified, 0 moved, 0 removed, 0 unchanged"
        );
    }

    #[test]
    fn a_reconcile_of_an_empty_vault_does_nothing() {
        let f = fixture();
        let plan = f.plan();
        assert!(plan.is_empty());
        assert_eq!(plan.unchanged, 0);
    }
}
