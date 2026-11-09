//! Markdown syntax highlighting as data.
//!
//! Requirements: MD-001, MD-009 (syntax highlighting for the editor), UI-004.
//!
//! Highlighting is computed here as plain `(byte range, token)` pairs and
//! turned into styled runs — no GPUI types, no window — so the rules are
//! testable the same way the buffer is. The editor element maps tokens to
//! colours from the theme and hands GPUI a flat run list.
//!
//! Sources of truth: the real CommonMark parser (via `pulldown-cmark`'s
//! offset iterator) for headings, emphasis, code and quotes, and the same
//! span-accurate scanner the indexer uses for `[[wiki links]]` and `#tags` —
//! so what lights up in the editor is exactly what the index will see, not a
//! lookalike regex's opinion of it.

use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag};

/// What a span of text is, for colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Token {
    /// Body prose: the default, never emitted explicitly.
    Text,
    /// A heading line, any level. The marker and the text both.
    Heading,
    Emphasis,
    Strong,
    Strikethrough,
    /// A `>` quote block.
    Blockquote,
    /// Inline code or a fenced block's contents.
    Code,
    /// A `[text](url)` link.
    Link,
    /// A `[[wiki link]]` the vault resolves.
    WikiLink,
    /// A `[[wiki link]]` that points at nothing yet.
    BrokenWikiLink,
    /// An inline `#tag`.
    Tag,
    /// The YAML frontmatter block.
    Frontmatter,
}

impl Token {
    /// When spans overlap — emphasis inside a heading, a tag inside a quote —
    /// the more specific token wins the bytes. Higher is more specific.
    fn priority(self) -> u8 {
        match self {
            Token::Text => 0,
            Token::Blockquote => 1,
            Token::Heading => 2,
            Token::Emphasis | Token::Strong | Token::Strikethrough => 3,
            Token::Link => 4,
            Token::Frontmatter => 5,
            Token::WikiLink | Token::BrokenWikiLink | Token::Tag => 6,
            Token::Code => 7,
        }
    }
}

/// Decide whether a wiki link target resolves, so broken links can differ
/// visually (MD-007's editing counterpart).
pub trait LinkResolver {
    fn resolves(&self, target: &str) -> bool;
}

/// A resolver for contexts with no vault, such as tests: everything resolves.
pub struct AllLinksResolve;

impl LinkResolver for AllLinksResolve {
    fn resolves(&self, _target: &str) -> bool {
        true
    }
}

/// Compute the token spans for a document.
///
/// Spans may overlap; [`flatten`] turns them into disjoint runs.
pub fn spans(text: &str, resolver: &dyn LinkResolver) -> Vec<(Range<usize>, Token)> {
    let mut out: Vec<(Range<usize>, Token)> = Vec::new();

    // The frontmatter block, if any, keeps its own colour; the body is
    // scanned from its real offset so every span lands in file coordinates.
    let split = marius_markdown::frontmatter::split(text);
    if let Some(block) = &split.frontmatter {
        out.push((block.block_span.clone(), Token::Frontmatter));
    }
    let body = split.body;
    let offset = split.body_offset;

    // Block and inline structure from the real CommonMark parser.
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    for (event, range) in Parser::new_ext(body, options).into_offset_iter() {
        let range = range.start + offset..range.end + offset;
        match event {
            Event::Start(Tag::Heading { .. }) => out.push((range, Token::Heading)),
            Event::Start(Tag::Emphasis) => out.push((range, Token::Emphasis)),
            Event::Start(Tag::Strong) => out.push((range, Token::Strong)),
            Event::Start(Tag::Strikethrough) => out.push((range, Token::Strikethrough)),
            Event::Start(Tag::BlockQuote(_)) => out.push((range, Token::Blockquote)),
            Event::Start(Tag::CodeBlock(_)) => out.push((range, Token::Code)),
            Event::Code(_) => out.push((range, Token::Code)),
            Event::Start(Tag::Link { .. }) | Event::Start(Tag::Image { .. }) => {
                out.push((range, Token::Link))
            }
            _ => {}
        }
    }

    // Wiki links and tags from the indexer's own scanner.
    let scan = marius_markdown::scan::scan(body, offset);
    for link in &scan.wiki_links {
        let token = if link.is_internal() || resolver.resolves(&link.target) {
            Token::WikiLink
        } else {
            Token::BrokenWikiLink
        };
        out.push((link.span.clone(), token));
    }
    for tag in &scan.inline_tags {
        out.push((tag.span.clone(), Token::Tag));
    }

    out
}

/// Flatten possibly-overlapping spans into disjoint runs covering `0..len`.
///
/// Where spans overlap, the highest-priority token takes the bytes. The output
/// is sorted, gapless and boundary-safe: exactly what a text shaper's run list
/// has to be.
pub fn flatten(spans: &[(Range<usize>, Token)], len: usize) -> Vec<(Range<usize>, Token)> {
    if len == 0 {
        return Vec::new();
    }
    // Sweep over every boundary point.
    let mut bounds: Vec<usize> = Vec::with_capacity(spans.len() * 2 + 2);
    bounds.push(0);
    bounds.push(len);
    for (range, _) in spans {
        if range.start < len {
            bounds.push(range.start);
        }
        bounds.push(range.end.min(len));
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out: Vec<(Range<usize>, Token)> = Vec::with_capacity(bounds.len());
    for window in bounds.windows(2) {
        let (start, end) = (window[0], window[1]);
        if start >= end {
            continue;
        }
        let token = spans
            .iter()
            .filter(|(range, _)| range.start <= start && range.end >= end)
            .map(|(_, token)| *token)
            .max_by_key(|token| token.priority())
            .unwrap_or(Token::Text);
        // Merge with the previous run when the token repeats.
        match out.last_mut() {
            Some((range, last)) if *last == token && range.end == start => range.end = end,
            _ => out.push((start..end, token)),
        }
    }
    out
}

/// Spans flattened for a whole document in one call.
pub fn runs(text: &str, resolver: &dyn LinkResolver) -> Vec<(Range<usize>, Token)> {
    flatten(&spans(text, resolver), text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs_of(text: &str) -> Vec<(Range<usize>, Token)> {
        runs(text, &AllLinksResolve)
    }

    fn token_at(text: &str, needle: &str) -> Token {
        let at = text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in text"));
        let mid = at + needle.len() / 2;
        runs_of(text)
            .into_iter()
            .find(|(range, _)| range.contains(&mid))
            .map(|(_, token)| token)
            .unwrap_or(Token::Text)
    }

    #[test]
    fn runs_are_gapless_sorted_and_boundary_safe() {
        // The structural contract a text shaper depends on.
        let text = "---\ntitle: x\n---\n# Head **bold** `code` [[Link]] #tag\n\nplain 灯台\n";
        let runs = runs_of(text);
        assert_eq!(runs.first().map(|(r, _)| r.start), Some(0));
        assert_eq!(runs.last().map(|(r, _)| r.end), Some(text.len()));
        for pair in runs.windows(2) {
            assert_eq!(pair[0].0.end, pair[1].0.start, "gap or overlap: {pair:?}");
            assert_ne!(pair[0].1, pair[1].1, "adjacent runs should be merged");
        }
        for (range, _) in &runs {
            assert!(text.is_char_boundary(range.start));
            assert!(text.is_char_boundary(range.end));
        }
    }

    #[test]
    fn each_construct_gets_its_token() {
        let text = "# Heading\n\nSome *em* and **strong** and ~~gone~~ text.\n\n\
                    > a quote\n\n`inline` and [a link](x.md) and [[Wiki]] and #tag\n\n\
                    ```rust\nlet x = 1;\n```\n";
        assert_eq!(token_at(text, "Heading"), Token::Heading);
        assert_eq!(token_at(text, "*em*"), Token::Emphasis);
        assert_eq!(token_at(text, "strong"), Token::Strong);
        assert_eq!(token_at(text, "gone"), Token::Strikethrough);
        assert_eq!(token_at(text, "a quote"), Token::Blockquote);
        assert_eq!(token_at(text, "inline"), Token::Code);
        assert_eq!(token_at(text, "a link"), Token::Link);
        assert_eq!(token_at(text, "Wiki"), Token::WikiLink);
        assert_eq!(token_at(text, "#tag"), Token::Tag);
        assert_eq!(token_at(text, "let x"), Token::Code);
        assert_eq!(token_at(text, "text."), Token::Text);
    }

    #[test]
    fn frontmatter_is_one_region() {
        let text = "---\ntitle: Pharos\ntags: [a]\n---\nbody\n";
        assert_eq!(token_at(text, "title"), Token::Frontmatter);
        assert_eq!(token_at(text, "body"), Token::Text);
    }

    #[test]
    fn a_tag_inside_a_code_fence_is_code_not_a_tag() {
        // The scanner already refuses it; the flattener must not resurrect it
        // by layering, and code outranks everything on priority.
        let text = "```\n#define X\n```\n\n#real\n";
        assert_eq!(token_at(text, "#define"), Token::Code);
        assert_eq!(token_at(text, "#real"), Token::Tag);
    }

    #[test]
    fn emphasis_inside_a_heading_wins_the_overlap() {
        let text = "# A *big* day\n";
        assert_eq!(token_at(text, "big"), Token::Emphasis);
        assert_eq!(token_at(text, "day"), Token::Heading);
    }

    #[test]
    fn broken_wiki_links_are_distinguished_by_the_resolver() {
        struct OnlyPharos;
        impl LinkResolver for OnlyPharos {
            fn resolves(&self, target: &str) -> bool {
                target == "Pharos"
            }
        }
        let text = "[[Pharos]] and [[Missing Note]]";
        let runs = runs(text, &OnlyPharos);
        let at = |needle: &str| {
            let mid = text.find(needle).unwrap() + 1;
            runs.iter()
                .find(|(r, _)| r.contains(&mid))
                .map(|(_, t)| *t)
                .unwrap()
        };
        assert_eq!(at("[[Pharos]]"), Token::WikiLink);
        assert_eq!(at("[[Missing"), Token::BrokenWikiLink);
    }

    #[test]
    fn highlighting_is_identical_to_what_the_indexer_sees() {
        // The claim in the module docs, held to: every tag and wiki link the
        // indexer would extract is highlighted, and nothing else is.
        let text = "# T\n\nreal #one and [[Two]] but `#not-this` and <https://x.y/#nor-this>\n";
        let parsed = marius_markdown::ParsedDocument::parse(text);
        let runs = runs_of(text);

        let highlighted_tags: Vec<&str> = runs
            .iter()
            .filter(|(_, t)| *t == Token::Tag)
            .map(|(r, _)| &text[r.clone()])
            .collect();
        let indexed_tags: Vec<String> = parsed
            .tags
            .iter()
            .filter(|t| t.span.is_some())
            .map(|t| format!("#{}", t.raw))
            .collect();
        assert_eq!(highlighted_tags, indexed_tags);

        let highlighted_links = runs.iter().filter(|(_, t)| *t == Token::WikiLink).count();
        assert_eq!(highlighted_links, parsed.scan.wiki_links.len());
    }

    #[test]
    fn an_empty_document_produces_no_runs() {
        assert!(runs_of("").is_empty());
    }

    #[test]
    fn plain_prose_is_a_single_text_run() {
        let runs = runs_of("just ordinary prose with nothing in it\n");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1, Token::Text);
    }
}
