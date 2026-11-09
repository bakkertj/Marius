//! Document outline: the heading tree and the byte range each heading owns.
//!
//! Requirements: MD-011 (document outlines), MD-006 (resolving `[[note#heading]]`
//! to a location), SRCH-008 (section-level indexing), SRCH-010 (citing a
//! passage the reader can click through to).

use std::ops::Range;

use crate::scan::{fragment_key, Heading};

/// One heading with the ranges it governs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    pub heading: Heading,
    /// Everything under this heading, including nested subsections. Ends at the
    /// next heading of the same or shallower level.
    pub section: Range<usize>,
    /// Just this heading's own prose, ending at the next heading of any level.
    pub content: Range<usize>,
    /// Heading texts from the document root down to and including this one.
    /// This is what a citation label is built from.
    pub path: Vec<String>,
}

/// A document's headings, flattened but path-aware.
///
/// A flat list with an explicit `path` serves every consumer better than a
/// nested tree: the sidebar renders from `heading.level`, chunking reads
/// `path`, and fragment resolution scans linearly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outline {
    pub entries: Vec<OutlineEntry>,
    /// Content before the first heading, which belongs to no section but is
    /// still indexable and citable.
    pub preamble: Range<usize>,
}

impl Outline {
    /// Build an outline from a scan's headings.
    ///
    /// `body` is the range the headings were scanned from, in the same absolute
    /// coordinates the headings use.
    pub fn build(headings: &[Heading], body: Range<usize>) -> Self {
        let first = headings.first().map_or(body.end, |h| h.span.start);
        let mut entries: Vec<OutlineEntry> = Vec::with_capacity(headings.len());
        // Heading texts of the open ancestors, indexed by level.
        let mut stack: Vec<(u8, String)> = Vec::new();

        for (i, heading) in headings.iter().enumerate() {
            while stack
                .last()
                .is_some_and(|(level, _)| *level >= heading.level)
            {
                stack.pop();
            }
            let mut path: Vec<String> = stack.iter().map(|(_, text)| text.clone()).collect();
            path.push(heading.text.clone());
            stack.push((heading.level, heading.text.clone()));

            let content_end = headings.get(i + 1).map_or(body.end, |next| next.span.start);
            let section_end = headings[i + 1..]
                .iter()
                .find(|next| next.level <= heading.level)
                .map_or(body.end, |next| next.span.start);

            entries.push(OutlineEntry {
                heading: heading.clone(),
                section: heading.span.start..section_end.max(heading.span.start),
                content: heading.content_start..content_end.max(heading.content_start),
                path,
            });
        }

        Self {
            entries,
            preamble: body.start..first.max(body.start),
        }
    }

    /// Resolve a `#fragment` from a wiki or Markdown link to its section.
    ///
    /// Matching is forgiving about case and punctuation (see
    /// [`crate::scan::fragment_key`]) because that is how people type
    /// fragments; an exact match is preferred when several headings collide.
    pub fn resolve_fragment(&self, fragment: &str) -> Option<&OutlineEntry> {
        let key = fragment_key(fragment);
        if key.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .find(|e| e.heading.text == fragment)
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| e.heading.fragment_key() == key)
            })
    }

    /// The deepest heading containing `offset`, for "where am I" in the editor.
    pub fn enclosing(&self, offset: usize) -> Option<&OutlineEntry> {
        self.entries.iter().rfind(|e| e.section.contains(&offset))
    }

    /// Top-level headings, for a collapsed sidebar.
    pub fn roots(&self) -> impl Iterator<Item = &OutlineEntry> {
        let min = self.entries.iter().map(|e| e.heading.level).min();
        self.entries
            .iter()
            .filter(move |e| Some(e.heading.level) == min)
    }

    /// The document's first level-1 heading, used as a title fallback (KB-007).
    pub fn first_title(&self) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.heading.level == 1 && !e.heading.text.is_empty())
            .map(|e| e.heading.text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;
    use pretty_assertions::assert_eq;

    fn outline_of(body: &str) -> Outline {
        let scan = scan(body, 0);
        Outline::build(&scan.headings, 0..body.len())
    }

    const DOC: &str = "\
intro text

# One

one body

## One A

a body

## One B

b body

# Two

two body
";

    #[test]
    fn paths_record_the_heading_hierarchy() {
        let outline = outline_of(DOC);
        let paths: Vec<Vec<String>> = outline.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                vec!["One"],
                vec!["One", "One A"],
                vec!["One", "One B"],
                vec!["Two"],
            ]
        );
    }

    #[test]
    fn a_section_covers_its_subsections_but_content_does_not() {
        let outline = outline_of(DOC);
        let one = &outline.entries[0];
        assert!(
            DOC[one.section.clone()].contains("One A"),
            "section must nest"
        );
        assert!(
            !DOC[one.content.clone()].contains("One A"),
            "content must not"
        );
        assert_eq!(DOC[one.content.clone()].trim(), "one body");
    }

    #[test]
    fn the_last_section_runs_to_the_end_of_the_document() {
        let outline = outline_of(DOC);
        let two = outline.entries.last().unwrap();
        assert_eq!(two.section.end, DOC.len());
        assert_eq!(DOC[two.content.clone()].trim(), "two body");
    }

    #[test]
    fn content_before_the_first_heading_is_the_preamble() {
        let outline = outline_of(DOC);
        assert_eq!(DOC[outline.preamble.clone()].trim(), "intro text");
    }

    #[test]
    fn a_document_with_no_headings_is_all_preamble() {
        let body = "just prose\n";
        let outline = outline_of(body);
        assert!(outline.entries.is_empty());
        assert_eq!(outline.preamble, 0..body.len());
        assert_eq!(outline.first_title(), None);
    }

    #[test]
    fn fragments_resolve_case_and_punctuation_insensitively() {
        // MD-006.
        let body = "# Construction, 280 BC\n\ntext\n";
        let outline = outline_of(body);
        for fragment in [
            "Construction, 280 BC",
            "construction 280 bc",
            "CONSTRUCTION,280  BC",
        ] {
            assert!(outline.resolve_fragment(fragment).is_some(), "{fragment:?}");
        }
        assert!(outline.resolve_fragment("Nonexistent").is_none());
        assert!(outline.resolve_fragment("").is_none());
    }

    #[test]
    fn an_exact_fragment_match_wins_over_a_normalised_one() {
        let body = "# A B\n\nx\n\n# a-b\n\ny\n";
        let outline = outline_of(body);
        let hit = outline.resolve_fragment("a-b").unwrap();
        assert_eq!(hit.heading.text, "a-b");
    }

    #[test]
    fn the_enclosing_section_is_the_deepest_one() {
        let outline = outline_of(DOC);
        let offset = DOC.find("a body").unwrap();
        assert_eq!(outline.enclosing(offset).unwrap().heading.text, "One A");

        let offset = DOC.find("one body").unwrap();
        assert_eq!(outline.enclosing(offset).unwrap().heading.text, "One");

        assert!(
            outline.enclosing(0).is_none(),
            "the preamble has no section"
        );
    }

    #[test]
    fn roots_are_the_shallowest_level_present() {
        let outline = outline_of(DOC);
        let roots: Vec<&str> = outline.roots().map(|e| e.heading.text.as_str()).collect();
        assert_eq!(roots, vec!["One", "Two"]);

        // A document whose shallowest heading is `##` still has roots.
        let outline = outline_of("## A\n\nx\n\n### B\n\ny\n\n## C\n");
        let roots: Vec<&str> = outline.roots().map(|e| e.heading.text.as_str()).collect();
        assert_eq!(roots, vec!["A", "C"]);
    }

    #[test]
    fn a_skipped_level_does_not_break_the_path() {
        // `# One` then `### Deep` — malformed but common.
        let outline = outline_of("# One\n\nx\n\n### Deep\n\ny\n");
        assert_eq!(outline.entries[1].path, vec!["One", "Deep"]);
    }

    #[test]
    fn the_first_level_one_heading_is_the_title_fallback() {
        // KB-007.
        assert_eq!(outline_of(DOC).first_title(), Some("One"));
        assert_eq!(outline_of("## Only H2\n").first_title(), None);
    }

    #[test]
    fn spans_stay_absolute_when_the_body_is_offset() {
        // Frontmatter shifts the body; citations must address the real file.
        let text = "---\na: 1\n---\n# One\n\nbody\n";
        let split = crate::frontmatter::split(text);
        let scan = scan(split.body, split.body_offset);
        let outline = Outline::build(&scan.headings, split.body_offset..text.len());
        let one = &outline.entries[0];
        assert_eq!(&text[one.heading.span.clone()], "# One\n");
        assert_eq!(text[one.content.clone()].trim(), "body");
    }
}
