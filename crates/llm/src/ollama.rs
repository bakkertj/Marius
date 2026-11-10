//! The Ollama adapter (AI-002): a model server on this machine.
//!
//! Content handed to it never crosses a network boundary, which is what
//! makes it the only locality `Confidential` material may reach (AC-016).

use std::time::Duration;

use marius_model::Locality;
use serde::Deserialize;
use serde_json::json;

use crate::{http_client, ChatProvider, ChatRequest, ChatResponse, LlmError};

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";

pub struct Ollama {
    base_url: String,
    client: reqwest::blocking::Client,
}

impl Ollama {
    /// `base_url` like `http://127.0.0.1:11434`; trailing slash tolerated.
    pub fn new(base_url: impl Into<String>) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self {
            base_url,
            // Local generation on modest hardware is slow; the timeout is
            // for a hung daemon, not a thinking one.
            client: http_client(Duration::from_secs(300)),
        }
    }

    pub fn default_local() -> Self {
        Self::new(DEFAULT_BASE_URL)
    }

    /// Models the daemon has pulled, by name. Also the cheapest liveness
    /// probe there is.
    pub fn models(&self) -> Result<Vec<String>, LlmError> {
        let endpoint = format!("{}/api/tags", self.base_url);
        #[derive(Deserialize)]
        struct Tags {
            #[serde(default)]
            models: Vec<Model>,
        }
        #[derive(Deserialize)]
        struct Model {
            name: String,
        }
        let response =
            self.client
                .get(&endpoint)
                .send()
                .map_err(|source| LlmError::Unreachable {
                    endpoint: endpoint.clone(),
                    source,
                })?;
        let tags: Tags = check(&endpoint, response)?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    pub fn is_available(&self) -> bool {
        self.models().is_ok()
    }
}

impl ChatProvider for Ollama {
    fn name(&self) -> &str {
        "ollama"
    }

    fn locality(&self) -> Locality {
        Locality::Local
    }

    fn chat(&self, model: &str, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let endpoint = format!("{}/api/chat", self.base_url);
        let mut body = json!({
            "model": model,
            "stream": false,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "options": { "temperature": request.temperature },
        });
        if let Some(max) = request.max_tokens {
            body["options"]["num_predict"] = json!(max);
        }

        #[derive(Deserialize)]
        struct Reply {
            message: Message,
            #[serde(default)]
            prompt_eval_count: Option<u32>,
            #[serde(default)]
            eval_count: Option<u32>,
            #[serde(default)]
            model: Option<String>,
        }
        #[derive(Deserialize)]
        struct Message {
            content: String,
        }

        let response = self
            .client
            .post(&endpoint)
            .json(&body)
            .send()
            .map_err(|source| LlmError::Unreachable {
                endpoint: endpoint.clone(),
                source,
            })?;
        let reply: Reply = check(&endpoint, response)?;
        Ok(ChatResponse {
            text: reply.message.content,
            model: reply.model.unwrap_or_else(|| model.to_owned()),
            prompt_tokens: reply.prompt_eval_count,
            completion_tokens: reply.eval_count,
        })
    }
}

/// Shared status + decode handling.
pub(crate) fn check<T: serde::de::DeserializeOwned>(
    endpoint: &str,
    response: reqwest::blocking::Response,
) -> Result<T, LlmError> {
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(LlmError::Http {
            endpoint: endpoint.to_owned(),
            status: status.as_u16(),
            body: body.chars().take(400).collect(),
        });
    }
    serde_json::from_str(&body).map_err(|e| LlmError::Protocol {
        endpoint: endpoint.to_owned(),
        detail: format!("{e} in {}", body.chars().take(200).collect::<String>()),
    })
}
