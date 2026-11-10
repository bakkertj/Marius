//! Probe: a remote model marked trusted_for_personal, saved and reloaded
//! the way the UI worker reloads it, must pass policy for a personal note.

use marius_core::config::{ModelChoice, ModelSettings};
use marius_core::{Config, Marius};
use marius_model::Sensitivity;

#[test]
fn a_trusted_remote_model_passes_policy_for_personal_notes() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    std::fs::write(files.path().join("n.md"), "# Note\n\nWords here.\n").unwrap();

    {
        let mut app = Marius::open(data.path()).unwrap();
        let vault = app.open_vault("T", files.path()).unwrap();
        app.index(&vault).unwrap();
        let mut policy = app.vault_policy(vault.id).unwrap();
        policy.default_sensitivity = Sensitivity::Personal;
        app.set_vault_policy(policy).unwrap();
        app.set_config(Config {
            models: ModelSettings {
                tagging: Some(ModelChoice {
                    provider: "work".into(),
                    model: "gpt-oss-120b".into(),
                    credential: None,
                    base_url: Some("http://127.0.0.1:9".into()), // nothing listens
                    locality: None,
                    trusted_for_personal: true,
                }),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    }

    // Fresh open from disk, exactly like the UI's worker thread.
    let mut app = Marius::open(data.path()).unwrap();
    let choice = app.config().models.tagging.clone().unwrap();
    assert!(choice.trusted_for_personal, "trusted flag lost on reload");

    let vault = app.vaults().unwrap().remove(0);
    let doc = marius_storage::documents::all_in_vault(app.database().conn(), vault.id)
        .unwrap()
        .remove(0);
    let result = app.suggest_tags(doc.id, true);
    // Policy must approve; the only acceptable failure is the dead socket.
    match result {
        Ok(suggested) => {
            assert!(
                !suggested.reason.contains("may not see"),
                "policy refused a trusted remote model: {}",
                suggested.reason
            );
        }
        Err(error) => {
            let text = error.to_string();
            assert!(
                !text.contains("may not see"),
                "policy refused a trusted remote model: {text}"
            );
        }
    }
}
