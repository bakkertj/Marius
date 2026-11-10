//! SRCH-007/009/013 end to end: chunking, embedding over a real loopback
//! socket, staleness, per-document policy, semantic ranking and hybrid
//! fusion — the fake daemon embeds by vocabulary, so similarity is real.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use marius_core::config::{ModelChoice, ModelSettings};
use marius_core::{Config, Marius};
use marius_model::{InferenceTask, Sensitivity, VaultPath};

/// A tiny embedding daemon: each text becomes a 4-vector counting the
/// occurrences of four topic words. Cosine over these is real geometry —
/// "harbour" queries genuinely land nearest harbour passages.
fn fake_embedder() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            counter.fetch_add(1, Ordering::SeqCst);
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
            let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            let inputs: Vec<String> = request["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            let vectors: Vec<Vec<f32>> = inputs
                .iter()
                .map(|text| {
                    let lower = text.to_lowercase();
                    let count = |word: &str| lower.matches(word).count() as f32;
                    vec![
                        1.0 + count("harbour") * 10.0,
                        1.0 + count("lighthouse") * 10.0,
                        1.0 + count("recipe") * 10.0,
                        1.0 + count("games") * 10.0,
                    ]
                })
                .collect();
            // Speak whichever protocol was asked for: Ollama's /api/embed
            // or the OpenAI-compatible /embeddings.
            let body = if first_line.contains("/api/embed") {
                serde_json::json!({ "model": "fake-embed", "embeddings": vectors }).to_string()
            } else {
                let data: Vec<_> = vectors
                    .iter()
                    .enumerate()
                    .map(|(index, embedding)| {
                        serde_json::json!({ "index": index, "embedding": embedding })
                    })
                    .collect();
                serde_json::json!({ "model": "fake-embed", "data": data }).to_string()
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (format!("http://{addr}"), calls)
}

struct Harness {
    app: Marius,
    vault: marius_model::Vault,
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
}

fn harness() -> (Harness, Arc<AtomicUsize>) {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    for (name, text) in [
        (
            "port.md",
            "# The Old Port\n\nThe harbour wall shelters the harbour basin. Ships wait in the harbour.\n",
        ),
        (
            "pharos.md",
            "# Pharos\n\nA lighthouse guides ships; the lighthouse keeper trims the lamp.\n",
        ),
        ("dinner.md", "# Dinner\n\nA recipe for lentil soup. The recipe serves four.\n"),
    ] {
        std::fs::write(files.path().join(name), text).unwrap();
    }
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("S", files.path()).unwrap();
    app.index(&vault).unwrap();

    let (base, calls) = fake_embedder();
    app.set_config(Config {
        models: ModelSettings {
            embedding: Some(ModelChoice {
                provider: "ollama".into(),
                model: "fake-embed".into(),
                credential: None,
                base_url: Some(base),
                locality: None,
                trusted_for_personal: false,
            }),
            ..ModelSettings::default()
        },
        ..Config::default()
    })
    .unwrap();
    (
        Harness {
            app,
            vault,
            _data: data,
            files,
        },
        calls,
    )
}

#[test]
fn embedding_is_idempotent_and_staleness_follows_the_content_hash() {
    let (mut h, _calls) = harness();
    let vault = h.vault.clone();
    let report = h.app.embed_vault(&vault).unwrap();
    assert_eq!(report.documents, 3);
    assert!(report.chunks_embedded >= 3);
    assert_eq!(report.refusal, None);

    // Second run: nothing stale, nothing re-sent (SRCH-013).
    let report = h.app.embed_vault(&vault).unwrap();
    assert_eq!(report.documents, 0);
    assert_eq!(report.up_to_date, 3);

    // Edit one file; only that document re-embeds.
    std::fs::write(
        h.files.path().join("dinner.md"),
        "# Dinner\n\nA recipe for bread. The recipe needs time.\n",
    )
    .unwrap();
    h.app.index(&vault).unwrap();
    let report = h.app.embed_vault(&vault).unwrap();
    assert_eq!(report.documents, 1);
    assert_eq!(report.up_to_date, 2);
}

#[test]
fn semantic_search_ranks_by_meaning_and_hybrid_fuses() {
    let (mut h, _calls) = harness();
    let vault = h.vault.clone();
    h.app.embed_vault(&vault).unwrap();

    let results = h.app.semantic_search("harbour", vault.id, 10).unwrap();
    assert_eq!(results.refusal, None);
    assert_eq!(results.hits.len(), 3, "one hit per document");
    assert_eq!(
        results.hits[0].document.title, "The Old Port",
        "most harbour-y note first"
    );
    assert!(results.hits[0].score > results.hits[2].score);
    assert!(
        results.hits[0].chunk.text.contains("harbour"),
        "the citation passage is the matching chunk"
    );

    let hybrid = h.app.hybrid_search("lighthouse", vault.id, 10).unwrap();
    assert!(!hybrid.hits.is_empty());
    assert_eq!(
        hybrid.hits[0].document.title, "Pharos",
        "keyword and semantic agree on the lighthouse note"
    );
}

#[test]
fn a_confidential_document_is_skipped_not_sent() {
    // The per-document gate: the vault stays permissive, one note is
    // confidential, and the daemon is remote by declaration.
    let (mut h, calls) = harness();
    let vault = h.vault.clone();

    // Reconfigure the same endpoint as a *remote* provider.
    let mut config = h.app.config().clone();
    {
        let choice = config.models.embedding.as_mut().unwrap();
        choice.provider = "remote-embeddings".into();
        choice.locality = Some(marius_model::Locality::Remote);
        choice.trusted_for_personal = true; // Personal may go; Confidential may not.
    }
    h.app.set_config(config).unwrap();

    let secret = h
        .app
        .document_at(vault.id, &VaultPath::new("dinner.md").unwrap())
        .unwrap()
        .unwrap();
    h.app
        .set_document_sensitivity(secret.id, Some(Sensitivity::Confidential))
        .unwrap();

    let report = h.app.embed_vault(&vault).unwrap();
    assert_eq!(report.documents, 2);
    assert_eq!(report.skipped_policy, 1);

    // The audit trail shows both halves (AI-015).
    let log = h.app.inference_log(10).unwrap();
    let refused: Vec<_> = log
        .iter()
        .filter(|r| r.task == InferenceTask::Embedding && r.provider.is_none())
        .collect();
    assert_eq!(refused.len(), 1);
    assert!(refused[0].reason.contains("may not see confidential"));
    assert_eq!(refused[0].documents.len(), 1);
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

#[test]
fn no_embedding_model_is_a_reason_not_an_error() {
    let (mut h, _calls) = harness();
    let vault = h.vault.clone();
    h.app.set_config(Config::default()).unwrap();
    let report = h.app.embed_vault(&vault).unwrap();
    assert!(report
        .refusal
        .unwrap()
        .contains("no provider is configured"));
    let results = h.app.semantic_search("harbour", vault.id, 10).unwrap();
    assert!(results.refusal.is_some());
    assert!(results.hits.is_empty());
}

/// A combined daemon: embeddings as above, plus /api/chat answering with a
/// fixed cited reply.
fn fake_full_daemon(chat_reply: &'static str) -> String {
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
                    "model": "fake-chat",
                    "message": { "role": "assistant", "content": chat_reply },
                    "prompt_eval_count": 80, "eval_count": 30, "done": true
                })
                .to_string()
            } else {
                let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                let inputs: Vec<String> = request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect();
                let vectors: Vec<Vec<f32>> = inputs
                    .iter()
                    .map(|text| {
                        let lower = text.to_lowercase();
                        let count = |word: &str| lower.matches(word).count() as f32;
                        vec![
                            1.0 + count("harbour") * 10.0,
                            1.0 + count("lighthouse") * 10.0,
                            1.0 + count("recipe") * 10.0,
                            1.0 + count("games") * 10.0,
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

#[test]
fn a_grounded_answer_cites_real_passages_by_byte_range() {
    // AC-008/SRCH-010: the citations resolve to the chunks that were sent.
    let (mut h, _calls) = harness();
    let vault = h.vault.clone();
    let base = fake_full_daemon(
        "The harbour wall shelters the basin [1]. The keeper trims the lamp [2]. \
         This suggests the port relied on both [1][2].",
    );
    let mut config = h.app.config().clone();
    config.models.embedding.as_mut().unwrap().base_url = Some(base.clone());
    config.models.question_answering = Some(ModelChoice {
        provider: "ollama".into(),
        model: "fake-chat".into(),
        credential: None,
        base_url: Some(base),
        locality: None,
        trusted_for_personal: false,
    });
    h.app.set_config(config).unwrap();
    h.app.embed_vault(&vault).unwrap();

    let answer = h
        .app
        .ask(
            "What protects the harbour, and who tends the lighthouse?",
            vault.id,
        )
        .unwrap();
    assert_eq!(answer.refusal, None);
    assert!(!answer.unanswered);
    assert_eq!(answer.citations.len(), 2, "two distinct passages cited");
    let labels: Vec<&str> = answer.citations.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.iter().any(|l| l.contains("The Old Port")),
        "{labels:?}"
    );
    assert!(labels.iter().any(|l| l.contains("Pharos")), "{labels:?}");
    for citation in &answer.citations {
        let (start, end) = citation.byte_range.unwrap();
        assert!(end > start, "byte range is real for click-to-highlight");
    }

    // The audit row lists the retrieved documents (AI-015).
    let log = h.app.inference_log(10).unwrap();
    let qa = log
        .iter()
        .find(|r| r.task == InferenceTask::QuestionAnswering)
        .unwrap();
    assert!(!qa.documents.is_empty());
}

#[test]
fn one_confidential_source_restricts_the_whole_question() {
    // AI-008 at the Q&A level: the retrieval set's strictest sensitivity
    // rules, and a remote answerer is refused — before any socket opens,
    // which is proven by pointing the chat model at a dead port.
    let (mut h, _calls) = harness();
    let vault = h.vault.clone();
    h.app.embed_vault(&vault).unwrap();

    let port = h
        .app
        .document_at(vault.id, &VaultPath::new("port.md").unwrap())
        .unwrap()
        .unwrap();
    h.app
        .set_document_sensitivity(port.id, Some(Sensitivity::Confidential))
        .unwrap();

    let mut config = h.app.config().clone();
    config.models.question_answering = Some(ModelChoice {
        provider: "remote-answers".into(),
        model: "big".into(),
        credential: None,
        base_url: Some("http://127.0.0.1:1".into()),
        locality: Some(marius_model::Locality::Remote),
        trusted_for_personal: true,
    });
    h.app.set_config(config).unwrap();

    let answer = h.app.ask("What protects the harbour?", vault.id).unwrap();
    let refusal = answer.refusal.expect("refused");
    assert!(refusal.contains("may not see confidential"), "{refusal}");

    let log = h.app.inference_log(10).unwrap();
    let qa = log
        .iter()
        .find(|r| r.task == InferenceTask::QuestionAnswering)
        .unwrap();
    assert_eq!(qa.sensitivity, Sensitivity::Confidential);
}

#[test]
fn an_off_topic_question_is_answered_with_honesty_not_improvisation() {
    // SRCH-011: nothing relevant → no model call at all.
    let (mut h, _calls) = harness();
    let vault = h.vault.clone();
    h.app.embed_vault(&vault).unwrap();
    let mut config = h.app.config().clone();
    config.models.question_answering = Some(ModelChoice {
        provider: "ollama".into(),
        model: "fake-chat".into(),
        credential: None,
        base_url: Some("http://127.0.0.1:1".into()), // dead: must not be dialled
        locality: None,
        trusted_for_personal: false,
    });
    h.app.set_config(config).unwrap();

    let answer = h.app.ask("games games games games", vault.id).unwrap();
    // "games" scores against the fourth axis, on which no note loads.
    assert!(answer.unanswered);
    assert!(answer.citations.is_empty());
}
