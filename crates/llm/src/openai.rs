//! The OpenAI-compatible adapter (AI-001): OpenRouter, a self-hosted
//! llama.cpp/vLLM server, or any other `/v1/chat/completions` endpoint.
//!
//! The caller states the locality, because a URL cannot be trusted to: the
//! same protocol serves a box in the next room and a third-party cloud, and
//! the policy engine treats those very differently (AI-005, AI-006).

use std::time::Duration;

use marius_model::Locality;
use serde::Deserialize;
use serde_json::json;

use crate::{http_client, ChatProvider, ChatRequest, ChatResponse, LlmError};

pub struct OpenAiCompatible {
    name: String,
    base_url: String,
    locality: Locality,
    /// Bearer token, fetched by the caller from the credential store —
    /// never from configuration (SEC-001).
    api_key: Option<String>,
    client: reqwest::blocking::Client,
}

impl OpenAiCompatible {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        locality: Locality,
        api_key: Option<String>,
    ) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self {
            name: name.into(),
            base_url,
            locality,
            api_key,
            client: http_client(Duration::from_secs(120)),
        }
    }

    /// List the endpoint's models (`GET /models`) — the cheap call a
    /// settings "Test connection" can make: it proves DNS, the route,
    /// the TLS trust chain and the credential without sending a single
    /// word of note content.
    pub fn models(&self) -> Result<Vec<String>, LlmError> {
        let endpoint = format!("{}/models", self.base_url);
        #[derive(Deserialize)]
        struct Listing {
            data: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            id: String,
        }
        let mut call = self.client.get(&endpoint);
        if let Some(key) = &self.api_key {
            call = call.bearer_auth(key);
        }
        let response = call.send().map_err(|source| LlmError::Unreachable {
            endpoint: endpoint.clone(),
            source,
        })?;
        let listing: Listing = crate::ollama::check(&endpoint, response)?;
        Ok(listing.data.into_iter().map(|entry| entry.id).collect())
    }

    /// OpenRouter with its conventional base URL.
    pub fn openrouter(api_key: String) -> Self {
        Self::new(
            "openrouter",
            "https://openrouter.ai/api/v1",
            Locality::Remote,
            Some(api_key),
        )
    }
}

impl ChatProvider for OpenAiCompatible {
    fn name(&self) -> &str {
        &self.name
    }

    fn locality(&self) -> Locality {
        self.locality
    }

    fn chat(&self, model: &str, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let endpoint = format!("{}/chat/completions", self.base_url);
        let mut body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "temperature": request.temperature,
        });
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = json!(max);
        }

        #[derive(Deserialize)]
        struct Reply {
            choices: Vec<Choice>,
            #[serde(default)]
            usage: Option<Usage>,
            #[serde(default)]
            model: Option<String>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: Message,
        }
        #[derive(Deserialize)]
        struct Message {
            content: String,
        }
        #[derive(Deserialize)]
        struct Usage {
            prompt_tokens: Option<u32>,
            completion_tokens: Option<u32>,
        }

        let mut call = self.client.post(&endpoint).json(&body);
        if let Some(key) = &self.api_key {
            call = call.bearer_auth(key);
        }
        let response = call.send().map_err(|source| LlmError::Unreachable {
            endpoint: endpoint.clone(),
            source,
        })?;
        let reply: Reply = crate::ollama::check(&endpoint, response)?;
        let choice = reply
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Protocol {
                endpoint: endpoint.clone(),
                detail: "no choices in response".into(),
            })?;
        let usage = reply.usage.unwrap_or(Usage {
            prompt_tokens: None,
            completion_tokens: None,
        });
        Ok(ChatResponse {
            text: choice.message.content,
            model: reply.model.unwrap_or_else(|| model.to_owned()),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
        })
    }
}

impl crate::EmbeddingProvider for OpenAiCompatible {
    fn name(&self) -> &str {
        &self.name
    }

    fn locality(&self) -> Locality {
        self.locality
    }

    fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        let endpoint = format!("{}/embeddings", self.base_url);
        #[derive(Deserialize)]
        struct Reply {
            data: Vec<Datum>,
        }
        #[derive(Deserialize)]
        struct Datum {
            index: usize,
            embedding: Vec<f32>,
        }
        let mut call = self
            .client
            .post(&endpoint)
            .json(&json!({ "model": model, "input": texts }));
        if let Some(key) = &self.api_key {
            call = call.bearer_auth(key);
        }
        let response = call.send().map_err(|source| LlmError::Unreachable {
            endpoint: endpoint.clone(),
            source,
        })?;
        let reply: Reply = crate::ollama::check(&endpoint, response)?;
        if reply.data.len() != texts.len() {
            return Err(LlmError::Protocol {
                endpoint,
                detail: format!(
                    "{} inputs produced {} vectors",
                    texts.len(),
                    reply.data.len()
                ),
            });
        }
        // The API documents an index field precisely because order is not
        // promised; honour it.
        let mut out = vec![Vec::new(); texts.len()];
        for datum in reply.data {
            let Some(slot) = out.get_mut(datum.index) else {
                return Err(LlmError::Protocol {
                    endpoint,
                    detail: format!("vector index {} out of range", datum.index),
                });
            };
            *slot = datum.embedding;
        }
        Ok(out)
    }
}
