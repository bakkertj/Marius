//! Applying byte-range edits to a document.
//!
//! Requirements: MD-007 (preserve link integrity when documents are renamed or
//! moved), AC-003, TAG-009 (authorised frontmatter writeback).
//!
//! Renaming a note has to update the `[[links]]` that point at it inside other
//! people's files. That is the one case where Marius writes into a source
//! document, so the edit is deliberately as small as it can be: the exact
//! target span the scanner reported, nothing else on the line, nothing else in
//! the file. Reserialising the document would be far easier and would quietly
//! reformat the user's Markdown.

use std::ops::Range;

/// A single replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Byte range to replace, in the coordinates of the text being edited.
    pub span: Range<usize>,
    pub replacement: String,
}

impl Edit {
    pub fn new(span: Range<usize>, replacement: impl Into<String>) -> Self {
        Self {
            span,
            replacement: replacement.into(),
        }
    }
}

/// Error from an edit set that cannot be applied safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("edit range {span:?} is outside the text (length {len})")]
    OutOfBounds { span: Range<usize>, len: usize },
    #[error("edit range {span:?} does not fall on character boundaries")]
    NotCharBoundary { span: Range<usize> },
    #[error("edits {first:?} and {second:?} overlap")]
    Overlapping {
        first: Range<usize>,
        second: Range<usize>,
    },
}

/// Apply `edits` to `text`, returning the new text.
///
/// Edits may be given in any order. The whole set is validated before anything
/// is written, so a bad edit cannot leave a half-rewritten document behind.
pub fn apply(text: &str, edits: &[Edit]) -> Result<String, EditError> {
    if edits.is_empty() {
        return Ok(text.to_owned());
    }
    let mut ordered: Vec<&Edit> = edits.iter().collect();
    ordered.sort_by_key(|e| (e.span.start, e.span.end));

    for edit in &ordered {
        if edit.span.end > text.len() || edit.span.start > edit.span.end {
            return Err(EditError::OutOfBounds {
                span: edit.span.clone(),
                len: text.len(),
            });
        }
        if !text.is_char_boundary(edit.span.start) || !text.is_char_boundary(edit.span.end) {
            return Err(EditError::NotCharBoundary {
                span: edit.span.clone(),
            });
        }
    }
    for pair in ordered.windows(2) {
        if pair[1].span.start < pair[0].span.end {
            return Err(EditError::Overlapping {
                first: pair[0].span.clone(),
                second: pair[1].span.clone(),
            });
        }
    }

    let added: usize = ordered.iter().map(|e| e.replacement.len()).sum();
    let mut out = String::with_capacity(text.len() + added);
    let mut cursor = 0usize;
    for edit in ordered {
        out.push_str(&text[cursor..edit.span.start]);
        out.push_str(&edit.replacement);
        cursor = edit.span.end;
    }
    out.push_str(&text[cursor..]);
    Ok(out)
}

/// A wiki-link target that needs no escaping can be written as-is; one
/// containing `]]`, `|` or `#` would change the link's meaning.
///
/// Rather than inventing an escape syntax no other tool reads, such a target is
/// reported as unwritable so the caller can warn instead of corrupting a link.
pub fn is_writable_wiki_target(target: &str) -> bool {
    !target.contains("]]")
        && !target.contains('|')
        && !target.contains('#')
        && !target.contains('[')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_rename_rewrites_only_the_target_text() {
        // MD-007 / AC-003 — the reason this module exists.
        let body = "See [[Alexandria|the Pharos]] and [[Alexandria#Construction]].\n";
        let scan = scan(body, 0);
        let edits: Vec<Edit> = scan
            .wiki_links
            .iter()
            .filter(|l| l.target == "Alexandria")
            .map(|l| Edit::new(l.target_span.clone(), "Pharos of Alexandria"))
            .collect();
        assert_eq!(edits.len(), 2);

        let out = apply(body, &edits).unwrap();
        assert_eq!(
            out,
            "See [[Pharos of Alexandria|the Pharos]] and [[Pharos of Alexandria#Construction]].\n"
        );
    }

    #[test]
    fn a_move_rewrites_a_markdown_destination_and_keeps_its_fragment() {
        let body = "[spec](notes/old/spec.md#intro)\n";
        let scan = scan(body, 0);
        let link = &scan.markdown_links[0];
        let out = apply(
            body,
            &[Edit::new(link.target_span.clone(), "archive/spec.md")],
        )
        .unwrap();
        assert_eq!(out, "[spec](archive/spec.md#intro)\n");
    }

    #[test]
    fn unrelated_text_including_similar_names_is_untouched() {
        let body = "[[Alexandria]] but not Alexandria, nor [[Alexandria Library]].\n";
        let scan = scan(body, 0);
        let edits: Vec<Edit> = scan
            .wiki_links
            .iter()
            .filter(|l| l.target == "Alexandria")
            .map(|l| Edit::new(l.target_span.clone(), "Pharos"))
            .collect();
        let out = apply(body, &edits).unwrap();
        assert_eq!(
            out,
            "[[Pharos]] but not Alexandria, nor [[Alexandria Library]].\n"
        );
    }

    #[test]
    fn edits_may_be_supplied_in_any_order() {
        let text = "aaa bbb ccc";
        let forward = apply(text, &[Edit::new(0..3, "X"), Edit::new(8..11, "Z")]).unwrap();
        let reverse = apply(text, &[Edit::new(8..11, "Z"), Edit::new(0..3, "X")]).unwrap();
        assert_eq!(forward, "X bbb Z");
        assert_eq!(forward, reverse);
    }

    #[test]
    fn an_empty_edit_set_returns_the_text_unchanged() {
        assert_eq!(apply("unchanged", &[]).unwrap(), "unchanged");
    }

    #[test]
    fn insertions_and_deletions_are_expressible() {
        assert_eq!(apply("ac", &[Edit::new(1..1, "b")]).unwrap(), "abc");
        assert_eq!(apply("abc", &[Edit::new(1..2, "")]).unwrap(), "ac");
    }

    #[test]
    fn overlapping_edits_are_refused_before_anything_is_written() {
        let err = apply("abcdef", &[Edit::new(0..3, "X"), Edit::new(2..5, "Y")]).unwrap_err();
        assert_eq!(
            err,
            EditError::Overlapping {
                first: 0..3,
                second: 2..5
            }
        );
    }

    #[test]
    // The reversed range is the point of the test: a caller that computes
    // `end < start` must be rejected rather than panic inside a slice.
    #[allow(clippy::reversed_empty_ranges)]
    fn out_of_bounds_edits_are_refused() {
        assert!(matches!(
            apply("abc", &[Edit::new(2..9, "X")]),
            Err(EditError::OutOfBounds { .. })
        ));
        assert!(matches!(
            apply("abc", &[Edit::new(3..1, "X")]),
            Err(EditError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn edits_splitting_a_multibyte_character_are_refused() {
        // Vault paths and note titles are UTF-8; a byte-offset bug must not be
        // able to produce invalid text in a user's file.
        let text = "灯台";
        assert!(matches!(
            apply(text, &[Edit::new(0..1, "x")]),
            Err(EditError::NotCharBoundary { .. })
        ));
        assert_eq!(apply(text, &[Edit::new(0..3, "x")]).unwrap(), "x台");
    }

    #[test]
    fn rewriting_works_through_a_frontmatter_offset() {
        let text = "---\ntitle: x\n---\nSee [[Old]].\n";
        let split = crate::frontmatter::split(text);
        let scan = scan(split.body, split.body_offset);
        let out = apply(
            text,
            &[Edit::new(scan.wiki_links[0].target_span.clone(), "New")],
        )
        .unwrap();
        assert_eq!(out, "---\ntitle: x\n---\nSee [[New]].\n");
    }

    #[test]
    fn targets_that_cannot_be_written_as_wiki_links_are_rejected() {
        assert!(is_writable_wiki_target("notes/Pharos of Alexandria"));
        assert!(!is_writable_wiki_target("has|pipe"));
        assert!(!is_writable_wiki_target("has#hash"));
        assert!(!is_writable_wiki_target("has]]brackets"));
    }
}
