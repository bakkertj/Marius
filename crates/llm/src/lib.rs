//! # Model provider adapters
//!
//! Interchangeable clients for locally hosted models (Ollama) and
//! OpenAI-compatible endpoints (AI-001, AI-002).
//!
//! Built *after* `marius-llm-policy`, deliberately: nothing in this crate
//! decides whether a request may run. Callers obtain a
//! [`marius_llm_policy::Decision`] first, write the audit row, and only then
//! reach for a client here. Credentials are passed in by the caller, who
//! got them from the platform credential store by name — they are never
//! read from configuration (SEC-001).

#![forbid(unsafe_code)]

use std::time::Duration;

use marius_model::Locality;
use serde::{Deserialize, Serialize};

pub mod ollama;
pub mod openai;

/// One chat-style request. Small on purpose: the features that need more
/// (tool use, streaming) can grow the type when they exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Instructions that frame the task.
    pub system: String,
    /// The content to work on.
    pub user: String,
    /// 0.0 for extraction-like tasks (tagging), higher for prose.
    pub temperature: f32,
    pub max_tokens: Option<u32>,
}

/// What came back, with the accounting the audit log wants (AI-015).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub text: String,
    pub model: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
}

/// A provider that can run a chat request, blocking the calling thread.
///
/// Blocking is the right shape here: every caller is either a CLI command
/// or a background worker, and an async runtime in the core would buy
/// nothing but colour.
pub trait ChatProvider {
    fn name(&self) -> &str;
    fn locality(&self) -> Locality;
    fn chat(&self, model: &str, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
}

/// Why a provider call failed. `Unreachable` is ordinary life for a local
/// daemon that is not running, and callers degrade gracefully on it (AI-004).
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("cannot reach {endpoint}: {}", error_chain(.source))]
    Unreachable {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{endpoint} answered HTTP {status}: {body}")]
    Http {
        endpoint: String,
        status: u16,
        body: String,
    },
    #[error("unexpected response from {endpoint}: {detail}")]
    Protocol { endpoint: String, detail: String },
}

/// The whole cause chain, joined — reqwest's top line is "error sending
/// request for url", which hides the part a person can act on (the DNS
/// failure, the refused connection, the untrusted certificate).
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut cursor = error.source();
    while let Some(cause) = cursor {
        let text = cause.to_string();
        if parts.last() != Some(&text) {
            parts.push(text);
        }
        cursor = cause.source();
    }
    parts.join(": ")
}

pub(crate) fn http_client(timeout: Duration) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .expect("constructing an HTTP client cannot fail with these options")
}

/// A provider that can turn texts into vectors (SRCH-007).
///
/// Batched: one call embeds many passages, which is the shape both the
/// Ollama and OpenAI APIs actually offer and the only shape that is not
/// painfully slow over HTTP.
pub trait EmbeddingProvider {
    fn name(&self) -> &str;
    fn locality(&self) -> Locality;
    fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError>;
}
