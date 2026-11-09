//! Readable text extraction for the full-text index.
//!
//! Requirements: SRCH-001 (full-text search over titles and contents), SRCH-003
//! (phrase matching and snippets), MD-002 (tables, checklists, fenced code).
//!
//! Here the CommonMark parser earns its keep: it already understands tables,
//! nested lists, block quotes, footnotes and task lists, so extraction is a
//! walk over its events rather than another hand-written parser.

use marius_model::VaultPath;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::scan::Scan;

/// Parser options: GitHub Flavored Markdown plus the extensions the spec names
/// (MD-002, MD-010).
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM
}

/// What to include when flattening a document to indexable text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextOptions {
    /// Include the contents of fenced and inline code. On by default: people
    /// search their notes for the command they wrote down.
    pub include_code: bool,
    /// Include image and link alt text.
    pub include_alt_text: bool,
    /// Include link destinations, so searching for a filename finds the notes
    /// that reference it.
    pub include_link_targets: bool,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            include_code: true,
            include_alt_text: true,
            include_link_targets: true,
        }
    }
}

/// Flatten a Markdown body to plain text for indexing.
///
/// `scan` is used to turn `[[Target|Display]]` into `Target Display`, so a
/// wiki link is findable by either name instead of appearing as bracket soup
/// that no tokeniser handles well.
pub fn plain_text(body: &str, scan: &Scan, offset: usize, options_: TextOptions) -> String {
    let prepared = expand_wiki_links(body, scan, offset);
    render(&prepared, options_)
}

/// Replace wiki-link constructs with their searchable words.
fn expand_wiki_links(body: &str, scan: &Scan, offset: usize) -> String {
    if scan.wiki_links.is_empty() {
        return body.to_owned();
    }
    let mut out = String::with_capacity(body.len());
    let mut cursor = 0usize;
    for link in &scan.wiki_links {
        let start = link.span.start.saturating_sub(offset);
        let end = link.span.end.saturating_sub(offset);
        if start < cursor || end > body.len() {
            continue;
        }
        out.push_str(&body[cursor..start]);
        // Target first, then heading and display text, each separated so the
        // tokeniser sees distinct words.
        out.push_str(&link.target);
        if let Some(heading) = &link.heading {
            out.push(' ');
            out.push_str(heading);
        }
        if let Some(display) = &link.display {
            out.push(' ');
            out.push_str(display);
        }
        cursor = end;
    }
    out.push_str(&body[cursor..]);
    out
}

fn render(text: &str, opts: TextOptions) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code_block = false;

    for event in Parser::new_ext(text, options()) {
        match event {
            // Literal text is appended verbatim. The parser's text events
            // reproduce the source byte-for-byte once markup is removed, so
            // concatenation preserves the author's spacing — inventing
            // separators here is what turns `_lighthouse_` into
            // "  lighthouse  " and strands the following full stop.
            Event::Text(t) => {
                // A fenced block's *content* arrives as `Text`, not `Code`.
                if !in_code_block || opts.include_code {
                    out.push_str(&t);
                }
            }
            Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                if opts.include_code {
                    out.push_str(&t);
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                push_break(&mut out);
                if opts.include_code {
                    if let CodeBlockKind::Fenced(lang) = kind {
                        push_words(&mut out, &lang);
                        push_break(&mut out);
                    }
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                push_break(&mut out);
            }
            Event::Start(Tag::Image {
                dest_url, title, ..
            })
            | Event::Start(Tag::Link {
                dest_url, title, ..
            }) => {
                if opts.include_alt_text {
                    push_words(&mut out, &title);
                }
                if opts.include_link_targets {
                    push_words(&mut out, &readable_target(&dest_url));
                }
            }
            Event::Start(Tag::FootnoteDefinition(label)) => push_words(&mut out, &label),
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::TableCell
                | TagEnd::TableRow
                | TagEnd::BlockQuote(_),
            ) => push_break(&mut out),
            Event::SoftBreak | Event::HardBreak | Event::Rule => push_break(&mut out),
            Event::TaskListMarker(done) => push_words(&mut out, if done { "[x]" } else { "[ ]" }),
            Event::Html(t) | Event::InlineHtml(t) => push_words(&mut out, &strip_tags(&t)),
            Event::FootnoteReference(_) | Event::Start(_) | Event::End(_) => {}
        }
    }
    tidy(&out)
}

/// Collapse horizontal whitespace, trim each line and drop blank lines.
///
/// Snippets are shown to the user (SRCH-003), so the index text has to read
/// like prose rather than like a parser's leftovers.
fn tidy(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let mut wrote = false;
        for word in line.split_whitespace() {
            if wrote {
                out.push(' ');
            }
            out.push_str(word);
            wrote = true;
        }
        if wrote {
            out.push('\n');
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// A link destination's human-readable part.
///
/// A whole URL tokenises badly, so only the final path segment is indexed —
/// `images/pharos.png` contributes `pharos.png`, which is what a user would
/// actually type.
fn readable_target(dest: &str) -> String {
    let dest = dest.split(['#', '?']).next().unwrap_or(dest);
    match VaultPath::new(dest) {
        Ok(path) => path.file_name().to_owned(),
        Err(_) => dest.rsplit('/').next().unwrap_or(dest).to_owned(),
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut depth = 0usize;
    for ch in html.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn push_words(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with([' ', '\n']) {
        out.push(' ');
    }
    out.push_str(text.trim_end_matches('\n'));
}

fn push_break(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;

    fn text(body: &str) -> String {
        plain_text(body, &scan::scan(body, 0), 0, TextOptions::default())
    }

    #[test]
    fn markup_is_stripped_but_words_are_kept() {
        let out = text("# Pharos\n\nThe **tallest** _lighthouse_ of ~~antiquity~~.\n");
        assert_eq!(out, "Pharos\nThe tallest lighthouse of antiquity.");
    }

    #[test]
    fn tables_checklists_and_quotes_are_flattened() {
        // MD-002.
        let out = text(
            "| Name | Year |\n|---|---|\n| Pharos | 280 BC |\n\n- [x] done\n- [ ] todo\n\n> quoted\n",
        );
        for expected in ["Name", "Year", "Pharos", "280 BC", "done", "todo", "quoted"] {
            assert!(out.contains(expected), "{expected:?} missing from {out:?}");
        }
    }

    #[test]
    fn code_is_indexed_so_notes_about_commands_are_findable() {
        let out = text("Run `cargo test` or:\n\n```bash\nmarius index --vault notes\n```\n");
        assert!(out.contains("cargo test"));
        assert!(out.contains("marius index"));
        assert!(out.contains("bash"));
    }

    #[test]
    fn code_can_be_excluded() {
        let body = "Prose here.\n\n```\nsecret_token\n```\n";
        let out = plain_text(
            body,
            &scan::scan(body, 0),
            0,
            TextOptions {
                include_code: false,
                ..Default::default()
            },
        );
        assert!(out.contains("Prose"));
        assert!(!out.contains("secret_token"));
    }

    #[test]
    fn wiki_links_are_searchable_by_target_and_display_text() {
        let out = text("See [[research/Alexandria|the Pharos]] and [[Rhodes]].");
        assert!(out.contains("research/Alexandria"), "{out:?}");
        assert!(out.contains("the Pharos"), "{out:?}");
        assert!(out.contains("Rhodes"), "{out:?}");
        assert!(!out.contains("[["), "{out:?}");
    }

    #[test]
    fn wiki_link_headings_are_indexed_too() {
        let out = text("[[Alexandria#Construction]]");
        assert!(out.contains("Alexandria"));
        assert!(out.contains("Construction"));
    }

    #[test]
    fn link_destinations_contribute_their_filename_not_the_whole_url() {
        let out =
            text("![diagram](images/pharos-plan.png) and [spec](https://example.com/a/b/spec.pdf)");
        assert!(out.contains("pharos-plan.png"), "{out:?}");
        assert!(out.contains("spec.pdf"), "{out:?}");
        assert!(!out.contains("https"), "{out:?}");
    }

    #[test]
    fn html_markup_is_removed_but_its_text_survives() {
        let out = text("<div class=\"note\">kept text</div>\n");
        assert!(out.contains("kept text"));
        assert!(!out.contains("div"));
    }

    #[test]
    fn math_is_indexed_as_text() {
        // MD-010.
        let out = text("The height $h = 100$ metres.\n");
        assert!(out.contains("h = 100"), "{out:?}");
    }

    #[test]
    fn footnotes_are_indexed() {
        let out = text("Claim[^a]\n\n[^a]: the evidence\n");
        assert!(out.contains("the evidence"), "{out:?}");
    }

    #[test]
    fn an_empty_document_yields_empty_text() {
        assert_eq!(text(""), "");
        assert_eq!(text("   \n\n  "), "");
    }

    #[test]
    fn words_never_run_together_across_blocks() {
        // A snippet that reads "PharosRhodes" would match neither word.
        let out = text("# Pharos\n## Rhodes\n");
        assert!(!out.contains("PharosRhodes"), "{out:?}");
    }
}
