//! Adapter tests against a real HTTP server on a loopback port — actual
//! sockets and actual JSON, no Ollama required.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use marius_llm::ollama::Ollama;
use marius_llm::openai::OpenAiCompatible;
use marius_llm::{ChatProvider, ChatRequest, LlmError};
use marius_model::Locality;

/// Serve exactly one request with a canned body, returning what the client
/// sent. Panics in the serving thread surface as test failures via join.
fn one_shot(status: &'static str, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
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
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        request
    });
    (format!("http://{addr}"), handle)
}

fn request() -> ChatRequest {
    ChatRequest {
        system: "You label notes.".into(),
        user: "A note about lighthouses.".into(),
        temperature: 0.0,
        max_tokens: Some(64),
    }
}

#[test]
fn ollama_chat_round_trips_and_reports_usage() {
    let (base, server) = one_shot(
        "200 OK",
        r#"{"model":"llama3.2","message":{"role":"assistant","content":"lighthouse, history"},
           "prompt_eval_count":42,"eval_count":7,"done":true}"#,
    );
    let reply = Ollama::new(base).chat("llama3.2", &request()).unwrap();
    assert_eq!(reply.text, "lighthouse, history");
    assert_eq!(reply.prompt_tokens, Some(42));
    assert_eq!(reply.completion_tokens, Some(7));

    let sent = server.join().unwrap();
    assert!(sent.starts_with("POST /api/chat"));
    assert!(sent.contains("\"stream\":false"));
    assert!(sent.contains("You label notes."));
    assert!(sent.contains("\"num_predict\":64"));
}

#[test]
fn ollama_model_listing_is_the_liveness_probe() {
    let (base, server) = one_shot(
        "200 OK",
        r#"{"models":[{"name":"llama3.2:latest"},{"name":"nomic-embed-text:latest"}]}"#,
    );
    let models = Ollama::new(base).models().unwrap();
    assert_eq!(models, ["llama3.2:latest", "nomic-embed-text:latest"]);
    assert!(server.join().unwrap().starts_with("GET /api/tags"));
}

#[test]
fn a_down_daemon_is_unreachable_not_a_panic() {
    // AI-004: a missing local daemon must degrade, not break.
    let error = Ollama::new("http://127.0.0.1:1")
        .chat("llama3.2", &request())
        .unwrap_err();
    assert!(matches!(error, LlmError::Unreachable { .. }), "{error}");
}

#[test]
fn http_errors_carry_status_and_body() {
    let (base, server) = one_shot("404 Not Found", r#"{"error":"model 'nope' not found"}"#);
    let error = Ollama::new(base).chat("nope", &request()).unwrap_err();
    match error {
        LlmError::Http { status, body, .. } => {
            assert_eq!(status, 404);
            assert!(body.contains("not found"));
        }
        other => panic!("expected Http, got {other}"),
    }
    server.join().unwrap();
}

#[test]
fn malformed_json_is_a_protocol_error() {
    let (base, server) = one_shot("200 OK", "this is not json");
    let error = Ollama::new(base).chat("llama3.2", &request()).unwrap_err();
    assert!(matches!(error, LlmError::Protocol { .. }), "{error}");
    server.join().unwrap();
}

#[test]
fn openai_compatible_speaks_the_other_protocol_and_sends_the_key() {
    let (base, server) = one_shot(
        "200 OK",
        r#"{"id":"gen-1","model":"meta-llama/llama-3-70b","choices":[{"message":{"role":"assistant","content":"tags: harbour"}}],
           "usage":{"prompt_tokens":100,"completion_tokens":5}}"#,
    );
    let provider = OpenAiCompatible::new(
        "openrouter",
        base,
        Locality::Remote,
        Some("sk-test-123".into()),
    );
    assert_eq!(provider.locality(), Locality::Remote);
    let reply = provider.chat("meta-llama/llama-3-70b", &request()).unwrap();
    assert_eq!(reply.text, "tags: harbour");
    assert_eq!(reply.prompt_tokens, Some(100));

    let sent = server.join().unwrap();
    assert!(sent.starts_with("POST /chat/completions"));
    assert!(
        sent.contains("authorization: Bearer sk-test-123")
            || sent.contains("Authorization: Bearer sk-test-123")
    );
    assert!(sent.contains("\"max_tokens\":64"));
}

#[test]
fn ollama_embeddings_batch_and_count_check() {
    use marius_llm::EmbeddingProvider;
    let (base, server) = one_shot(
        "200 OK",
        r#"{"model":"nomic-embed-text","embeddings":[[0.1,0.2],[0.3,0.4]]}"#,
    );
    let vectors = Ollama::new(base)
        .embed("nomic-embed-text", &["first", "second"])
        .unwrap();
    assert_eq!(vectors, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
    let sent = server.join().unwrap();
    assert!(sent.starts_with("POST /api/embed"));
    assert!(sent.contains(r#""input":["first","second"]"#));

    // A count mismatch is a protocol error, not a silent misalignment.
    let (base, _server) = one_shot("200 OK", r#"{"embeddings":[[0.1]]}"#);
    let error = Ollama::new(base).embed("m", &["a", "b"]).unwrap_err();
    assert!(matches!(error, LlmError::Protocol { .. }));
}

#[test]
fn openai_embeddings_honour_the_index_field() {
    use marius_llm::EmbeddingProvider;
    // Out-of-order data array — the index field is authoritative.
    let (base, server) = one_shot(
        "200 OK",
        r#"{"data":[{"index":1,"embedding":[9.0]},{"index":0,"embedding":[1.0]}],"model":"text-embedding"}"#,
    );
    let provider = OpenAiCompatible::new("x", base, Locality::Remote, None);
    let vectors = provider.embed("text-embedding", &["a", "b"]).unwrap();
    assert_eq!(vectors, vec![vec![1.0], vec![9.0]]);
    assert!(server.join().unwrap().starts_with("POST /embeddings"));
}
