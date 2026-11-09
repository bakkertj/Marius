//! Splitting a document into passages for embedding and citation.
//!
//! Requirements: SRCH-008 (index document passages/sections as well as whole
//! documents), SRCH-010 (clickable source citations), SRCH-013 (incremental
//! update), AC-008.
//!
//! Two properties matter more than clever sizing:
//!
//! * **`chunk.text == document[chunk.span]`.** A citation highlights a real
//!   range of the real file, so the reader can see exactly what the answer was
//!   built from. Every operation here preserves that, which is why overlap is
//!   expressed as overlapping spans rather than as synthesised text.
//! * **Chunks follow headings.** A passage that straddles two unrelated
//!   sections retrieves badly and cites confusingly, so section structure wins
//!   over uniform size.

use std::ops::Range;

use marius_model::{Chunk, ChunkId, DocumentId};

use crate::outline::Outline;

/// Passage sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkConfig {
    /// Hard upper bound on a passage, in bytes. Sized for the context budget of
    /// a small local embedding model (SRCH-007 keeps embedding local by
    /// default), with room for the heading path that is prepended at query time.
    pub max_bytes: usize,
    /// Passages shorter than this are merged with their neighbours, so a note
    /// of one-line headings does not become fifty near-empty vectors.
    pub min_bytes: usize,
    /// Bytes of the previous passage repeated at the start of the next when a
    /// section had to be split, so a sentence cut in half is still retrievable.
    pub overlap_bytes: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_bytes: 1_200,
            min_bytes: 300,
            overlap_bytes: 120,
        }
    }
}

/// A passage, before it is given an identity.
///
/// The markdown crate knows nothing about documents, so IDs are attached by the
/// caller via [`ChunkDraft::into_chunk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDraft {
    pub ordinal: u32,
    /// Absolute byte range in the document.
    pub span: Range<usize>,
    /// Heading path this passage sits under, outermost first.
    pub heading_path: Vec<String>,
    pub text: String,
}

impl ChunkDraft {
    /// Attach a document identity.
    pub fn into_chunk(self, document: DocumentId) -> Chunk {
        Chunk {
            id: ChunkId::new(),
            document,
            ordinal: self.ordinal,
            byte_start: self.span.start,
            byte_end: self.span.end,
            heading_path: self.heading_path,
            text: self.text,
        }
    }
}

/// A span plus the heading path that governs it.
struct Unit {
    span: Range<usize>,
    path: Vec<String>,
}

/// Split a document into passages.
///
/// `text` is the whole document and `outline` must have been built from it, so
/// that spans line up.
pub fn chunk(text: &str, outline: &Outline, config: &ChunkConfig) -> Vec<ChunkDraft> {
    let mut units: Vec<Unit> = Vec::with_capacity(outline.entries.len() + 1);
    if has_content(text, &outline.preamble) {
        units.push(Unit {
            span: outline.preamble.clone(),
            path: Vec::new(),
        });
    }
    for entry in &outline.entries {
        // The heading line is part of its own passage: it is the single most
        // useful sentence for retrieving the section.
        let span = entry.heading.span.start..entry.content.end.max(entry.heading.span.end);
        if has_content(text, &span) {
            units.push(Unit {
                span,
                path: entry.path.clone(),
            });
        }
    }
    if units.is_empty() {
        return Vec::new();
    }

    let mut drafts: Vec<ChunkDraft> = Vec::new();
    let mut group: Vec<&Unit> = Vec::new();
    let mut group_bytes = 0usize;

    for unit in &units {
        let len = unit.span.len();
        if len > config.max_bytes {
            flush(text, &mut group, &mut group_bytes, config, &mut drafts);
            split_unit(text, unit, config, &mut drafts);
            continue;
        }
        if !group.is_empty() && group_bytes + len > config.max_bytes {
            flush(text, &mut group, &mut group_bytes, config, &mut drafts);
        }
        group.push(unit);
        group_bytes += len;
        if group_bytes >= config.min_bytes {
            flush(text, &mut group, &mut group_bytes, config, &mut drafts);
        }
    }
    flush(text, &mut group, &mut group_bytes, config, &mut drafts);

    for (i, draft) in drafts.iter_mut().enumerate() {
        draft.ordinal = i as u32;
    }
    drafts
}

/// Emit the accumulated units as one passage.
fn flush(
    text: &str,
    group: &mut Vec<&Unit>,
    group_bytes: &mut usize,
    _config: &ChunkConfig,
    out: &mut Vec<ChunkDraft>,
) {
    if group.is_empty() {
        return;
    }
    let span = group[0].span.start..group[group.len() - 1].span.end;
    let path = common_path(group);
    push(text, span, path, out);
    group.clear();
    *group_bytes = 0;
}

/// The heading path shared by every unit in a merged group.
///
/// Merging `One › A` with `One › B` yields `One`, which is the honest label for
/// a passage spanning both.
fn common_path(group: &[&Unit]) -> Vec<String> {
    let Some(first) = group.first() else {
        return Vec::new();
    };
    let mut shared = first.path.clone();
    for unit in &group[1..] {
        let keep = shared
            .iter()
            .zip(&unit.path)
            .take_while(|(a, b)| a == b)
            .count();
        shared.truncate(keep);
    }
    shared
}

/// Split one over-long section at paragraph boundaries, with overlap.
fn split_unit(text: &str, unit: &Unit, config: &ChunkConfig, out: &mut Vec<ChunkDraft>) {
    let span = clamp(text, &unit.span);
    let mut cursor = span.start;

    while cursor < span.end {
        let remaining = span.end - cursor;
        if remaining <= config.max_bytes {
            push(text, cursor..span.end, unit.path.clone(), out);
            break;
        }
        let hard_limit = floor_boundary(text, cursor + config.max_bytes);
        // Prefer a paragraph break, then a line break, then a sentence end,
        // then any character boundary.
        let end = find_break(text, cursor, hard_limit).unwrap_or(hard_limit);
        let end = if end <= cursor {
            floor_boundary(text, cursor + 1).max(cursor + 1)
        } else {
            end
        };
        push(text, cursor..end, unit.path.clone(), out);

        // Step back by the overlap so a sentence cut at `end` is still whole in
        // the next passage.
        let next = end.saturating_sub(config.overlap_bytes).max(cursor + 1);
        cursor = ceil_boundary(text, next);
    }
}

/// The best split point in `from..limit`, searching backwards.
fn find_break(text: &str, from: usize, limit: usize) -> Option<usize> {
    let window = &text[from..limit];
    for pattern in ["\n\n", "\n", ". ", "! ", "? "] {
        if let Some(at) = window.rfind(pattern) {
            let candidate = from + at + pattern.len();
            // Reject a break so early that it would make a tiny passage and
            // loop for a long time.
            if candidate > from + (limit - from) / 4 {
                return Some(candidate);
            }
        }
    }
    None
}

fn push(text: &str, span: Range<usize>, path: Vec<String>, out: &mut Vec<ChunkDraft>) {
    let span = clamp(text, &span);
    let slice = &text[span.clone()];
    // Require real content, not just punctuation: an empty heading (`# ` on its
    // own) or a horizontal rule is nothing to embed and nothing to cite.
    if !slice.chars().any(char::is_alphanumeric) {
        return;
    }
    out.push(ChunkDraft {
        ordinal: 0,
        span,
        heading_path: path,
        text: slice.to_owned(),
    });
}

fn has_content(text: &str, span: &Range<usize>) -> bool {
    text[clamp(text, span)].chars().any(char::is_alphanumeric)
}

fn clamp(text: &str, span: &Range<usize>) -> Range<usize> {
    let start = ceil_boundary(text, span.start.min(text.len()));
    let end = floor_boundary(text, span.end.min(text.len())).max(start);
    start..end
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::Outline;
    use crate::scan::scan;

    fn chunks(text: &str, config: ChunkConfig) -> Vec<ChunkDraft> {
        let scan = scan(text, 0);
        let outline = Outline::build(&scan.headings, 0..text.len());
        chunk(text, &outline, &config)
    }

    fn default_chunks(text: &str) -> Vec<ChunkDraft> {
        chunks(text, ChunkConfig::default())
    }

    /// The invariant every citation depends on.
    fn assert_spans_quote_the_document(text: &str, drafts: &[ChunkDraft]) {
        for draft in drafts {
            assert_eq!(
                draft.text,
                &text[draft.span.clone()],
                "chunk {} does not quote its span",
                draft.ordinal
            );
        }
    }

    #[test]
    fn every_chunk_quotes_its_own_span_exactly() {
        let text = "# A\n\nword word word.\n\n## B\n\nmore text here.\n";
        let drafts = default_chunks(text);
        assert_spans_quote_the_document(text, &drafts);
    }

    #[test]
    fn chunks_are_numbered_in_document_order() {
        // Long enough that the middle section must split, so ordering across
        // both merging and splitting is exercised.
        let text = "intro\n\n# A\n\n".to_string()
            + &"a sentence of filler. ".repeat(200)
            + "\n\n# B\n\nb\n";
        let drafts = default_chunks(&text);
        assert!(drafts.len() > 3, "expected a split, got {}", drafts.len());
        for (i, draft) in drafts.iter().enumerate() {
            assert_eq!(draft.ordinal, i as u32);
        }
        assert!(
            drafts
                .windows(2)
                .all(|w| w[0].span.start <= w[1].span.start),
            "chunks must stay in document order"
        );
    }

    #[test]
    fn a_short_document_is_one_chunk() {
        let text = "# Pharos\n\nIt stood at the harbour mouth.\n";
        let drafts = default_chunks(text);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].text, text);
        assert_eq!(drafts[0].heading_path, vec!["Pharos"]);
    }

    #[test]
    fn the_heading_line_travels_with_its_section() {
        let text = format!("# Construction\n\n{}\n", "detail ".repeat(60));
        let drafts = default_chunks(&text);
        assert!(drafts[0].text.starts_with("# Construction"));
    }

    #[test]
    fn content_before_the_first_heading_is_chunked_too() {
        let text = format!("{}\n\n# Later\n\nmore\n", "preamble ".repeat(60));
        let drafts = default_chunks(&text);
        assert!(drafts[0].heading_path.is_empty());
        assert!(drafts[0].text.contains("preamble"));
    }

    #[test]
    fn small_sections_are_merged_and_keep_their_common_ancestor() {
        let text = "# One\n\n## A\n\nshort\n\n## B\n\nshort\n";
        let drafts = default_chunks(text);
        assert_eq!(drafts.len(), 1, "{drafts:#?}");
        // `One › A` merged with `One › B` is honestly labelled `One`.
        assert_eq!(drafts[0].heading_path, vec!["One"]);
    }

    #[test]
    fn unrelated_small_sections_merge_to_an_empty_path() {
        let text = "# One\n\nx\n\n# Two\n\ny\n";
        let drafts = default_chunks(text);
        assert_eq!(drafts.len(), 1);
        assert!(drafts[0].heading_path.is_empty());
    }

    #[test]
    fn a_long_section_is_split_without_exceeding_the_limit() {
        let config = ChunkConfig {
            max_bytes: 300,
            min_bytes: 80,
            overlap_bytes: 40,
        };
        let paragraph = "This is a sentence of reasonable length. ".repeat(40);
        let text = format!("# Long\n\n{paragraph}\n");
        let drafts = chunks(&text, config);
        assert!(
            drafts.len() > 3,
            "expected several chunks, got {}",
            drafts.len()
        );
        for draft in &drafts {
            assert!(
                draft.span.len() <= config.max_bytes,
                "chunk of {} bytes exceeds the {} byte limit",
                draft.span.len(),
                config.max_bytes
            );
        }
        assert_spans_quote_the_document(&text, &drafts);
    }

    #[test]
    fn split_chunks_overlap_so_a_cut_sentence_stays_retrievable() {
        let config = ChunkConfig {
            max_bytes: 300,
            min_bytes: 80,
            overlap_bytes: 60,
        };
        let text = format!("# Long\n\n{}\n", "sentence here. ".repeat(60));
        let drafts = chunks(&text, config);
        assert!(drafts.len() >= 2);
        let overlapping = drafts
            .windows(2)
            .filter(|w| w[1].span.start < w[0].span.end)
            .count();
        assert!(
            overlapping >= 1,
            "expected overlapping spans, got {drafts:#?}"
        );
    }

    #[test]
    fn a_split_section_keeps_its_heading_path_on_every_piece() {
        let config = ChunkConfig {
            max_bytes: 250,
            min_bytes: 80,
            overlap_bytes: 30,
        };
        let text = format!("# One\n\n## Deep\n\n{}\n", "text ".repeat(200));
        let drafts = chunks(&text, config);
        let deep: Vec<_> = drafts
            .iter()
            .filter(|d| d.heading_path == vec!["One".to_string(), "Deep".to_string()])
            .collect();
        assert!(deep.len() >= 2, "{drafts:#?}");
    }

    #[test]
    fn splitting_prefers_paragraph_boundaries() {
        let config = ChunkConfig {
            max_bytes: 200,
            min_bytes: 50,
            overlap_bytes: 0,
        };
        let para = "x".repeat(120);
        let text = format!("# H\n\n{para}\n\n{para}\n\n{para}\n");
        let drafts = chunks(&text, config);
        // With no overlap and paragraph-aligned splits, no chunk should begin
        // in the middle of a run of x's.
        for draft in drafts.iter().skip(1) {
            assert!(
                draft.text.starts_with('x') || draft.text.starts_with('\n'),
                "unexpected chunk start: {:?}",
                &draft.text[..draft.text.len().min(20)]
            );
        }
        assert_spans_quote_the_document(&text, &drafts);
    }

    #[test]
    fn an_unbreakable_run_is_still_split_and_terminates() {
        // A minified blob or a long base64 line has no break to find.
        let config = ChunkConfig {
            max_bytes: 100,
            min_bytes: 40,
            overlap_bytes: 10,
        };
        let text = format!("# H\n\n{}\n", "a".repeat(2000));
        let drafts = chunks(&text, config);
        assert!(drafts.len() > 10);
        for draft in &drafts {
            assert!(draft.span.len() <= config.max_bytes);
        }
        assert_spans_quote_the_document(&text, &drafts);
    }

    #[test]
    fn multibyte_text_is_never_split_mid_character() {
        let config = ChunkConfig {
            max_bytes: 100,
            min_bytes: 40,
            overlap_bytes: 17,
        };
        let text = format!("# 灯台\n\n{}\n", "灯台のある港。".repeat(80));
        let drafts = chunks(&text, config);
        assert!(drafts.len() > 5);
        // `assert_spans_quote_the_document` would panic on a bad boundary, and
        // slicing in `push` would too; this makes the intent explicit.
        for draft in &drafts {
            assert!(text.is_char_boundary(draft.span.start));
            assert!(text.is_char_boundary(draft.span.end));
        }
        assert_spans_quote_the_document(&text, &drafts);
    }

    #[test]
    fn an_empty_or_blank_document_yields_no_chunks() {
        assert!(default_chunks("").is_empty());
        assert!(default_chunks("   \n\n\t\n").is_empty());
        // An empty heading and a rule carry nothing to embed or cite.
        assert!(default_chunks("# \n\n").is_empty());
        assert!(default_chunks("---\n\n***\n").is_empty());
    }

    #[test]
    fn spans_are_absolute_so_citations_address_the_real_file() {
        let text = "---\ntitle: x\n---\n# H\n\nbody text here.\n";
        let split = crate::frontmatter::split(text);
        let scan = scan(split.body, split.body_offset);
        let outline = Outline::build(&scan.headings, split.body_offset..text.len());
        let drafts = chunk(text, &outline, &ChunkConfig::default());
        assert_eq!(drafts.len(), 1);
        assert!(drafts[0].span.start >= split.body_offset);
        assert_spans_quote_the_document(text, &drafts);
    }

    #[test]
    fn drafts_become_chunks_with_citation_labels() {
        let text = "# One\n\n## Deep\n\nbody\n";
        let drafts = default_chunks(text);
        let chunk = drafts
            .into_iter()
            .next()
            .unwrap()
            .into_chunk(DocumentId::new());
        assert_eq!(chunk.byte_start, 0);
        assert!(chunk.citation_label().contains("One"));
    }
}
