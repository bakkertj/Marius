//! # Automatic tagging
//!
//! Builds the prompt a tagging model sees and parses what comes back
//! (TAG-005, AI-010). Pure: no I/O, no policy — the caller has already
//! obtained a `marius_llm_policy::Decision` and written the audit row.
//!
//! Everything that protects the user from this crate lives below it:
//! `tags::assign` refuses machine proposals over rejected or pinned
//! assignments, `replace_machine_tags` touches only the classifier's own
//! unreviewed rows, and `tags::ensure` routes a proposal to an existing
//! canonical tag or alias rather than minting a near-duplicate (TAG-008).

#![forbid(unsafe_code)]

use marius_llm::ChatRequest;
use marius_model::{Confidence, TagName};

/// How much of a document the model sees. Tagging does not need the whole
/// of a long note, and bounding the prompt bounds cost (AI-015).
const BODY_BUDGET: usize = 6_000;

/// One proposed tag.
#[derive(Debug, Clone, PartialEq)]
pub struct TagSuggestion {
    pub name: TagName,
    pub confidence: Confidence,
}

/// The request for one document.
///
/// `vocabulary` is the vault's existing tag names, most used first: models
/// reuse a vocabulary they are shown and invent one they are not (TAG-008).
pub fn tagging_request(
    title: &str,
    body: &str,
    vocabulary: &[TagName],
    max_tags: usize,
) -> ChatRequest {
    let mut vocab = String::new();
    for name in vocabulary.iter().take(60) {
        if !vocab.is_empty() {
            vocab.push_str(", ");
        }
        vocab.push_str(name.as_str());
    }
    let body_excerpt = truncate_on_char_boundary(body, BODY_BUDGET);
    let system = format!(
        "You assign topic tags to personal notes. Reply with ONLY a JSON array \
         of lowercase tag strings, most relevant first, at most {max_tags}. \
         Use '/' for hierarchy (e.g. \"history/maritime\"). Prefer reusing \
         the vault's existing tags when they fit; invent a new tag only when \
         nothing existing does. No commentary."
    );
    let user = if vocab.is_empty() {
        format!("Title: {title}\n\n{body_excerpt}")
    } else {
        format!("Existing tags: {vocab}\n\nTitle: {title}\n\n{body_excerpt}")
    };
    ChatRequest {
        system,
        user,
        temperature: 0.0,
        max_tokens: Some(200),
    }
}

/// Parse a model's reply into valid tag names.
///
/// Tolerant by design: models are told to answer with a JSON array and
/// mostly do, but "mostly" is load-bearing — fenced JSON, a `tags:` prefix,
/// comma or newline lists and stray `#` marks all occur in practice. What
/// cannot be made a valid [`TagName`] is dropped, order is preserved,
/// duplicates (post-normalisation) collapse, and `max` caps the result.
///
/// Confidence is positional: the model was asked for most-relevant-first,
/// and rank is the only signal it actually gives us. First suggestion 0.9,
/// each later one a step lower, floored — honest about being an estimate
/// without pretending to precision.
pub fn parse_suggestions(reply: &str, max: usize) -> Vec<TagSuggestion> {
    let candidates = extract_candidates(reply);
    let mut out: Vec<TagSuggestion> = Vec::new();
    for raw in candidates {
        let cleaned = raw.trim().trim_start_matches('#').trim();
        if cleaned.is_empty() || cleaned.len() > 100 {
            continue;
        }
        let Ok(name) = TagName::new(cleaned) else {
            continue;
        };
        if !looks_like_a_tag(&name) {
            continue;
        }
        if out.iter().any(|s| s.name == name) {
            continue;
        }
        let confidence = Confidence::new(0.9 - 0.1 * out.len() as f32);
        out.push(TagSuggestion { name, confidence });
        if out.len() == max {
            break;
        }
    }
    out
}

/// A tag is a label, not a sentence. Models that ignore instructions
/// produce prose fragments, and normalisation dutifully hyphenates them
/// into "valid" names — so validity is not enough. Each hierarchy level
/// may carry at most three words, and the whole name stays short.
fn looks_like_a_tag(name: &TagName) -> bool {
    let text = name.as_str();
    text.len() <= 40
        && text
            .split('/')
            .all(|level| level.split('-').count() <= 3 && !level.is_empty())
}

fn extract_candidates(reply: &str) -> Vec<String> {
    let trimmed = strip_code_fence(reply.trim());

    // The answer we asked for: a JSON array of strings.
    if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(trimmed) {
        return items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
    }
    // A JSON object with a tags-like field.
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(trimmed) {
        for key in ["tags", "labels", "suggestions"] {
            if let Some(serde_json::Value::Array(items)) = map.get(key) {
                return items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
            }
        }
    }

    // Free text: strip a label prefix, then split lines and commas, with
    // bullets and numbering shaved off.
    let text = trimmed
        .strip_prefix("tags:")
        .or_else(|| trimmed.strip_prefix("Tags:"))
        .unwrap_or(trimmed);
    text.lines()
        .flat_map(|line| line.split(','))
        .map(|item| {
            item.trim()
                .trim_start_matches(['-', '*', '•'])
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')'])
                .trim()
                .trim_matches('"')
                .to_owned()
        })
        .filter(|item| !item.is_empty())
        .collect()
}

fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    let rest = rest.trim_start_matches(['\r', '\n']);
    rest.strip_suffix("```").map(str::trim_end).unwrap_or(rest)
}

fn truncate_on_char_boundary(text: &str, budget: usize) -> &str {
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

    fn names(suggestions: &[TagSuggestion]) -> Vec<&str> {
        suggestions.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn the_requested_format_parses() {
        let got = parse_suggestions(r#"["lighthouse", "history/maritime", "ottoman-empire"]"#, 8);
        assert_eq!(
            names(&got),
            ["lighthouse", "history/maritime", "ottoman-empire"]
        );
        assert!(
            got[0].confidence > got[2].confidence,
            "rank orders confidence"
        );
    }

    #[test]
    fn fenced_json_and_object_shapes_parse_too() {
        let got = parse_suggestions("```json\n[\"harbour\", \"survey\"]\n```", 8);
        assert_eq!(names(&got), ["harbour", "survey"]);

        let got = parse_suggestions(r#"{"tags": ["harbour", "survey"]}"#, 8);
        assert_eq!(names(&got), ["harbour", "survey"]);
    }

    #[test]
    fn chatty_free_text_still_yields_tags() {
        let got = parse_suggestions(
            "Tags: #lighthouse, maritime history\n- ottoman empire\n2. biography",
            8,
        );
        assert_eq!(
            names(&got),
            [
                "lighthouse",
                "maritime-history",
                "ottoman-empire",
                "biography"
            ]
        );
    }

    #[test]
    fn garbage_duplicates_and_overflow_are_contained() {
        let got = parse_suggestions(
            r#"["lighthouse", "LIGHTHOUSE", "", "///", "a", "b", "c", "d", "e"]"#,
            4,
        );
        // Case-folded duplicate collapses; invalid entries drop; cap holds.
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].name.as_str(), "lighthouse");
        assert!(parse_suggestions("I'm sorry, I cannot help with that.", 8).len() <= 2);
    }

    #[test]
    fn prose_fragments_are_not_tags_however_valid_their_normalisation() {
        // Seen live: a roleplay-tuned model answered with sentences, and
        // normalisation turned them into 90-character "tags".
        let got = parse_suggestions(
            r#"["we will take a closer look at the main characters",
                "each with their own unique fighting style",
                "maritime history", "history/ottoman-empire", "lighthouse"]"#,
            8,
        );
        assert_eq!(
            names(&got),
            ["maritime-history", "history/ottoman-empire", "lighthouse"]
        );
    }

    #[test]
    fn the_prompt_shows_the_vocabulary_and_respects_the_budget() {
        let vocab = [
            TagName::new("lighthouse").unwrap(),
            TagName::new("survey").unwrap(),
        ];
        let long_body = "x".repeat(50_000);
        let request = tagging_request("Pharos", &long_body, &vocab, 8);
        assert!(request.user.contains("Existing tags: lighthouse, survey"));
        assert!(request.user.len() < 7_000, "body got truncated");
        assert_eq!(request.temperature, 0.0);
    }

    #[test]
    fn multibyte_bodies_truncate_on_char_boundaries() {
        let body = "é".repeat(10_000);
        let request = tagging_request("T", &body, &[], 8);
        assert!(request.user.is_char_boundary(request.user.len()));
    }
}
