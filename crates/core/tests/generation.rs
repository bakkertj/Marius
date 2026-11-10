//! Generated notes and relationship discovery, end to end (AI-011..014,
//! AC-010, GRAPH-007/011): a protocol-faithful daemon on a loopback port,
//! real files, and the integrity-rule-6 gate exercised for real.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use marius_core::config::{ModelChoice, ModelSettings};
use marius_core::{Config, Marius, Summarized};
use marius_model::{Authorship, Provenance, RelationshipKind, ReviewState, Sensitivity, VaultPath};

/// Ollama-shaped daemon: /api/chat answers `chat_reply`, /api/embed
/// embeds by counting topic words so similarity is real geometry.
fn daemon(chat_reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut content_length = 0usize;
            let mut first_line = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                if first_line.is_empty() {
                    first_line = line.clone();
                }
                if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = rest.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut payload = vec![0u8; content_length];
            reader.read_exact(&mut payload).unwrap();
            let body = if first_line.contains("/api/chat") {
                serde_json::json!({
                    "model": "fake-model",
                    "message": { "role": "assistant", "content": chat_reply },
                    "prompt_eval_count": 60, "eval_count": 40, "done": true
                })
                .to_string()
            } else {
                let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                let vectors: Vec<Vec<f32>> = request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| {
                        let lower = v.as_str().unwrap().to_lowercase();
                        let count = |w: &str| lower.matches(w).count() as f32;
                        vec![
                            1.0 + count("lighthouse") * 10.0,
                            1.0 + count("recipe") * 10.0,
                        ]
                    })
                    .collect();
                serde_json::json!({ "model": "fake-embed", "embeddings": vectors }).to_string()
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    format!("http://{addr}")
}

struct Harness {
    app: Marius,
    vault: marius_model::Vault,
    _data: tempfile::TempDir,
    _files: tempfile::TempDir,
}

fn harness(chat_reply: &'static str) -> Harness {
    let base = daemon(chat_reply);
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    for (name, text) in [
        (
            "pharos.md",
            "# Pharos\n\nThe lighthouse of Alexandria guided ships for sixteen centuries.\n",
        ),
        (
            "michel.md",
            "# Marius Michel Pasha\n\nHe built more than a hundred lighthouse towers on the Ottoman coast.\n",
        ),
        ("dinner.md", "# Dinner\n\nA recipe for lentil soup, nothing more.\n"),
    ] {
        std::fs::write(files.path().join(name), text).unwrap();
    }
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("G", files.path()).unwrap();
    app.index(&vault).unwrap();
    let choice = |model: &str| ModelChoice {
        provider: "ollama".into(),
        model: model.into(),
        credential: None,
        base_url: Some(base.clone()),
        locality: None,
        trusted_for_personal: false,
    };
    app.set_config(Config {
        models: ModelSettings {
            summarization: Some(choice("fake-model")),
            tagging: Some(choice("fake-model")),
            embedding: Some(choice("fake-embed")),
            ..ModelSettings::default()
        },
        ..Config::default()
    })
    .unwrap();
    Harness {
        app,
        vault,
        _data: data,
        _files: files,
    }
}

fn doc(h: &Harness, path: &str) -> marius_model::Document {
    h.app
        .document_at(h.vault.id, &VaultPath::new(path).unwrap())
        .unwrap()
        .unwrap()
}

#[test]
fn a_summary_lands_as_a_derived_note_with_visible_provenance() {
    let mut h = harness(
        "Both notes concern lighthouses: the ancient Pharos and the man who industrialised them.",
    );
    let sources = [doc(&h, "pharos.md").id, doc(&h, "michel.md").id];
    let Summarized::Written {
        document,
        provenance,
        wrote_sibling,
    } = h.app.summarize(&sources, None).unwrap()
    else {
        panic!("refused");
    };
    assert!(!wrote_sibling);
    assert_eq!(document.authorship, Authorship::Derived);
    assert_eq!(provenance.sources.len(), 2);
    assert!(!provenance.user_edited);
    assert_eq!(provenance.prompt_id.as_deref(), Some("summary-v1"));

    // The file itself carries the sources as wiki links (AI-011).
    let text = std::fs::read_to_string(document.path.to_fs(&h.vault.root)).unwrap();
    assert!(text.contains("## Sources"));
    assert!(text.contains("- [[Pharos]]"));
    assert!(text.contains("- [[Marius Michel Pasha]]"));

    // Generated notes can be excluded from a listing (AI-012).
    let filter = marius_storage::documents::Filter {
        vaults: vec![h.vault.id],
        exclude_generated: true,
        ..Default::default()
    };
    let listed = marius_storage::documents::list(h.app.database().conn(), &filter).unwrap();
    assert!(listed.iter().all(|d| d.id != document.id));
}

#[test]
fn regeneration_replaces_in_place_until_a_person_edits_then_writes_a_sibling() {
    // Integrity rule 6, both halves, plus AI-014's promotion.
    let mut h = harness("First synthesis.");
    let sources = [doc(&h, "pharos.md").id];
    let Summarized::Written { document, .. } = h.app.summarize(&sources, None).unwrap() else {
        panic!("refused");
    };

    // Untouched: regeneration replaces the body in place.
    let Summarized::Written {
        document: again,
        wrote_sibling,
        ..
    } = h.app.summarize(&sources, None).unwrap()
    else {
        panic!("refused");
    };
    assert_eq!(again.id, document.id);
    assert!(!wrote_sibling);

    // A person edits the note: the gate flips, and stays flipped.
    h.app
        .save_document(document.id, "# My summary\n\nI rewrote this myself.\n")
        .unwrap();
    assert!(
        h.app
            .note_provenance(document.id)
            .unwrap()
            .unwrap()
            .user_edited
    );

    let Summarized::Written {
        document: sibling,
        wrote_sibling,
        ..
    } = h.app.summarize(&sources, None).unwrap()
    else {
        panic!("refused");
    };
    assert!(wrote_sibling);
    assert_ne!(sibling.id, document.id);
    assert!(sibling.path.as_str().contains("(regenerated)"));
    let kept = std::fs::read_to_string(document.path.to_fs(&h.vault.root)).unwrap();
    assert!(
        kept.contains("I rewrote this myself"),
        "the user's bytes were never the model's to take back"
    );
}

#[test]
fn a_promoted_note_is_protected_like_a_source_document() {
    let mut h = harness("A synthesis.");
    let sources = [doc(&h, "pharos.md").id];
    let Summarized::Written { document, .. } = h.app.summarize(&sources, None).unwrap() else {
        panic!("refused");
    };
    h.app.promote_note(document.id).unwrap();
    assert_eq!(
        h.app.document(document.id).unwrap().authorship,
        Authorship::Promoted
    );

    let Summarized::Written { wrote_sibling, .. } = h.app.summarize(&sources, None).unwrap() else {
        panic!("refused");
    };
    assert!(wrote_sibling, "a promoted note is never overwritten");

    // Promoting a source note is refused: there is nothing to promote.
    assert!(h.app.promote_note(doc(&h, "dinner.md").id).is_err());
}

#[test]
fn confidential_sources_keep_summaries_off_remote_models() {
    let mut h = harness("irrelevant");
    let mut config = h.app.config().clone();
    {
        let m = config.models.summarization.as_mut().unwrap();
        m.provider = "remote-llm".into();
        m.locality = Some(marius_model::Locality::Remote);
        m.trusted_for_personal = true;
        m.base_url = Some("http://127.0.0.1:1".into()); // dead: must not be dialled
    }
    h.app.set_config(config).unwrap();
    let secret = doc(&h, "pharos.md");
    h.app
        .set_document_sensitivity(secret.id, Some(Sensitivity::Confidential))
        .unwrap();

    let Summarized::Refused(reason) = h
        .app
        .summarize(&[secret.id, doc(&h, "michel.md").id], None)
        .unwrap()
    else {
        panic!("should have been refused");
    };
    assert!(reason.contains("may not see confidential"), "{reason}");
}

#[test]
fn discovery_proposes_an_explained_edge_and_respects_rejection() {
    let mut h = harness(
        r#"{"related": true, "rationale": "Both are about lighthouse history.", "confidence": 0.8}"#,
    );
    let vault = h.vault.clone();
    h.app.embed_vault(&vault).unwrap();

    let report = h.app.discover_relationships(&vault, 5).unwrap();
    assert_eq!(report.refusal, None);
    assert_eq!(report.proposed, 1, "{report:?}");

    let pharos = doc(&h, "pharos.md");
    let neighbourhood = h.app.neighbourhood(pharos.id).unwrap();
    let edge = neighbourhood
        .outgoing
        .iter()
        .chain(neighbourhood.incoming.iter())
        .find(|r| r.kind == RelationshipKind::Semantic)
        .expect("semantic edge stored");
    assert_eq!(edge.provenance, Provenance::AiGenerated);
    assert_eq!(edge.review, ReviewState::Unreviewed);
    assert_eq!(
        edge.rationale.as_deref(),
        Some("Both are about lighthouse history.")
    );
    assert!(edge.confidence.is_some());

    // Re-running proposes nothing new: the pair is connected now.
    let report = h.app.discover_relationships(&vault, 5).unwrap();
    assert_eq!(report.proposed, 0);
    assert!(report.skipped_existing >= 1, "{report:?}");

    // Reject it; discovery must never bring it back (GRAPH-011).
    let id = edge.id;
    marius_storage::relationships::reject(h.app.database().conn(), id, Some("coincidence"))
        .unwrap();
    let report = h.app.discover_relationships(&vault, 5).unwrap();
    assert_eq!(report.proposed, 0);
    assert!(report.skipped_rejected >= 1, "{report:?}");
}
