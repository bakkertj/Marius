//! Rendered reading mode.
//!
//! Requirements: MD-003 (rendered reading view), MD-002 (tables, checklists,
//! fenced code), MD-005 (images and local attachments), MD-006 (wiki links —
//! clickable here, so reading mode is also how you *browse*), MD-012
//! (quotes), UI-004.
//!
//! The same split as the editor: everything decidable without a window —
//! turning Markdown into a tree of typed blocks with styled inline spans and
//! link targets — is a pure function with tests. The GPUI half walks that
//! tree and makes elements, and is kept too boring to be wrong.
//!
//! Wiki links are not CommonMark, so the source is first *wikified*: each
//! `[[target|display]]` becomes an ordinary link with a `marius:` scheme,
//! using the indexer's own span-accurate scanner — the one component in the
//! codebase entitled to an opinion about where a wiki link starts and ends.

use std::ops::Range;
use std::path::PathBuf;

use gpui::{
    actions, div, img, point, prelude::*, px, Context, ElementId, EventEmitter, FocusHandle,
    FontStyle, FontWeight, HighlightStyle, InteractiveText, KeyBinding, ScrollHandle, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle, Window,
};

actions!(
    reading,
    [PageDown, PageUp, LineDown, LineUp, GoTop, GoBottom]
);

/// Bindings live on the pane's own focus; a bare spacebar anywhere
/// wider would steal typing from the editor and the search box.
pub fn keybindings() -> Vec<KeyBinding> {
    const CTX: Option<&str> = Some("ReadingPane");
    vec![
        KeyBinding::new("pagedown", PageDown, CTX),
        KeyBinding::new("pageup", PageUp, CTX),
        KeyBinding::new("space", PageDown, CTX),
        KeyBinding::new("shift-space", PageUp, CTX),
        KeyBinding::new("down", LineDown, CTX),
        KeyBinding::new("up", LineUp, CTX),
        KeyBinding::new("home", GoTop, CTX),
        KeyBinding::new("end", GoBottom, CTX),
    ]
}
use marius_markdown::rewrite::{apply, Edit};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::theme::Theme;

/// Scheme marking a converted `[[wiki link]]`.
pub const WIKI_SCHEME: &str = "marius:";
/// Scheme marking a converted `![[embed]]`.
pub const EMBED_SCHEME: &str = "marius-embed:";

/// Replace wiki syntax with schemed CommonMark links, so one parser handles
/// everything downstream.
///
/// Destinations are wrapped in `<…>` because wiki targets routinely contain
/// spaces, which an unwrapped CommonMark destination cannot.
pub fn wikify(source: &str) -> String {
    let scan = marius_markdown::scan::scan(source, 0);
    let mut edits = Vec::with_capacity(scan.wiki_links.len());
    for link in &scan.wiki_links {
        if link.is_internal() {
            // `[[#heading]]` scrolling is not built; leave the text alone.
            continue;
        }
        let label = link.display_text().to_owned();
        let target = &link.target;
        let replacement = if link.is_embed {
            format!("![{label}](<{EMBED_SCHEME}{target}>)")
        } else {
            format!("[{label}](<{WIKI_SCHEME}{target}>)")
        };
        edits.push(Edit::new(link.span.clone(), replacement));
    }
    apply(source, &edits).unwrap_or_else(|_| source.to_owned())
}

/// Inline emphasis state. A plain struct rather than GPUI styles, so the
/// parser half stays windowless and testable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: bool,
    /// A link whose target is a wiki target that does not resolve.
    pub broken: bool,
}

impl SpanStyle {
    fn is_plain(&self) -> bool {
        *self == Self::default()
    }
}

/// One run of inline text with uniform styling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    pub spans: Vec<(Range<usize>, SpanStyle)>,
    /// Click targets: byte range in `text` and the raw destination.
    pub links: Vec<(Range<usize>, String)>,
    /// Set while the inline consists of exactly one `$$…$$` block, so
    /// the paragraph can hoist it to a display-math block (MD-010).
    pub sole_math: Option<String>,
    /// Byte offset of the first soft or hard break, for the callout
    /// recogniser: the title is the marker's own line, and flattening
    /// breaks to spaces would otherwise erase that boundary.
    pub first_break: Option<usize>,
}

/// A rendered block.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        inline: Inline,
    },
    Paragraph(Inline),
    Code {
        lang: Option<String>,
        text: String,
    },
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Table {
        head: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
    Rule,
    /// An image standing alone, rendered as a figure.
    Image {
        destination: String,
        alt: String,
    },
    /// An Obsidian-style callout: `> [!note] Title` (MD-012).
    Callout {
        kind: String,
        title: String,
        children: Vec<Block>,
    },
    /// Display math, `$$…$$`. Shown distinctly, not yet typeset (MD-010).
    Math(String),
}

/// One list item: an optional task state and its content blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// Parse a (wikified) body into blocks. Pure; tested below.
pub fn parse_blocks(body: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM;
    let mut parser = Parser::new_ext(body, options);
    let blocks = collect_blocks(&mut parser, None);
    // A PDF import used to weld whole documents into one paragraph, and
    // existing notes still carry them. One colossal text run is heavy
    // enough to upset layout (a user's reading view scrolled only part
    // way); a plain paragraph splits losslessly at word boundaries.
    blocks
        .into_iter()
        .flat_map(|block| match block {
            Block::Paragraph(inline)
                if inline.text.len() > 4_000
                    && inline.spans.is_empty()
                    && inline.links.is_empty() =>
            {
                split_plain_paragraph(&inline.text)
            }
            other => vec![other],
        })
        .collect()
}

/// Break an unstyled paragraph into ~2,000-char pieces at whitespace.
fn split_plain_paragraph(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while rest.len() > 2_000 {
        let boundary = {
            let mut at = 2_000;
            while at < rest.len() && !rest.is_char_boundary(at) {
                at += 1;
            }
            at.min(rest.len())
        };
        let window = &rest[..boundary];
        let Some(at) = window.rfind(char::is_whitespace).filter(|&a| a > 0) else {
            break;
        };
        out.push(Block::Paragraph(Inline {
            text: rest[..at].trim_end().to_owned(),
            ..Inline::default()
        }));
        rest = rest[at..].trim_start();
    }
    if !rest.is_empty() {
        out.push(Block::Paragraph(Inline {
            text: rest.to_owned(),
            ..Inline::default()
        }));
    }
    out
}

/// Collect blocks until the given end tag (or the end of input).
fn collect_blocks(parser: &mut Parser, until: Option<TagEnd>) -> Vec<Block> {
    let mut blocks = Vec::new();
    while let Some(event) = parser.next() {
        match event {
            Event::End(end) if Some(end) == until => break,
            Event::Start(Tag::Heading { level, .. }) => {
                let inline = collect_inline(parser, TagEnd::Heading(level));
                blocks.push(Block::Heading {
                    level: level as u8,
                    inline,
                });
            }
            Event::Start(Tag::Paragraph) => {
                let inline = collect_inline(parser, TagEnd::Paragraph);
                // A paragraph that is exactly one image renders as a figure.
                if let Some((range, dest)) = sole_image(&inline) {
                    blocks.push(Block::Image {
                        destination: dest,
                        alt: inline.text[range].to_owned(),
                    });
                } else if let Some(source) = inline.sole_math.clone() {
                    blocks.push(Block::Math(source));
                } else if !inline.text.trim().is_empty() || !inline.links.is_empty() {
                    blocks.push(Block::Paragraph(inline));
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    _ => None,
                };
                let mut text = String::new();
                for event in parser.by_ref() {
                    match event {
                        Event::Text(t) => text.push_str(&t),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                }
                blocks.push(Block::Code {
                    lang,
                    text: text.trim_end_matches('\n').to_owned(),
                });
            }
            Event::Start(Tag::BlockQuote(kind)) => {
                let inner = collect_blocks(parser, Some(TagEnd::BlockQuote(None)));
                // GFM's own alert set is recognised by the parser and
                // arrives on the tag; Obsidian's wider set (and titled
                // callouts) arrive as text for `callout_or_quote`.
                blocks.push(match kind {
                    Some(gfm) => {
                        let kind = match gfm {
                            pulldown_cmark::BlockQuoteKind::Note => "note",
                            pulldown_cmark::BlockQuoteKind::Tip => "tip",
                            pulldown_cmark::BlockQuoteKind::Important => "important",
                            pulldown_cmark::BlockQuoteKind::Warning => "warning",
                            pulldown_cmark::BlockQuoteKind::Caution => "caution",
                        };
                        let mut title = kind.to_owned();
                        if let Some(first) = title.get_mut(0..1) {
                            first.make_ascii_uppercase();
                        }
                        Block::Callout {
                            kind: kind.to_owned(),
                            title,
                            children: inner,
                        }
                    }
                    None => callout_or_quote(inner),
                });
            }
            Event::Start(Tag::List(start)) => {
                let end = TagEnd::List(start.is_some());
                let mut items = Vec::new();
                while let Some(event) = parser.next() {
                    match event {
                        Event::Start(Tag::Item) => {
                            items.push(collect_item(parser));
                        }
                        Event::End(e) if e == end => break,
                        _ => {}
                    }
                }
                blocks.push(Block::List { start, items });
            }
            Event::DisplayMath(src) => {
                blocks.push(Block::Math(src.to_string()));
            }
            Event::Start(Tag::Table(_)) => {
                blocks.push(collect_table(parser));
            }
            Event::Rule => blocks.push(Block::Rule),
            _ => {}
        }
    }
    blocks
}

/// A list item's content; loose items contain paragraphs, tight ones bare
/// inlines, and both should read the same.
fn collect_item(parser: &mut Parser) -> ListItem {
    let mut task = None;
    let mut blocks = Vec::new();
    let mut bare = Inline::default();
    while let Some(event) = parser.next() {
        match event {
            Event::End(TagEnd::Item) => break,
            Event::TaskListMarker(done) => task = Some(done),
            Event::Start(Tag::Paragraph) => {
                let inline = collect_inline(parser, TagEnd::Paragraph);
                blocks.push(Block::Paragraph(inline));
            }
            Event::Start(Tag::List(start)) => {
                let end = TagEnd::List(start.is_some());
                let mut items = Vec::new();
                while let Some(event) = parser.next() {
                    match event {
                        Event::Start(Tag::Item) => items.push(collect_item(parser)),
                        Event::End(e) if e == end => break,
                        _ => {}
                    }
                }
                blocks.push(Block::List { start, items });
            }
            // Tight list content arrives as bare inline events.
            other => append_inline_event(&mut bare, other, parser),
        }
    }
    if !bare.text.is_empty() {
        blocks.insert(0, Block::Paragraph(bare));
    }
    ListItem { task, blocks }
}

fn collect_table(parser: &mut Parser) -> Block {
    let mut head = Vec::new();
    let mut rows: Vec<Vec<Inline>> = Vec::new();
    let mut in_head = false;
    let mut current: Vec<Inline> = Vec::new();
    for event in parser.by_ref() {
        match event {
            Event::Start(Tag::TableHead) => in_head = true,
            Event::End(TagEnd::TableHead) => {
                in_head = false;
                head = std::mem::take(&mut current);
            }
            Event::Start(Tag::TableRow) => current.clear(),
            Event::End(TagEnd::TableRow) => rows.push(std::mem::take(&mut current)),
            Event::Start(Tag::TableCell) => current.push(Inline::default()),
            Event::Text(t) => {
                if let Some(cell) = current.last_mut() {
                    cell.text.push_str(&t);
                }
            }
            Event::Code(t) => {
                if let Some(cell) = current.last_mut() {
                    let start = cell.text.len();
                    cell.text.push_str(&t);
                    cell.spans.push((
                        start..cell.text.len(),
                        SpanStyle {
                            code: true,
                            ..Default::default()
                        },
                    ));
                }
            }
            Event::End(TagEnd::Table) => break,
            _ => {}
        }
    }
    let _ = in_head;
    Block::Table { head, rows }
}

/// `> [!note] Title` and kin become callouts (MD-012); any other quote
/// stays a quote. The marker must open the quote's first paragraph.
fn callout_or_quote(mut inner: Vec<Block>) -> Block {
    let Some(Block::Paragraph(first)) = inner.first_mut() else {
        return Block::Quote(inner);
    };
    let text = first.text.trim_start();
    let Some(rest) = text.strip_prefix("[!") else {
        return Block::Quote(inner);
    };
    let Some(close) = rest.find(']') else {
        return Block::Quote(inner);
    };
    let kind = rest[..close].trim().to_lowercase();
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Block::Quote(inner);
    }
    // Title: the remainder of the marker's own line; everything after
    // the first (recorded) line break stays as content.
    let marker_offset = first.text.len() - text.len(); // leading trim
    let after_start = marker_offset + 2 + close + 1;
    let (title, remainder) = match first.first_break {
        Some(break_at) if break_at > after_start => (
            first.text[after_start..break_at].trim().to_owned(),
            first.text[break_at..].trim_start().to_owned(),
        ),
        Some(break_at) => (
            String::new(),
            first.text[break_at..].trim_start().to_owned(),
        ),
        None => (first.text[after_start..].trim().to_owned(), String::new()),
    };
    let title = if title.is_empty() {
        let mut t = kind.clone();
        if let Some(first) = t.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        t
    } else {
        title
    };

    if remainder.is_empty() {
        inner.remove(0);
    } else {
        // Re-parse the remainder so its own emphasis and links survive.
        let reparsed = parse_blocks(&remainder);
        inner.splice(0..1, reparsed);
    }
    Block::Callout {
        kind,
        title,
        children: inner,
    }
}

/// Flatten one block's inline events into text plus styled and linked spans.
fn collect_inline(parser: &mut Parser, until: TagEnd) -> Inline {
    let mut inline = Inline::default();
    while let Some(event) = parser.next() {
        if matches!(&event, Event::End(e) if *e == until) {
            break;
        }
        append_inline_event(&mut inline, event, parser);
    }
    inline
}

/// The shared inline state machine. Styles nest via a depth-counted state.
fn append_inline_event(inline: &mut Inline, event: Event, parser: &mut Parser) {
    // Nested emphasis state is tracked on the Inline itself via spans-in-
    // progress: simplest correct structure is a running style and explicit
    // push/pop on start/end events, recursing only for links.
    thread_local! {
        static STATE: std::cell::RefCell<SpanStyle> =
            std::cell::RefCell::new(SpanStyle::default());
    }
    fn with_state<R>(f: impl FnOnce(&mut SpanStyle) -> R) -> R {
        STATE.with(|s| f(&mut s.borrow_mut()))
    }

    match event {
        Event::Text(t) => {
            if !t.trim().is_empty() {
                inline.sole_math = None;
            }
            let start = inline.text.len();
            inline.text.push_str(&t);
            let style = with_state(|s| *s);
            if !style.is_plain() {
                inline.spans.push((start..inline.text.len(), style));
            }
        }
        Event::Code(t) => {
            let start = inline.text.len();
            inline.text.push_str(&t);
            let mut style = with_state(|s| *s);
            style.code = true;
            inline.spans.push((start..inline.text.len(), style));
        }
        Event::InlineMath(t) => {
            // Inline math reads as code until a typesetter exists: the
            // honest presentation is "distinct and verbatim" (MD-010).
            inline.sole_math = None;
            let start = inline.text.len();
            inline.text.push_str(&t);
            let mut style = with_state(|s| *s);
            style.code = true;
            inline.spans.push((start..inline.text.len(), style));
        }
        Event::DisplayMath(t) => {
            // Alone in its paragraph it becomes a math block; mixed with
            // prose it degrades to a code-styled span.
            if inline.text.is_empty() {
                inline.sole_math = Some(t.to_string());
            } else {
                inline.sole_math = None;
            }
            let start = inline.text.len();
            inline.text.push_str(&t);
            let mut style = with_state(|s| *s);
            style.code = true;
            inline.spans.push((start..inline.text.len(), style));
        }
        Event::Start(Tag::Emphasis) => with_state(|s| s.italic = true),
        Event::End(TagEnd::Emphasis) => with_state(|s| s.italic = false),
        Event::Start(Tag::Strong) => with_state(|s| s.bold = true),
        Event::End(TagEnd::Strong) => with_state(|s| s.bold = false),
        Event::Start(Tag::Strikethrough) => with_state(|s| s.strike = true),
        Event::End(TagEnd::Strikethrough) => with_state(|s| s.strike = false),
        Event::Start(Tag::Link { dest_url, .. }) => {
            with_state(|s| s.link = true);
            let start = inline.text.len();
            // Collect the link's inner inlines.
            while let Some(event) = parser.next() {
                if matches!(&event, Event::End(TagEnd::Link)) {
                    break;
                }
                append_inline_event(inline, event, parser);
            }
            with_state(|s| s.link = false);
            let range = start..inline.text.len();
            if !range.is_empty() {
                // Only the click target. The *style* spans were pushed by
                // the inner text events, which carried `link` through the
                // running state — a second span over the whole range here
                // would overlap them, and `StyledText` turns overlapping
                // highlight ranges into runs longer than the text, which
                // aborts the process (learned from a live ⌘E crash).
                inline.links.push((range, dest_url.to_string()));
            }
        }
        Event::Start(Tag::Image { dest_url, .. }) => {
            // Record as a pseudo-link so `sole_image` can promote it; the
            // alt text is the content.
            let start = inline.text.len();
            for event in parser.by_ref() {
                if matches!(&event, Event::End(TagEnd::Image)) {
                    break;
                }
                if let Event::Text(t) = event {
                    inline.text.push_str(&t);
                }
            }
            if inline.text.len() == start {
                inline.text.push_str("(image)");
            }
            inline
                .links
                .push((start..inline.text.len(), format!("img:{dest_url}")));
        }
        Event::SoftBreak => {
            if inline.first_break.is_none() {
                inline.first_break = Some(inline.text.len());
            }
            inline.text.push(' ');
        }
        Event::HardBreak => {
            if inline.first_break.is_none() {
                inline.first_break = Some(inline.text.len());
            }
            inline.text.push('\n');
        }
        Event::FootnoteReference(name) => {
            let start = inline.text.len();
            inline.text.push_str(&format!("[{name}]"));
            inline.spans.push((
                start..inline.text.len(),
                SpanStyle {
                    code: true,
                    ..Default::default()
                },
            ));
        }
        _ => {}
    }
}

/// Make a set of highlight ranges safe for `StyledText`, whose run
/// arithmetic ABORTS the process on anything else: ranges must be sorted,
/// non-overlapping, inside the text, and on character boundaries. The
/// parser above produces exactly that by construction — this function is
/// the guarantee that no future parse shape can crash the reading pane,
/// only mis-colour it.
fn sanitize_highlights(
    text: &str,
    mut highlights: Vec<(Range<usize>, HighlightStyle)>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    highlights.sort_by_key(|(range, _)| (range.start, range.end));
    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::with_capacity(highlights.len());
    let mut cursor = 0usize;
    for (range, style) in highlights {
        let mut start = range.start.max(cursor).min(text.len());
        let mut end = range.end.min(text.len());
        while start < text.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        if start >= end {
            continue;
        }
        out.push((start..end, style));
        cursor = end;
    }
    out
}

/// If the inline is exactly one image, its range and destination.
fn sole_image(inline: &Inline) -> Option<(Range<usize>, String)> {
    let (range, dest) = inline.links.first()?;
    let dest = dest.strip_prefix("img:")?;
    (inline.links.len() == 1 && inline.text.trim().len() == inline.text[range.clone()].trim().len())
        .then(|| (range.clone(), dest.to_owned()))
}

/// Decides whether a wiki target resolves, and where attachments live.
pub trait PreviewContext {
    fn wiki_resolves(&self, target: &str) -> bool;
    /// Absolute path for a relative image/attachment destination.
    fn attachment_path(&self, destination: &str) -> Option<PathBuf>;
}

/// What the reading view tells the application.
#[derive(Debug, Clone)]
pub enum PreviewEvent {
    /// A wiki link was clicked; open this target.
    OpenWiki(String),
    /// An ordinary relative link to another note was clicked.
    OpenRelative(String),
}

/// The reading view.
pub struct Preview {
    theme: Theme,
    blocks: Vec<Block>,
    context: Box<dyn PreviewContext>,
    /// Font scale (⌃⇧+/−), 1.0 = the design sizes.
    zoom: f32,
    /// The reading pane's scroll position, held so the keyboard can
    /// drive it (the wheel goes through gpui's own handling).
    scroll: ScrollHandle,
    /// Focus for reading mode, so the paging keys land here.
    focus: FocusHandle,
}

impl EventEmitter<PreviewEvent> for Preview {}

impl Preview {
    pub fn new(theme: Theme, context: Box<dyn PreviewContext>, cx: &mut Context<Self>) -> Self {
        Self {
            theme,
            blocks: Vec::new(),
            context,
            zoom: 1.0,
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Jump back to the top — a different document must not inherit the
    /// last one's scroll position.
    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// Scroll by a fraction of the viewport: +1.0 is a page down,
    /// -0.5 half a page up. Offsets grow negative as content moves up.
    pub fn scroll_pages(&mut self, pages: f32, cx: &mut Context<Self>) {
        let viewport = self.scroll.bounds().size.height;
        let mut offset = self.scroll.offset();
        offset.y -= viewport * pages;
        self.scroll.set_offset(offset);
        cx.notify();
    }

    /// Straight to the end of the document.
    pub fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        let mut offset = self.scroll.offset();
        offset.y = -self.scroll.max_offset().height;
        self.scroll.set_offset(offset);
        cx.notify();
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    pub fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.zoom = zoom;
        cx.notify();
    }

    /// Re-render from source text (the editor buffer, so unsaved edits show).
    pub fn set_source(&mut self, source: &str, cx: &mut Context<Self>) {
        let split = marius_markdown::frontmatter::split(source);
        // Frontmatter is not prose; the properties strip already presents it.
        self.blocks = parse_blocks(&wikify(split.body));
        cx.notify();
    }

    fn highlight_for(&self, style: SpanStyle) -> HighlightStyle {
        let theme = &self.theme;
        let mut highlight = HighlightStyle::default();
        if style.bold {
            highlight.font_weight = Some(FontWeight::BOLD);
        }
        if style.italic {
            highlight.font_style = Some(FontStyle::Italic);
        }
        if style.strike {
            highlight.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(theme.muted.into()),
            });
            highlight.color = Some(theme.muted.into());
        }
        if style.code {
            highlight.color = Some(theme.code_text.into());
            highlight.background_color = Some(theme.code_background.into());
        }
        if style.link {
            highlight.color = Some(if style.broken {
                theme.broken_link.into()
            } else {
                theme.link.into()
            });
            highlight.underline = Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(
                    if style.broken {
                        theme.broken_link
                    } else {
                        theme.link
                    }
                    .into(),
                ),
                wavy: style.broken,
            });
        }
        highlight
    }

    /// The explicit base style every inline run starts from.
    ///
    /// `StyledText::with_default_highlights` does **not** inherit from the
    /// surrounding div — it lays out with exactly the style it is given, and
    /// `TextStyle::default()`'s colour is black. In dark mode that renders
    /// body text invisible, which is precisely the bug a user found within
    /// minutes of trying reading mode. Everything visible must come from the
    /// theme, explicitly.
    fn base_style(
        &self,
        size: gpui::Pixels,
        weight: FontWeight,
        colour: gpui::Rgba,
    ) -> gpui::TextStyle {
        gpui::TextStyle {
            color: colour.into(),
            font_size: size.into(),
            font_weight: weight,
            ..gpui::TextStyle::default()
        }
    }

    fn render_inline(
        &self,
        id: ElementId,
        inline: &Inline,
        base: gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for (range, style) in &inline.spans {
            let mut style = *style;
            if style.link {
                // The span may be a styled fragment inside the link, so
                // match by containment rather than equality.
                if let Some((_, dest)) = inline.links.iter().find(|(link_range, _)| {
                    link_range.start <= range.start && range.end <= link_range.end
                }) {
                    if let Some(target) = dest.strip_prefix(WIKI_SCHEME) {
                        style.broken = !self.context.wiki_resolves(target);
                    }
                }
            }
            highlights.push((range.clone(), self.highlight_for(style)));
        }
        let highlights = sanitize_highlights(&inline.text, highlights);

        let click_targets: Vec<(Range<usize>, String)> = inline
            .links
            .iter()
            .filter(|(_, dest)| !dest.starts_with("img:"))
            .cloned()
            .collect();
        let ranges: Vec<Range<usize>> = click_targets
            .iter()
            .map(|(range, _)| range.clone())
            .collect();
        let destinations: Vec<String> = click_targets.into_iter().map(|(_, dest)| dest).collect();

        let styled = StyledText::new(SharedString::from(inline.text.clone()))
            .with_default_highlights(&base, highlights);
        let entity = cx.entity().clone();
        InteractiveText::new(id, styled)
            .on_click(ranges, move |ix, _window, cx| {
                let Some(destination) = destinations.get(ix) else {
                    return;
                };
                entity.update(cx, |_, cx| {
                    if let Some(target) = destination.strip_prefix(WIKI_SCHEME) {
                        cx.emit(PreviewEvent::OpenWiki(target.to_owned()));
                    } else if !destination.contains("://") && !destination.starts_with("mailto:") {
                        cx.emit(PreviewEvent::OpenRelative(destination.clone()));
                    }
                    // True external URLs: nothing yet; opening a browser from
                    // a reading pane should be a deliberate, visible feature.
                });
            })
            .into_any_element()
    }

    fn render_block(
        &self,
        index: &mut usize,
        block: &Block,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        *index += 1;
        let id = *index;
        let theme = self.theme;
        let z = self.zoom;
        match block {
            Block::Heading { level, inline } => {
                let (size, weight) = match level {
                    1 => (px(26.0 * z), FontWeight::BOLD),
                    2 => (px(21.0 * z), FontWeight::BOLD),
                    3 => (px(17.0 * z), FontWeight::BOLD),
                    _ => (px(15.0 * z), FontWeight::SEMIBOLD),
                };
                let base = self.base_style(size, weight, theme.text);
                div()
                    .pt(px(10.))
                    .pb(px(2.))
                    .child(self.render_inline(("h", id).into(), inline, base, cx))
                    .into_any_element()
            }
            Block::Paragraph(inline) => {
                let base = self.base_style(px(15.0 * z), FontWeight::NORMAL, theme.text);
                div()
                    .py(px(4.))
                    .child(self.render_inline(("p", id).into(), inline, base, cx))
                    .into_any_element()
            }
            Block::Code { lang, text } => {
                // Code does not reflow — a wrapped line changes meaning —
                // so long lines scroll sideways inside the block instead
                // of being clipped.
                let mut code = div()
                    .id(("code", id))
                    .my(px(6.))
                    .p(px(10.))
                    .rounded(px(6.))
                    .bg(theme.code_background)
                    .text_color(theme.code_text)
                    .font_family(mono_family())
                    .text_size(px(12.5 * z))
                    .overflow_x_scroll();
                if let Some(lang) = lang {
                    code = code.child(
                        div()
                            .text_size(px(10.0 * z))
                            .text_color(theme.muted)
                            .child(lang.clone()),
                    );
                }
                code.child(div().whitespace_nowrap().child(text.clone()))
                    .into_any_element()
            }
            Block::Quote(blocks) => {
                let mut quote = div()
                    .my(px(4.))
                    .pl(px(12.))
                    .border_l_2()
                    .border_color(theme.accent)
                    .text_color(theme.muted);
                for inner in blocks {
                    quote = quote.child(self.render_block(index, inner, cx));
                }
                quote.into_any_element()
            }
            Block::List { start, items } => {
                let mut list = div().py(px(2.));
                for (n, item) in items.iter().enumerate() {
                    let marker: SharedString = match (item.task, start) {
                        (Some(true), _) => "☑".into(),
                        (Some(false), _) => "☐".into(),
                        (None, Some(first)) => format!("{}.", *first + n as u64).into(),
                        (None, None) => "•".into(),
                    };
                    let mut content = div().flex_grow().min_w(px(0.));
                    for inner in &item.blocks {
                        content = content.child(self.render_block(index, inner, cx));
                    }
                    list = list.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(24.))
                                    .text_color(if item.task == Some(true) {
                                        theme.tag_manual
                                    } else {
                                        theme.muted
                                    })
                                    .child(marker),
                            )
                            .child(content),
                    );
                }
                list.into_any_element()
            }
            Block::Table { head, rows } => {
                let cell = |inline: &Inline,
                            header: bool,
                            index: &mut usize,
                            cx: &mut Context<Self>,
                            this: &Self| {
                    *index += 1;
                    let weight = if header {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    };
                    let base = this.base_style(px(13.5 * this.zoom), weight, this.theme.text);
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .px(px(8.))
                        .py(px(4.))
                        .child(this.render_inline(("c", *index).into(), inline, base, cx))
                };
                let mut table = div()
                    .my(px(6.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(theme.border);
                if !head.is_empty() {
                    let mut row = div().flex().flex_row().bg(theme.surface);
                    for inline in head {
                        row = row.child(cell(inline, true, index, cx, self));
                    }
                    table = table.child(row);
                }
                for (n, cells) in rows.iter().enumerate() {
                    let mut row = div()
                        .flex()
                        .flex_row()
                        .when(n % 2 == 1, |d| d.bg(theme.surface));
                    for inline in cells {
                        row = row.child(cell(inline, false, index, cx, self));
                    }
                    table = table.child(row);
                }
                table.into_any_element()
            }
            Block::Rule => div()
                .my(px(10.))
                .h(px(1.))
                .bg(theme.border)
                .into_any_element(),
            Block::Callout {
                kind,
                title,
                children,
            } => {
                // Kind → accent colour, mapped onto the theme's own
                // palette so both appearances stay WCAG-checked.
                let (icon, colour) = match kind.as_str() {
                    "warning" | "caution" | "attention" => ("⚠", theme.tag_proposed),
                    "danger" | "error" | "bug" | "failure" => ("✖", theme.broken_link),
                    "tip" | "success" | "check" | "done" => ("✔", theme.tag_confirmed),
                    "question" | "help" | "faq" => ("?", theme.tag_proposed),
                    "quote" | "cite" => ("❝", theme.muted),
                    _ => ("ℹ", theme.accent), // note, info, abstract, …
                };
                let mut callout = div()
                    .my(px(6.))
                    .pl(px(12.))
                    .pr(px(10.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .border_l_2()
                    .border_color(colour)
                    .bg(theme.elevated)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colour)
                            .child(icon)
                            .child(title.clone()),
                    );
                for inner in children {
                    callout = callout.child(self.render_block(index, inner, cx));
                }
                callout.into_any_element()
            }
            Block::Math(source) => div()
                .id(("math", id))
                .my(px(6.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(6.))
                .bg(theme.code_background)
                .border_l_2()
                .border_color(theme.accent)
                .overflow_x_scroll()
                .child(
                    div()
                        .text_size(px(10.0 * z))
                        .text_color(theme.muted)
                        .child("math — not yet typeset"),
                )
                .child(
                    div()
                        .font_family(mono_family())
                        .text_size(px(13.0 * z))
                        .whitespace_nowrap()
                        .child(source.clone()),
                )
                .into_any_element(),
            Block::Image { destination, alt } => {
                let clean = destination
                    .strip_prefix(EMBED_SCHEME)
                    .unwrap_or(destination);
                match self.context.attachment_path(clean) {
                    Some(path) if path.exists() => div()
                        .my(px(8.))
                        .child(img(path).max_w(px(680.)).rounded(px(4.)))
                        .into_any_element(),
                    _ => div()
                        .my(px(8.))
                        .p(px(8.))
                        .rounded(px(4.))
                        .bg(theme.surface)
                        .text_color(theme.muted)
                        .text_size(px(12.))
                        .child(format!("missing image: {alt} ({clean})"))
                        .into_any_element(),
                }
            }
        }
    }
}

fn mono_family() -> &'static str {
    if cfg!(target_os = "windows") {
        "Consolas"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "monospace"
    }
}

impl Render for Preview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let blocks = self.blocks.clone();
        let mut index = 0usize;
        // `w_full` + `max_w`: fill the pane up to the measure, *shrink*
        // below it. Without the fill, the column's intrinsic (unwrapped)
        // width hits the cap and `justify_center` clips both edges in any
        // pane narrower than the cap — exactly what the split view exposed.
        let mut column = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(740.0 * self.zoom))
            .pb(px(60.));
        for block in &blocks {
            column = column.child(self.render_block(&mut index, block, cx));
        }
        div()
            .id("preview-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .track_focus(&self.focus)
            .key_context("ReadingPane")
            .on_action(cx.listener(|this, _: &PageDown, _w, cx| this.scroll_pages(0.9, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _w, cx| this.scroll_pages(-0.9, cx)))
            .on_action(cx.listener(|this, _: &LineDown, _w, cx| this.scroll_pages(0.1, cx)))
            .on_action(cx.listener(|this, _: &LineUp, _w, cx| this.scroll_pages(-0.1, cx)))
            .on_action(cx.listener(|this, _: &GoTop, _w, cx| this.scroll_to_top(cx)))
            .on_action(cx.listener(|this, _: &GoBottom, _w, cx| this.scroll_to_end(cx)))
            .bg(theme.background)
            .text_color(theme.text)
            .text_size(px(15.0 * self.zoom))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .min_w(px(0.))
                    .px(px(28.))
                    .pt(px(18.))
                    .child(column),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // ------------------------------------------------------------- wikify

    #[test]
    fn a_colossal_pdf_paragraph_is_split_into_scrollable_pieces() {
        // The shape an old PDF import left behind: one paragraph the
        // size of the whole document.
        let body = "word ".repeat(4_000);
        let blocks = parse_blocks(&body);
        assert!(blocks.len() > 5, "{} block(s)", blocks.len());
        let total: usize = blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph(i) => i.text.split_whitespace().count(),
                _ => 0,
            })
            .sum();
        assert_eq!(total, 4_000, "no words lost in the split");
        // A styled paragraph is never split: ranges must stay valid.
        let styled = format!("**bold** {}", "word ".repeat(2_000));
        assert!(parse_blocks(&styled).len() < 3);
    }

    #[test]
    fn wiki_links_become_schemed_links_with_display_text() {
        assert_eq!(
            wikify("See [[Pharos]] and [[research/Note|the note]]."),
            "See [Pharos](<marius:Pharos>) and [the note](<marius:research/Note>)."
        );
    }

    #[test]
    fn embeds_become_schemed_images() {
        assert_eq!(
            wikify("![[plan.png]]"),
            "![plan.png](<marius-embed:plan.png>)"
        );
    }

    #[test]
    fn wiki_syntax_inside_code_is_left_alone() {
        let source = "`[[not a link]]` and\n\n```\n[[nor this]]\n```\n";
        assert_eq!(wikify(source), source);
    }

    #[test]
    fn heading_fragments_keep_their_target() {
        assert_eq!(
            wikify("[[Note#Section]]"),
            "[Note](<marius:Note>)",
            "the fragment names the doc; section scrolling is not built"
        );
    }

    // ------------------------------------------------------------- blocks

    fn blocks(source: &str) -> Vec<Block> {
        parse_blocks(&wikify(source))
    }

    #[test]
    fn headings_paragraphs_and_rules_parse() {
        let parsed = blocks("# Title\n\nBody text.\n\n---\n");
        assert!(
            matches!(&parsed[0], Block::Heading { level: 1, inline } if inline.text == "Title")
        );
        assert!(matches!(&parsed[1], Block::Paragraph(p) if p.text == "Body text."));
        assert!(matches!(parsed[2], Block::Rule));
    }

    #[test]
    fn emphasis_nests_and_spans_carry_the_combined_style() {
        let parsed = blocks("plain *em **both** em*\n");
        let Block::Paragraph(inline) = &parsed[0] else {
            panic!("{parsed:?}")
        };
        assert_eq!(inline.text, "plain em both em");
        let both = inline
            .spans
            .iter()
            .find(|(range, _)| &inline.text[range.clone()] == "both")
            .expect("span for 'both'");
        assert!(both.1.bold && both.1.italic);
        let em = inline
            .spans
            .iter()
            .find(|(range, _)| &inline.text[range.clone()] == "em ")
            .expect("span for 'em '");
        assert!(em.1.italic && !em.1.bold);
    }

    #[test]
    fn links_carry_targets_and_wiki_links_keep_their_scheme() {
        let parsed = blocks("See [[Pharos]] and [docs](notes/doc.md) and <https://x.y>.\n");
        let Block::Paragraph(inline) = &parsed[0] else {
            panic!()
        };
        let targets: Vec<&str> = inline.links.iter().map(|(_, d)| d.as_str()).collect();
        assert_eq!(
            targets,
            vec!["marius:Pharos", "notes/doc.md", "https://x.y"]
        );
        // Every link range is also a styled span.
        for (range, _) in &inline.links {
            assert!(inline.spans.iter().any(|(r, s)| r == range && s.link));
        }
    }

    #[test]
    fn code_blocks_keep_language_and_literal_text() {
        let parsed = blocks("```rust\nlet x = 1;\n```\n");
        assert_eq!(
            parsed[0],
            Block::Code {
                lang: Some("rust".into()),
                text: "let x = 1;".into()
            }
        );
    }

    #[test]
    fn quotes_nest_blocks() {
        let parsed = blocks("> quoted *text*\n>\n> second\n");
        let Block::Quote(inner) = &parsed[0] else {
            panic!("{parsed:?}")
        };
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn lists_keep_numbering_tasks_and_nesting() {
        let parsed = blocks("1. one\n2. two\n\n- [x] done\n- [ ] todo\n  - nested\n");
        let Block::List {
            start: Some(1),
            items,
        } = &parsed[0]
        else {
            panic!("{parsed:?}")
        };
        assert_eq!(items.len(), 2);

        let Block::List { start: None, items } = &parsed[1] else {
            panic!("{parsed:?}")
        };
        assert_eq!(items[0].task, Some(true));
        assert_eq!(items[1].task, Some(false));
        assert!(items[1]
            .blocks
            .iter()
            .any(|b| matches!(b, Block::List { .. })));
    }

    #[test]
    fn tables_parse_head_and_rows() {
        let parsed = blocks("| Name | Year |\n|---|---|\n| Pharos | 280 BC |\n");
        let Block::Table { head, rows } = &parsed[0] else {
            panic!("{parsed:?}")
        };
        assert_eq!(head.len(), 2);
        assert_eq!(head[0].text, "Name");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1].text, "280 BC");
    }

    #[test]
    fn a_lone_image_becomes_a_figure_block() {
        let parsed = blocks("![plan](images/plan.png)\n");
        assert!(
            matches!(&parsed[0], Block::Image { destination, .. } if destination == "images/plan.png"),
            "{parsed:?}"
        );
        // An embed too.
        let parsed = blocks("![[plan.png]]\n");
        assert!(matches!(&parsed[0], Block::Image { destination, .. }
            if destination == "marius-embed:plan.png"));
    }

    #[test]
    fn an_image_amid_text_stays_inline_as_its_alt() {
        let parsed = blocks("before ![alt](x.png) after\n");
        let Block::Paragraph(inline) = &parsed[0] else {
            panic!()
        };
        assert_eq!(inline.text, "before alt after");
    }

    #[test]
    fn soft_breaks_join_with_spaces_and_hard_breaks_stay() {
        let parsed = blocks("line one\nline two\n");
        let Block::Paragraph(inline) = &parsed[0] else {
            panic!()
        };
        assert_eq!(inline.text, "line one line two");
    }

    #[test]
    fn an_empty_document_renders_no_blocks() {
        assert!(blocks("").is_empty());
        assert!(blocks("\n\n  \n").is_empty());
    }

    #[test]
    fn span_ranges_are_valid_and_within_bounds() {
        // The renderer slices these; a bad range would panic mid-frame.
        let source = "# H *i*\n\n> q **b** `c` [[W]] [l](t.md) ~~s~~\n\n- [x] item *em*\n";
        fn check(blocks: &[Block]) {
            for block in blocks {
                match block {
                    Block::Heading { inline, .. } | Block::Paragraph(inline) => {
                        let ranges = inline
                            .spans
                            .iter()
                            .map(|(r, _)| r.clone())
                            .chain(inline.links.iter().map(|(r, _)| r.clone()));
                        for range in ranges {
                            assert!(range.end <= inline.text.len(), "{range:?} in {inline:?}");
                            assert!(inline.text.is_char_boundary(range.start));
                            assert!(inline.text.is_char_boundary(range.end));
                        }
                    }
                    Block::Quote(inner) => check(inner),
                    Block::List { items, .. } => {
                        for item in items {
                            check(&item.blocks);
                        }
                    }
                    _ => {}
                }
            }
        }
        check(&blocks(source));
    }
}

#[cfg(test)]
mod crash_regressions {
    use super::*;

    /// The invariant `StyledText` aborts on: runs must tile the text.
    fn assert_safe(inline: &Inline) {
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = inline
            .spans
            .iter()
            .map(|(range, _)| (range.clone(), HighlightStyle::default()))
            .collect();
        // What render_inline hands to gpui after sanitising:
        highlights = sanitize_highlights(&inline.text, highlights);
        let mut cursor = 0usize;
        for (range, _) in &highlights {
            assert!(
                range.start >= cursor,
                "overlapping spans in {:?}: {:?}",
                inline.text,
                inline.spans
            );
            assert!(range.end <= inline.text.len());
            assert!(inline.text.is_char_boundary(range.start));
            assert!(inline.text.is_char_boundary(range.end));
            cursor = range.end;
        }
    }

    fn inlines_of(markdown: &str) -> Vec<Inline> {
        fn walk(blocks: &[Block], out: &mut Vec<Inline>) {
            for block in blocks {
                match block {
                    Block::Heading { inline, .. } | Block::Paragraph(inline) => {
                        out.push(inline.clone())
                    }
                    Block::Quote(children) => walk(children, out),
                    Block::List { items, .. } => {
                        for item in items {
                            walk(&item.blocks, out);
                        }
                    }
                    Block::Table { head, rows } => {
                        out.extend(head.iter().cloned());
                        for row in rows {
                            out.extend(row.iter().cloned());
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        // The same pipeline render uses: wiki links become real links first.
        walk(&parse_blocks(&wikify(markdown)), &mut out);
        out
    }

    #[test]
    fn a_plain_link_produces_one_span_not_two() {
        // The ⌘E abort: inner link text pushed a span AND the link wrapper
        // pushed an identical one; StyledText's runs then summed past the
        // text and the process died with "invalid text run".
        let inlines = inlines_of("See [[alexandria]] and the [survey](s.md).");
        for inline in &inlines {
            assert_safe(inline);
        }
        let inline = &inlines[0];
        assert_eq!(inline.links.len(), 2);
        let link_spans = inline.spans.iter().filter(|(_, s)| s.link).count();
        assert_eq!(
            link_spans, 2,
            "one span per link\nspans: {:?}\nlinks: {:?}\ntext: {:?}",
            inline.spans, inline.links, inline.text
        );
    }

    #[test]
    fn styled_content_inside_links_stays_non_overlapping() {
        for markdown in [
            "*[[emphasised link]]*",
            "[**bold** middle *italic*](dest.md)",
            "[`code in a link`](dest.md)",
            "**[[bold wiki]]** and ~~[gone](x.md)~~",
            "[outer **inner [[deep]]** tail](x.md)",
        ] {
            for inline in inlines_of(markdown) {
                assert_safe(&inline);
            }
        }
    }

    #[test]
    fn the_sanitizer_defuses_hostile_ranges() {
        // Belt and braces: even ranges the parser should never produce
        // must come out orderly rather than aborting the app.
        let text = "héllo wörld";
        let style = HighlightStyle::default();
        let hostile = vec![
            (3..9, style), // starts inside 'é'
            (0..usize::MAX, style),
            (5..5, style),  // empty
            (2..4, style),  // overlaps the first after sorting
            (8..30, style), // runs past the end
        ];
        let clean = sanitize_highlights(text, hostile);
        let mut cursor = 0;
        for (range, _) in &clean {
            assert!(range.start >= cursor && range.end <= text.len());
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
            cursor = range.end;
        }
    }
}

#[cfg(test)]
mod callout_tests {
    use super::*;

    #[test]
    fn an_obsidian_callout_becomes_a_typed_block_with_title() {
        let blocks =
            parse_blocks("> [!warning] Shoals ahead\n> The eastern channel silted up in 1869.\n");
        let [Block::Callout {
            kind,
            title,
            children,
        }] = blocks.as_slice()
        else {
            panic!("expected a callout, got {blocks:?}");
        };
        assert_eq!(kind, "warning");
        assert_eq!(title, "Shoals ahead");
        let [Block::Paragraph(body)] = children.as_slice() else {
            panic!("body paragraph expected: {children:?}");
        };
        assert!(body.text.contains("eastern channel"));
    }

    #[test]
    fn a_titleless_callout_titles_itself_and_plain_quotes_stay_quotes() {
        let blocks = parse_blocks("> [!tip]\n> Sleep before deciding.\n");
        let [Block::Callout { kind, title, .. }] = blocks.as_slice() else {
            panic!("{blocks:?}");
        };
        assert_eq!(kind, "tip");
        assert_eq!(title, "Tip");

        let blocks = parse_blocks("> An ordinary quotation.\n");
        assert!(matches!(blocks.as_slice(), [Block::Quote(_)]), "{blocks:?}");
        // A bracketed non-marker stays a quote too.
        let blocks = parse_blocks("> [!not a marker] because of the space\n");
        assert!(matches!(blocks.as_slice(), [Block::Quote(_)]), "{blocks:?}");
    }

    #[test]
    fn display_and_inline_math_parse_distinctly() {
        let blocks = parse_blocks("Euler: $e^{i\\pi}+1=0$ in text.\n\n$$\\int_0^1 x\\,dx$$\n");
        assert!(matches!(blocks.last(), Some(Block::Math(m)) if m.contains("int_0^1")));
        let [Block::Paragraph(inline), ..] = blocks.as_slice() else {
            panic!("{blocks:?}");
        };
        assert!(inline.text.contains("e^{i\\pi}+1=0"));
        assert!(
            inline
                .spans
                .iter()
                .any(|(range, style)| style.code && inline.text[range.clone()].contains("e^")),
            "inline math styled as code: {:?}",
            inline.spans
        );
    }
}
