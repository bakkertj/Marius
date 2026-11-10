//! Grounded question answering: the prompt and the citation parse
//! (SRCH-010, SRCH-011, SRCH-012, AC-008).
//!
//! Pure, like the tagging half: retrieval, policy, audit and the model
//! call live in `marius-core`. The contract with the model is numbered
//! passages in, `[n]`-cited prose out — and the parser treats the citation
//! numbers as the ground truth of which sources were actually used.

use marius_llm::ChatRequest;

/// One passage offered to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    /// Where it came from, for the reader: `Title › Heading › Path`.
    pub label: String,
    pub text: String,
}

/// The request for one question over retrieved passages.
pub fn request(question: &str, passages: &[Passage]) -> ChatRequest {
    let mut sources = String::new();
    for (index, passage) in passages.iter().enumerate() {
        sources.push_str(&format!(
            "[{}] {}\n{}\n\n",
            index + 1,
            passage.label,
            passage.text
        ));
    }
    let system = "You answer questions from a personal knowledge base. Use ONLY \
                  the numbered passages provided. After every claim, cite the \
                  passage(s) it rests on as [1] or [2][3]. If the passages do \
                  not contain the answer, say exactly that — do not fill the \
                  gap from general knowledge. Where you connect passages by \
                  inference, say 'this suggests' so synthesis reads differently \
                  from quotation."
        .to_owned();
    let user = format!("Passages:\n\n{sources}Question: {question}");
    ChatRequest {
        system,
        user,
        temperature: 0.1,
        max_tokens: Some(700),
    }
}

/// Which passages the answer actually cites, in first-use order.
///
/// Out-of-range numbers are dropped: a model inventing `[7]` over five
/// passages is claiming support it was never given, and showing the reader
/// a dead citation would dress fabrication as grounding (SRCH-012).
pub fn cited_passages(answer: &str, passage_count: usize) -> Vec<usize> {
    let mut cited = Vec::new();
    let bytes = answer.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(close) = answer[i + 1..].find(']') {
                let inner = &answer[i + 1..i + 1 + close];
                if let Ok(number) = inner.parse::<usize>() {
                    if number >= 1 && number <= passage_count && !cited.contains(&(number - 1)) {
                        cited.push(number - 1);
                    }
                }
                i += close + 2;
                continue;
            }
        }
        i += 1;
    }
    cited
}

/// True when the answer cites nothing — the model declined for lack of
/// support, the honest outcome SRCH-011 asks to surface rather than paper
/// over. An uncited claim is treated as no answer, whatever its prose.
pub fn is_unanswered(cited: &[usize]) -> bool {
    cited.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passages() -> Vec<Passage> {
        vec![
            Passage {
                label: "Pharos › Construction".into(),
                text: "Built under Ptolemy II.".into(),
            },
            Passage {
                label: "Harbour survey".into(),
                text: "The eastern basin silted up.".into(),
            },
        ]
    }

    #[test]
    fn the_prompt_numbers_passages_and_carries_the_question() {
        let request = request("Who built the Pharos?", &passages());
        assert!(request.user.contains("[1] Pharos › Construction"));
        assert!(request.user.contains("[2] Harbour survey"));
        assert!(request.user.ends_with("Question: Who built the Pharos?"));
        assert!(request.system.contains("ONLY"));
    }

    #[test]
    fn citations_parse_in_first_use_order_without_duplicates() {
        let cited = cited_passages("Ptolemy II built it [2][1]. It silted [1].", 2);
        assert_eq!(cited, vec![1, 0]);
    }

    #[test]
    fn invented_and_malformed_citations_are_dropped() {
        let cited = cited_passages("Fact [7]. Also [0] and [notes] and [2].", 2);
        assert_eq!(cited, vec![1]);
        assert!(cited_passages("Nothing cited here.", 5).is_empty());
    }

    #[test]
    fn an_uncited_answer_counts_as_unanswered() {
        let cited = cited_passages("The passages do not say who built it.", 2);
        assert!(is_unanswered(&cited));
    }
}
