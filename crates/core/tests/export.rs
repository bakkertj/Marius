//! KB-014 end to end: export a vault, re-open the copy elsewhere as a
//! fresh vault, attach the metadata — identities, review decisions and
//! asserted edges survive the trip.

use marius_core::Marius;
use marius_model::{Provenance, ReviewState, VaultPath};

#[test]
fn an_exported_vault_migrates_with_identity_and_judgement_intact() {
    // ------------------------------------------------- the old machine
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    std::fs::write(
        files.path().join("pharos.md"),
        "# Pharos\n\nThe lighthouse. #antiquity\n",
    )
    .unwrap();
    std::fs::write(files.path().join("survey.md"), "# Survey\n\nSoundings.\n").unwrap();
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("Old", files.path()).unwrap();
    app.index(&vault).unwrap();

    let pharos = app
        .document_at(vault.id, &VaultPath::new("pharos.md").unwrap())
        .unwrap()
        .unwrap();
    let survey = app
        .document_at(vault.id, &VaultPath::new("survey.md").unwrap())
        .unwrap()
        .unwrap();
    app.add_tag(pharos.id, "beacon").unwrap();
    app.reject_tag(pharos.id, "boring", Some("never")).unwrap();
    app.assert_relationship(pharos.id, survey.id).unwrap();

    let export_dir = tempfile::tempdir().unwrap();
    let sidecar = app.export_vault(&vault, export_dir.path()).unwrap();
    assert!(sidecar.ends_with("marius-export.json"));
    assert!(export_dir.path().join("files/pharos.md").exists());

    // ------------------------------------------------- the new machine
    let data2 = tempfile::tempdir().unwrap();
    let mut app2 = Marius::open(data2.path()).unwrap();
    let vault2 = app2
        .open_vault("New", export_dir.path().join("files"))
        .unwrap();
    app2.index(&vault2).unwrap();

    // Freshly indexed: new ids, no metadata yet.
    let before = app2
        .document_at(vault2.id, &VaultPath::new("pharos.md").unwrap())
        .unwrap()
        .unwrap();
    assert_ne!(before.id, pharos.id);

    let report = app2.import_metadata(&vault2, &sidecar).unwrap();
    assert_eq!(report.matched, 2);
    assert_eq!(report.identities_adopted, 2);
    assert!(report.missing.is_empty());

    // Identity survived (KB-014, KB-006).
    let after = app2
        .document_at(vault2.id, &VaultPath::new("pharos.md").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(after.id, pharos.id, "the exported identity was adopted");

    // The manual tag with its provenance.
    let tags = app2.document_tags(after.id).unwrap();
    assert!(tags.iter().any(|(t, dt)| {
        t.canonical_name.as_str() == "beacon" && dt.provenance == Provenance::Manual
    }));

    // The rejection is still a wall: a machine proposal bounces.
    let suggestion = marius_model::DocumentTag::ai_generated(
        after.id,
        marius_storage::tags::ensure(
            app2.database().conn(),
            Some(vault2.id),
            &marius_model::TagName::new("boring").unwrap(),
        )
        .unwrap()
        .id,
        "model",
        marius_model::Confidence::new(0.9),
    );
    let outcome = marius_storage::tags::assign(app2.database().conn(), &suggestion).unwrap();
    assert!(matches!(
        outcome,
        marius_storage::tags::Assigned::SkippedRejected
    ));

    // The asserted edge, pointing at the adopted ids.
    let neighbourhood = app2.neighbourhood(after.id).unwrap();
    assert!(neighbourhood
        .outgoing
        .iter()
        .chain(neighbourhood.incoming.iter())
        .any(|r| r.kind == marius_model::RelationshipKind::Asserted
            && r.review != ReviewState::Rejected));

    // And everything still searches.
    let hits = app2
        .search("lighthouse", marius_core::SearchOptions::default())
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document.id, pharos.id);
}
