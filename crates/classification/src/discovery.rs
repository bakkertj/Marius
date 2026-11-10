//! Relationship discovery (GRAPH-007): the prompt and the verdict parse.
//!
//! Embeddings nominate candidate pairs cheaply; a chat model then judges
//! whether the pair is *meaningfully* related and says why. The rationale
//! is stored on the edge and shown on hover (UI-010), because an edge
//! nobody can explain is an edge nobody will trust.

use marius_llm::ChatRequest;
use marius_model::Confidence;

const EXCERPT_BUDGET: usize = 2_500;

/// The request for one candidate pair.
pub fn request(title_a: &str, text_a: &str, title_b: &str, text_b: &str) -> ChatRequest {
    let system = "You judge whether two personal notes are meaningfully related \
                  beyond sharing common words. Reply with ONLY a JSON object: \
                  {\"related\": true|false, \"rationale\": \"one sentence\", \
                  \"confidence\": 0.0-1.0}. Related means a reader of one would \
                  genuinely want the other: same event, same subject, one \
                  explains the other. Mere topical overlap is not enough."
        .to_owned();
    let user = format!(
        "Note A: {title_a}\n{}\n\nNote B: {title_b}\n{}",
        truncate(text_a, EXCERPT_BUDGET),
        truncate(text_b, EXCERPT_BUDGET)
    );
    ChatRequest {
        system,
        user,
        temperature: 0.0,
        max_tokens: Some(150),
    }
}

/// The model's judgement of one pair.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub related: bool,
    pub rationale: Option<String>,
    pub confidence: Confidence,
}

/// Parse the verdict. Anything unparseable is `related: false` — an edge
/// is an assertion, and an assertion the model could not even format is
/// not one worth storing.
pub fn parse_verdict(reply: &str) -> Verdict {
    let trimmed = strip_fence(reply.trim());
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Verdict {
            related: false,
            rationale: None,
            confidence: Confidence::new(0.0),
        };
    };
    let related = value["related"].as_bool().unwrap_or(false);
    let rationale = value["rationale"]
        .as_str()
        .map(|r| r.chars().take(300).collect::<String>())
        .filter(|r| !r.is_empty());
    let confidence = Confidence::new(value["confidence"].as_f64().unwrap_or(0.5) as f32);
    Verdict {
        related,
        rationale,
        confidence,
    }
}

fn strip_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    let rest = rest.trim_start_matches(['\r', '\n']);
    rest.strip_suffix("```").map(str::trim_end).unwrap_or(rest)
}

fn truncate(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_verdict_parses_with_rationale_and_confidence() {
        let verdict = parse_verdict(
            r#"{"related": true, "rationale": "Both describe the 1869 survey.", "confidence": 0.85}"#,
        );
        assert!(verdict.related);
        assert_eq!(
            verdict.rationale.as_deref(),
            Some("Both describe the 1869 survey.")
        );
        assert!(verdict.confidence.get() > 0.8);
    }

    #[test]
    fn fenced_json_parses_and_garbage_means_unrelated() {
        assert!(parse_verdict("```json\n{\"related\": true}\n```").related);
        assert!(!parse_verdict("These notes seem related to me!").related);
        assert!(!parse_verdict("").related);
    }
}
