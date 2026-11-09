//! IM-010 end to end: real filesystem events, the debounced watcher, and
//! the identity-preserving apply rule — no mocks.

use std::fs;
use std::time::{Duration, Instant};

use marius_core::{ExternalReport, Marius, SearchOptions};
use marius_indexing::watch::VaultWatcher;

/// Collect watcher batches until `done` says the picture is complete, or a
/// generous deadline passes (FSEvents takes its time).
fn gather(
    watcher: &VaultWatcher,
    done: impl Fn(&marius_indexing::watch::ExternalChanges) -> bool,
) -> marius_indexing::watch::ExternalChanges {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut all = marius_indexing::watch::ExternalChanges::default();
    while Instant::now() < deadline {
        if let Some(batch) = watcher.changes_timeout(Duration::from_millis(500)) {
            all.merge(batch);
            if done(&all) {
                break;
            }
        }
    }
    all
}

#[test]
fn external_edits_reach_the_index_without_a_manual_reindex() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    // FSEvents reports the canonical path; a symlinked /tmp root would make
    // every event look like it came from outside the vault.
    let root = files.path().canonicalize().unwrap();
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("Watched", &root).unwrap();
    app.index(&vault).unwrap();

    let watcher = VaultWatcher::new(&root, Duration::from_millis(250)).unwrap();

    // Another program writes a note.
    fs::write(
        root.join("meteor.md"),
        "# Meteor Shower\n\nSeen over the harbour.\n",
    )
    .unwrap();

    let changes = gather(&watcher, |c| {
        c.paths.iter().any(|p| p.as_str() == "meteor.md")
    });
    assert!(
        changes.paths.iter().any(|p| p.as_str() == "meteor.md"),
        "watcher never reported the new file: {changes:?}"
    );

    app.apply_external_changes(&vault, &changes).unwrap();
    let hits = app.search("meteor", SearchOptions::default()).unwrap().hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document.title, "Meteor Shower");
}

#[test]
fn an_external_rename_keeps_the_documents_identity() {
    // AC-003 through the watcher: mv outside the app, identity survives.
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let root = files.path().canonicalize().unwrap();
    fs::write(root.join("original.md"), "# Original\n\nBody.\n").unwrap();

    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("Watched", &root).unwrap();
    app.index(&vault).unwrap();
    let before = app
        .document_at(
            vault.id,
            &marius_model::VaultPath::new("original.md").unwrap(),
        )
        .unwrap()
        .unwrap();

    let watcher = VaultWatcher::new(&root, Duration::from_millis(250)).unwrap();
    fs::rename(root.join("original.md"), root.join("renamed.md")).unwrap();

    let changes = gather(&watcher, |c| {
        c.paths.iter().any(|p| p.as_str() == "renamed.md")
    });
    assert!(
        changes.paths.iter().any(|p| p.as_str() == "renamed.md"),
        "watcher never reported the rename: {changes:?}"
    );

    // One side of the rename no longer exists, so the apply rule must take
    // the full-reconcile road, which detects the move by content hash.
    let report = app.apply_external_changes(&vault, &changes).unwrap();
    assert!(
        matches!(report, ExternalReport::Reindexed(_)),
        "a rename must reconcile, not tombstone: {report:?}"
    );

    let after = app
        .document_at(
            vault.id,
            &marius_model::VaultPath::new("renamed.md").unwrap(),
        )
        .unwrap()
        .expect("renamed document is indexed");
    assert_eq!(after.id, before.id, "identity lost across external rename");
    assert!(app
        .document_at(
            vault.id,
            &marius_model::VaultPath::new("original.md").unwrap()
        )
        .unwrap()
        .is_none());
}
