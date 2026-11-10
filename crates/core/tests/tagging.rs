//! TAG-005 / AI-005..AI-015 end to end: the policy engine, the audit log,
//! the Ollama protocol over a real loopback socket, and the storage gates —
//! no mocks, no network beyond 127.0.0.1.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use marius_core::config::{ModelChoice, ModelSettings};
use marius_core::{Config, Marius};
use marius_model::{InferenceOutcome, Provenance, ReviewState, Sensitivity, VaultPath};

/// A one-request Ollama look-alike on a loopback port.
fn fake_ollama(reply_json: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = rest.trim().parse().unwrap();
            }
            let done = line == "\r\n";
            request.push_str(&line);
            if done {
                break;
            }
        }
        let mut payload = vec![0u8; content_length];
        reader.read_exact(&mut payload).unwrap();
        request.push_str(&String::from_utf8_lossy(&payload));
        let body = format!(
            r#"{{"model":"test-model","message":{{"role":"assistant","content":{reply_json}}},"prompt_eval_count":50,"eval_count":9,"done":true}}"#
        );
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        request
    });
    (format!("http://{addr}"), handle)
}

struct Harness {
    app: Marius,
    vault: marius_model::Vault,
    _data: tempfile::TempDir,
    _files: tempfile::TempDir,
}

fn harness(base_url: Option<String>) -> Harness {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    std::fs::write(
        files.path().join("pharos.md"),
        "# The Pharos\n\nThe great lighthouse of Alexandria. #lighthouse\n",
    )
    .unwrap();
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("T", files.path()).unwrap();
    app.index(&vault).unwrap();
    if let Some(base_url) = base_url {
        app.set_config(Config {
            models: ModelSettings {
                tagging: Some(ModelChoice {
                    provider: "ollama".into(),
                    model: "test-model".into(),
                    credential: None,
                    base_url: Some(base_url),
                    locality: None,
                    trusted_for_personal: false,
                }),
                ..ModelSettings::default()
            },
            ..Config::default()
        })
        .unwrap();
    }
    Harness {
        app,
        vault,
        _data: data,
        _files: files,
    }
}

fn doc_id(h: &Harness) -> marius_model::DocumentId {
    h.app
        .document_at(h.vault.id, &VaultPath::new("pharos.md").unwrap())
        .unwrap()
        .unwrap()
        .id
}

#[test]
fn suggested_tags_arrive_with_provenance_and_an_audit_row() {
    let (base, server) = fake_ollama(r#""[\"maritime-history\", \"alexandria\"]""#);
    let mut h = harness(Some(base));
    let id = doc_id(&h);

    let result = h.app.suggest_tags(id, true).unwrap();
    assert_eq!(result.applied.len(), 2);
    assert!(result.skipped.is_empty());

    // The prompt carried the vault's existing vocabulary (TAG-008).
    let sent = server.join().unwrap();
    assert!(sent.contains("lighthouse"), "vocabulary shown to the model");
    assert!(sent.contains("The great lighthouse"), "body shown");

    // Stored as machine proposals, unreviewed, with model and confidence.
    let tags = h.app.document_tags(id).unwrap();
    let proposed: Vec<_> = tags
        .iter()
        .filter(|(_, dt)| dt.provenance == Provenance::AiGenerated)
        .collect();
    assert_eq!(proposed.len(), 2);
    for (_, dt) in &proposed {
        assert_eq!(dt.review, ReviewState::Unreviewed);
        assert_eq!(dt.model.as_deref(), Some("test-model"));
        assert!(dt.confidence.is_some());
    }

    // The audit row records the run (AI-015).
    let log = h.app.inference_log(10).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, InferenceOutcome::Completed);
    assert_eq!(log[0].provider.as_deref(), Some("ollama"));
    assert_eq!(log[0].prompt_tokens, Some(50));
    assert!(!log[0].left_the_device(), "local run transmitted nothing");
}

#[test]
fn a_rejected_tag_stays_rejected_across_reclassification() {
    // TAG-012 / integrity rule 7, now with a real classifier in the loop.
    let (base, _server) = fake_ollama(r#""[\"maritime-history\"]""#);
    let mut h = harness(Some(base));
    let id = doc_id(&h);
    h.app.suggest_tags(id, true).unwrap();
    h.app
        .reject_tag(id, "maritime-history", Some("wrong era"))
        .unwrap();

    let (base2, _server2) = fake_ollama(r#""[\"maritime-history\"]""#);
    let mut config = h.app.config().clone();
    config.models.tagging.as_mut().unwrap().base_url = Some(base2);
    h.app.set_config(config).unwrap();

    let result = h.app.suggest_tags(id, true).unwrap();
    assert_eq!(result.applied, Vec::<marius_model::TagName>::new());
    assert_eq!(result.skipped.len(), 1, "the proposal was gated out");
    assert!(h
        .app
        .document_tags(id)
        .unwrap()
        .iter()
        .all(|(tag, _)| tag.canonical_name.as_str() != "maritime-history"));
}

#[test]
fn confidential_content_is_refused_before_any_socket_opens() {
    // AC-016: the "provider" here is remote by declaration; nothing may
    // listen — so a refusal that tried to connect would error, not refuse.
    let mut h = harness(None);
    let mut config = Config::default();
    config.models.tagging = Some(ModelChoice {
        provider: "openrouter".into(),
        model: "big".into(),
        credential: Some("NOT_SET".into()),
        base_url: Some("http://127.0.0.1:1".into()),
        locality: None,
        trusted_for_personal: true,
    });
    h.app.set_config(config).unwrap();

    let mut policy = h.app.vault_policy(h.vault.id).unwrap();
    policy.default_sensitivity = Sensitivity::Confidential;
    h.app.set_vault_policy(policy).unwrap();

    let id = doc_id(&h);
    let result = h.app.suggest_tags(id, true).unwrap();
    assert!(result.suggestions.is_empty());
    assert!(result.reason.contains("may not see confidential"));

    let log = h.app.inference_log(10).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, InferenceOutcome::NoEligibleProvider);
    assert_eq!(log[0].provider, None, "no provider was selected");
}

#[test]
fn no_configured_model_degrades_with_a_useful_reason() {
    // AI-004: core features unaffected, the refusal says what to do.
    let mut h = harness(None);
    let id = doc_id(&h);
    let result = h.app.suggest_tags(id, false).unwrap();
    assert!(result.reason.contains("no provider is configured"));
    assert_eq!(
        h.app.inference_log(10).unwrap()[0].outcome,
        InferenceOutcome::NoEligibleProvider
    );
}

#[test]
fn no_inference_material_is_denied_not_merely_ineligible() {
    let (base, _server) = fake_ollama(r#""[\"x\"]""#);
    let mut h = harness(Some(base));
    let mut policy = h.app.vault_policy(h.vault.id).unwrap();
    policy.default_sensitivity = Sensitivity::NoInference;
    h.app.set_vault_policy(policy).unwrap();

    let result = h.app.suggest_tags(doc_id(&h), true).unwrap();
    assert!(result.suggestions.is_empty());
    assert_eq!(
        h.app.inference_log(10).unwrap()[0].outcome,
        InferenceOutcome::DeniedByPolicy
    );
}
