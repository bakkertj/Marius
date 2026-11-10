//! Plugins against the real application (PLUG-001, 005, 007, 008):
//! a JS plugin on disk, a real vault, and every file operation routed
//! through the document service.

use std::cell::RefCell;
use std::rc::Rc;

use marius_core::plugins::run_plugin_command;
use marius_core::{Marius, SearchOptions};
use marius_model::Capability;

struct Harness {
    app: Rc<RefCell<Marius>>,
    vault: marius_model::Vault,
    _data: tempfile::TempDir,
    _files: tempfile::TempDir,
    plugin_dir: tempfile::TempDir,
}

fn harness(main_js: &str) -> Harness {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    std::fs::write(
        files.path().join("pharos.md"),
        "# Pharos\n\nThe lighthouse stood sixteen centuries.\n",
    )
    .unwrap();
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("P", files.path()).unwrap();
    app.index(&vault).unwrap();

    let plugin_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        plugin_dir.path().join("manifest.json"),
        r#"{ "id": "digest", "name": "Digest", "version": "0.1.0" }"#,
    )
    .unwrap();
    std::fs::write(plugin_dir.path().join("main.js"), main_js).unwrap();
    app.install_plugin(plugin_dir.path()).unwrap();

    Harness {
        app: Rc::new(RefCell::new(app)),
        vault,
        _data: data,
        _files: files,
        plugin_dir,
    }
}

const DIGEST_JS: &str = r##"
marius.registerCommand("digest", () => {
    const hits = marius.search("lighthouse");
    let out = "# Digest\n\n";
    for (const hit of hits) {
        out += "- [[" + hit.title + "]]\n";
    }
    marius.write("digest.md", out);
    marius.log("wrote digest with " + hits.length + " entries");
    return String(hits.length);
});
"##;

#[test]
fn an_installed_plugin_starts_powerless_and_runs_once_trusted() {
    let h = harness(DIGEST_JS);
    let vault = h.vault.id;
    let _ = &h.plugin_dir;

    // Installed but disabled: refuses to run (PLUG-001).
    let error = run_plugin_command(h.app.clone(), vault, "digest", "digest").unwrap_err();
    assert!(error.to_string().contains("not enabled"));

    // Enabled but ungranted: the door refuses (PLUG-007).
    h.app
        .borrow_mut()
        .set_plugin_enabled("digest", true)
        .unwrap();
    let error = run_plugin_command(h.app.clone(), vault, "digest", "digest").unwrap_err();
    assert!(error.to_string().contains("not granted"), "{error}");

    // Granted: the digest is written through the document service and is
    // immediately indexed like any other note (PLUG-005).
    {
        let mut app = h.app.borrow_mut();
        app.set_plugin_capability("digest", Capability::ReadVault, true)
            .unwrap();
        app.set_plugin_capability("digest", Capability::WriteVault, true)
            .unwrap();
    }
    let (output, log) = run_plugin_command(h.app.clone(), vault, "digest", "digest").unwrap();
    assert_eq!(output, "1");
    assert_eq!(log, ["[digest] wrote digest with 1 entries"]);

    let app = h.app.borrow();
    let digest = app
        .document_at(vault, &marius_model::VaultPath::new("digest.md").unwrap())
        .unwrap()
        .expect("digest note exists");
    assert_eq!(digest.title, "Digest");
    let hits = app.search("digest", SearchOptions::default()).unwrap().hits;
    assert!(hits.iter().any(|h| h.document.id == digest.id));
}

#[test]
fn a_crashing_plugin_leaves_the_application_standing() {
    // PLUG-008 at the application level.
    let h = harness(r#"marius.registerCommand("boom", () => { undefined.x; });"#);
    {
        let mut app = h.app.borrow_mut();
        app.set_plugin_enabled("digest", true).unwrap();
    }
    let error = run_plugin_command(h.app.clone(), h.vault.id, "digest", "boom").unwrap_err();
    assert!(error.to_string().contains("plugin failed"));
    // The application is fine: ordinary work continues.
    let hits = h
        .app
        .borrow()
        .search("lighthouse", SearchOptions::default())
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1);
}

#[test]
fn revoking_a_capability_takes_effect_on_the_next_run() {
    let h = harness(DIGEST_JS);
    {
        let mut app = h.app.borrow_mut();
        app.set_plugin_enabled("digest", true).unwrap();
        app.set_plugin_capability("digest", Capability::ReadVault, true)
            .unwrap();
        app.set_plugin_capability("digest", Capability::WriteVault, true)
            .unwrap();
    }
    run_plugin_command(h.app.clone(), h.vault.id, "digest", "digest").unwrap();
    h.app
        .borrow_mut()
        .set_plugin_capability("digest", Capability::WriteVault, false)
        .unwrap();
    let error = run_plugin_command(h.app.clone(), h.vault.id, "digest", "digest").unwrap_err();
    assert!(error.to_string().contains("write_vault"), "{error}");
}
