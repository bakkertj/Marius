//! # Marius Markdown
//!
//! Everything Marius needs to know about a Markdown file, derived without ever
//! modifying it.
//!
//! Requirements: MD-002, MD-006, MD-007, MD-008, MD-010, MD-011, TAG-004,
//! SRCH-001, SRCH-008, GRAPH-002, ARCH-005.
//!
//! ## Design
//!
//! The user's file is authoritative (ARCH-005), so this crate is read-only by
//! default and reports *spans* rather than rewritten text. The one write path,
//! [`rewrite`], exists because MD-007 requires updating `[[links]]` when their
//! target is renamed, and it applies the smallest possible edit — the exact
//! target span — rather than reserialising the document and reformatting the
//! user's Markdown along the way.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`frontmatter`] | Locate, parse and surgically edit YAML frontmatter (MD-008) |
//! | [`scan`] | Span-accurate links, tags and headings (MD-006, TAG-004) |
//! | [`outline`] | Heading tree and section ranges (MD-011) |
//! | [`text`] | Flatten to prose for the full-text index (SRCH-001) |
//! | [`chunk`] | Heading-aware passages for embedding and citation (SRCH-008) |
//! | [`rewrite`] | Apply minimal byte-range edits (MD-007) |

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod chunk;
pub mod frontmatter;
pub mod outline;
pub mod rewrite;
pub mod scan;
pub mod text;

use std::ops::Range;

use crate::chunk::chunk;
use crate::frontmatter::split;
use crate::scan::scan;
use crate::text::plain_text;

pub use chunk::{ChunkConfig, ChunkDraft};
pub use frontmatter::{Frontmatter, FrontmatterError};
pub use outline::{Outline, OutlineEntry};
pub use rewrite::{apply as apply_edits, Edit, EditError};
pub use scan::{InlineTag, MarkdownLink, Scan, WikiLink};
pub use text::TextOptions;

/// Where a tag was found in a document (TAG-004).
///
/// Mirrors `marius_model::TagSource` without depending on it here, so the
/// mapping into storage stays explicit at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagOrigin {
    /// An inline `#tag` in the body.
    Inline,
    /// A `tags:` entry in frontmatter.
    Frontmatter,
}

/// A tag as written in a document, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundTag {
    /// Raw text as the user wrote it, before canonicalisation.
    pub raw: String,
    pub origin: TagOrigin,
    /// Byte span of the occurrence, for inline tags only.
    pub span: Option<Range<usize>>,
}

/// One Markdown file, fully analysed.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    /// Interpreted frontmatter. Empty when there is none or when it is invalid.
    pub frontmatter: Frontmatter,
    /// The frontmatter block's YAML, kept verbatim for writeback (MD-008).
    pub frontmatter_yaml: Option<String>,
    /// Byte range of the whole frontmatter block, including delimiters.
    pub frontmatter_span: Option<Range<usize>>,
    /// Byte offset where the body begins. Every span below is absolute in the
    /// original text, so citations and edits address the real file.
    pub body_offset: usize,
    /// Why the frontmatter could not be interpreted, if it could not be.
    ///
    /// Recorded rather than returned: a note with a stray colon in its
    /// frontmatter must still open, still index and still be searchable
    /// (SYNC-001, AI-004 — nothing optional may break the core).
    pub frontmatter_error: Option<String>,
    pub scan: Scan,
    pub outline: Outline,
    /// Flattened prose for the full-text index (SRCH-001).
    pub plain_text: String,
    /// Title from frontmatter, else the first level-1 heading. `None` means the
    /// caller should fall back to the filename (KB-007).
    pub title: Option<String>,
    /// Alternate titles this document may be linked by (KB-010).
    pub aliases: Vec<String>,
    /// Tags from both the body and the frontmatter, in that order, deduplicated
    /// case-insensitively on first appearance.
    pub tags: Vec<FoundTag>,
}

impl ParsedDocument {
    /// Analyse a Markdown document.
    ///
    /// Never fails: malformed frontmatter is reported in
    /// [`ParsedDocument::frontmatter_error`] and the body is still analysed.
    pub fn parse(text: &str) -> Self {
        Self::parse_with(text, TextOptions::default())
    }

    /// Analyse a document, controlling what reaches the index.
    pub fn parse_with(text: &str, text_options: TextOptions) -> Self {
        let split = split(text);
        let (frontmatter, frontmatter_error) = match &split.frontmatter {
            Some(block) => Frontmatter::parse_or_recover(block.yaml),
            None => (Frontmatter::default(), None),
        };

        let scan = scan(split.body, split.body_offset);
        let outline = Outline::build(&scan.headings, split.body_offset..text.len());
        let plain_text = plain_text(split.body, &scan, split.body_offset, text_options);

        let title = frontmatter
            .title()
            .or_else(|| outline.first_title().map(str::to_owned));

        let mut tags: Vec<FoundTag> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut remember = |raw: &str| -> bool {
            let key = raw.to_lowercase();
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        };
        for tag in &scan.inline_tags {
            if remember(&tag.name) {
                tags.push(FoundTag {
                    raw: tag.name.clone(),
                    origin: TagOrigin::Inline,
                    span: Some(tag.span.clone()),
                });
            }
        }
        for raw in frontmatter.tags() {
            if remember(&raw) {
                tags.push(FoundTag {
                    raw,
                    origin: TagOrigin::Frontmatter,
                    span: None,
                });
            }
        }

        Self {
            aliases: frontmatter.aliases(),
            frontmatter_yaml: split.frontmatter.as_ref().map(|b| b.yaml.to_owned()),
            frontmatter_span: split.frontmatter.as_ref().map(|b| b.block_span.clone()),
            frontmatter,
            frontmatter_error,
            body_offset: split.body_offset,
            scan,
            outline,
            plain_text,
            title,
            tags,
        }
    }

    /// Passages for embedding and citation (SRCH-008).
    ///
    /// `text` must be the same document this was parsed from.
    pub fn chunks(&self, text: &str, config: &ChunkConfig) -> Vec<ChunkDraft> {
        chunk(text, &self.outline, config)
    }

    /// Everything this document links to, excluding external URLs.
    pub fn link_targets(&self) -> Vec<&str> {
        self.scan.link_targets()
    }

    /// The stable ID written into the file, for vaults that opted into
    /// frontmatter identity.
    pub fn stable_id(&self) -> Option<String> {
        self.frontmatter.stable_id()
    }

    /// Frontmatter keys Marius does not interpret, kept and indexed anyway
    /// (MD-008).
    pub fn unrecognised_frontmatter(&self) -> Vec<&str> {
        self.frontmatter.unrecognised_keys()
    }

    /// Text to index, with the title first so a title match ranks naturally.
    pub fn index_text(&self) -> String {
        match &self.title {
            Some(title) if !self.plain_text.starts_with(title.as_str()) => {
                format!("{title}\n{}", self.plain_text)
            }
            _ => self.plain_text.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const NOTE: &str = "\
---
title: The Pharos of Alexandria
tags: [history, lighthouse]
aliases:
  - Pharos
cssclass: wide
---
# Pharos

It stood at the harbour mouth. See [[Rhodes|the Colossus]] and #antiquity.

## Construction

Begun under Ptolemy I.
";

    #[test]
    fn a_complete_note_is_analysed_end_to_end() {
        let doc = ParsedDocument::parse(NOTE);
        assert_eq!(doc.title.as_deref(), Some("The Pharos of Alexandria"));
        assert_eq!(doc.aliases, vec!["Pharos"]);
        assert_eq!(doc.frontmatter_error, None);
        assert_eq!(doc.link_targets(), vec!["Rhodes"]);
        assert_eq!(doc.outline.entries.len(), 2);
        assert!(doc.plain_text.contains("harbour mouth"));
        assert!(doc.unrecognised_frontmatter().contains(&"cssclass"));
    }

    #[test]
    fn tags_from_the_body_and_the_frontmatter_are_merged_in_order() {
        // TAG-004.
        let doc = ParsedDocument::parse(NOTE);
        let tags: Vec<(&str, TagOrigin)> = doc
            .tags
            .iter()
            .map(|t| (t.raw.as_str(), t.origin))
            .collect();
        assert_eq!(
            tags,
            vec![
                ("antiquity", TagOrigin::Inline),
                ("history", TagOrigin::Frontmatter),
                ("lighthouse", TagOrigin::Frontmatter),
            ]
        );
        // An inline tag keeps its span so the indexer can tell when the user
        // deletes it from the body.
        assert!(doc.tags[0].span.is_some());
        assert!(doc.tags[1].span.is_none());
    }

    #[test]
    fn a_tag_in_both_places_is_reported_once_from_the_body() {
        let doc = ParsedDocument::parse("---\ntags: [Lighthouse]\n---\nAbout #lighthouse.\n");
        assert_eq!(doc.tags.len(), 1);
        assert_eq!(doc.tags[0].origin, TagOrigin::Inline);
    }

    #[test]
    fn the_title_falls_back_to_the_first_h1_then_to_nothing() {
        // KB-007.
        assert_eq!(
            ParsedDocument::parse("# From Heading\n\ntext\n")
                .title
                .as_deref(),
            Some("From Heading")
        );
        assert_eq!(ParsedDocument::parse("just text\n").title, None);
        // Frontmatter wins over the heading.
        assert_eq!(
            ParsedDocument::parse("---\ntitle: Winner\n---\n# Loser\n")
                .title
                .as_deref(),
            Some("Winner")
        );
    }

    #[test]
    fn broken_frontmatter_does_not_make_a_note_unreadable() {
        // Nothing optional may break the core (AI-004, SYNC-001).
        let doc = ParsedDocument::parse("---\ntitle: [unclosed\n---\n# Body\n\nStill here.\n");
        assert!(doc.frontmatter_error.is_some());
        assert_eq!(doc.title.as_deref(), Some("Body"));
        assert!(doc.plain_text.contains("Still here"));
        // The block is still preserved verbatim, so nothing is lost on write.
        assert_eq!(doc.frontmatter_yaml.as_deref(), Some("title: [unclosed\n"));
    }

    #[test]
    fn every_span_addresses_the_original_file() {
        let doc = ParsedDocument::parse(NOTE);
        let link = &doc.scan.wiki_links[0];
        assert_eq!(&NOTE[link.target_span.clone()], "Rhodes");
        let tag = &doc.scan.inline_tags[0];
        assert_eq!(&NOTE[tag.span.clone()], "#antiquity");
        let heading = &doc.outline.entries[1].heading;
        assert_eq!(&NOTE[heading.span.clone()], "## Construction\n");
        assert_eq!(
            &NOTE[doc.frontmatter_span.clone().unwrap()],
            &NOTE[..NOTE.find("# Pharos").unwrap()]
        );
    }

    #[test]
    fn index_text_leads_with_the_title() {
        let doc = ParsedDocument::parse(NOTE);
        assert!(doc.index_text().starts_with("The Pharos of Alexandria"));
        // And does not duplicate it when the body already starts with it.
        let doc = ParsedDocument::parse("# Repeat\n\nbody\n");
        assert_eq!(doc.index_text(), "Repeat\nbody");
    }

    #[test]
    fn chunks_quote_the_original_document() {
        let doc = ParsedDocument::parse(NOTE);
        let chunks = doc.chunks(NOTE, &ChunkConfig::default());
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert_eq!(chunk.text, &NOTE[chunk.span.clone()]);
        }
    }

    #[test]
    fn a_document_with_no_frontmatter_has_no_block_to_write_back_to() {
        let doc = ParsedDocument::parse("# Plain\n");
        assert_eq!(doc.frontmatter_yaml, None);
        assert_eq!(doc.frontmatter_span, None);
        assert_eq!(doc.body_offset, 0);
    }

    #[test]
    fn an_empty_file_parses_to_an_empty_document() {
        let doc = ParsedDocument::parse("");
        assert_eq!(doc.title, None);
        assert!(doc.tags.is_empty());
        assert_eq!(doc.plain_text, "");
        assert!(doc.chunks("", &ChunkConfig::default()).is_empty());
    }
}
