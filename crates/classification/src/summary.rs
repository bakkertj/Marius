//! Summary and synthesis notes: the prompt and the body assembly
//! (AI-011, AI-012, AC-010).
//!
//! Pure, like its siblings. The generated note's *body* is assembled here
//! so its shape is testable: provenance banner in frontmatter-adjacent
//! position, the model's prose, and a Sources section of wiki links — a
//! generated note must carry its citations in the text itself, where they
//! survive export to any other Markdown tool.

use marius_llm::ChatRequest;

/// One source document offered for summarisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub title: String,
    pub text: String,
}

/// Identifier of this prompt's shape, stored in provenance so a note can
/// be regenerated the same way later (AI-016).
pub const PROMPT_ID: &str = "summary-v1";

const PER_SOURCE_BUDGET: usize = 8_000;

/// The request for one summary over the given sources.
pub fn request(sources: &[Source]) -> ChatRequest {
    let mut body = String::new();
    for (index, source) in sources.iter().enumerate() {
        body.push_str(&format!(
            "--- Source {}: {} ---\n{}\n\n",
            index + 1,
            source.title,
            truncate(&source.text, PER_SOURCE_BUDGET)
        ));
    }
    let system = "You write a concise synthesis note from the user's own source \
                  notes. Use ONLY what the sources say; omit what they do not. \
                  Write plain Markdown prose (headings allowed, no top-level \
                  title). When a claim comes from a specific source, you may \
                  mention it by title. Do not invent facts, dates or names."
        .to_owned();
    ChatRequest {
        system,
        user: format!("{body}Write the synthesis note now."),
        temperature: 0.2,
        max_tokens: Some(900),
    }
}

/// Assemble the note body that lands on disk: title, the model's prose,
/// and the Sources section that makes provenance visible *in the file*
/// (AI-011) — the database banner is for the UI, the wiki links are for
/// every other tool the vault might meet.
pub fn note_body(title: &str, prose: &str, source_titles: &[String]) -> String {
    let mut out = format!("# {title}\n\n{}\n", prose.trim());
    if !source_titles.is_empty() {
        out.push_str("\n## Sources\n\n");
        for source in source_titles {
            out.push_str(&format!("- [[{source}]]\n"));
        }
    }
    out
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
    fn the_prompt_numbers_sources_and_budgets_each() {
        let sources = vec![
            Source {
                title: "Pharos".into(),
                text: "x".repeat(50_000),
            },
            Source {
                title: "Survey".into(),
                text: "Soundings.".into(),
            },
        ];
        let request = request(&sources);
        assert!(request.user.contains("--- Source 1: Pharos ---"));
        assert!(request.user.contains("--- Source 2: Survey ---"));
        assert!(request.user.len() < 10_000, "per-source budget applied");
        assert!(request.system.contains("ONLY"));
    }

    #[test]
    fn the_note_carries_its_sources_as_wiki_links() {
        let body = note_body(
            "Lighthouse synthesis",
            "They all guided ships.\n",
            &["Pharos".into(), "1869 harbour survey".into()],
        );
        assert!(body.starts_with("# Lighthouse synthesis\n"));
        assert!(body.contains("## Sources"));
        assert!(body.contains("- [[Pharos]]"));
        assert!(body.contains("- [[1869 harbour survey]]"));
    }
}
