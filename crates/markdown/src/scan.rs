//! A span-accurate scan of a Markdown body for links, tags and headings.
//!
//! Requirements: MD-006 (`[[Wiki Links]]`, aliases, heading links, embeds),
//! MD-007 (preserve link integrity across renames and moves), MD-002, MD-010,
//! TAG-004 (inline `#tags`), GRAPH-002, AC-003.
//!
//! Why this is hand-written rather than delegated to the CommonMark parser:
//! MD-007 requires *rewriting* a link inside the user's own file when its
//! target is renamed. That is only safe with the exact byte range of the target
//! text, which an event-based parser does not expose — and `[[wiki links]]`
//! are not CommonMark at all, so a parser sees them as ordinary text that it is
//! free to split across events. `marius_markdown::text` still uses
//! pulldown-cmark for what it is good at: rendering prose for the index.
//!
//! The scan proceeds in two stages. First a *mask* marks every byte that is
//! code, math or an HTML comment, because a `#tag` or `[[link]]` inside a
//! fenced block is sample text, not a reference. Then extraction runs only over
//! unmasked bytes.

use std::ops::Range;

/// Everything one pass over a body found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// `[[wiki links]]` and `![[embeds]]`, in document order.
    pub wiki_links: Vec<WikiLink>,
    /// Inline and reference-style Markdown links and images, in document order.
    pub markdown_links: Vec<MarkdownLink>,
    /// Inline `#tags`, in document order, with duplicates retained so the
    /// caller can report every occurrence.
    pub inline_tags: Vec<InlineTag>,
    /// ATX and setext headings, in document order (MD-011 outline).
    pub headings: Vec<Heading>,
}

impl Scan {
    /// Distinct link targets this document points at, for graph building.
    pub fn link_targets(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for link in &self.wiki_links {
            if !link.target.is_empty() && !out.contains(&link.target.as_str()) {
                out.push(&link.target);
            }
        }
        for link in &self.markdown_links {
            if !link.is_external() && !out.contains(&link.target.as_str()) {
                out.push(&link.target);
            }
        }
        out
    }

    /// Distinct inline tag names, in first-appearance order.
    pub fn tag_names(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for tag in &self.inline_tags {
            if !out.contains(&tag.name.as_str()) {
                out.push(&tag.name);
            }
        }
        out
    }
}

/// A `[[target#heading|display]]` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// Byte range of the whole construct, including `![[` and `]]`.
    pub span: Range<usize>,
    /// Byte range of just the target text, which is what a rename rewrites
    /// (MD-007). Empty targets (`[[#section]]`) give an empty range.
    pub target_span: Range<usize>,
    /// Target note name, path or alias, with surrounding spaces trimmed.
    pub target: String,
    /// Heading fragment after `#`, if present (MD-006 heading links).
    pub heading: Option<String>,
    /// Block reference after `#^`, if present.
    pub block_ref: Option<String>,
    /// Display text after `|`, if present.
    pub display: Option<String>,
    /// True for `![[…]]`, which embeds the target's content (MD-006).
    pub is_embed: bool,
}

impl WikiLink {
    /// Text the reader sees.
    pub fn display_text(&self) -> &str {
        self.display.as_deref().unwrap_or(&self.target)
    }

    /// True when the link points within the current document.
    pub fn is_internal(&self) -> bool {
        self.target.is_empty()
    }
}

/// A `[text](target)` link or `![alt](target)` image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLink {
    pub span: Range<usize>,
    /// Byte range of the destination, for rewriting on a move (MD-007).
    pub target_span: Range<usize>,
    pub target: String,
    pub text: String,
    /// True for `![…]`, an image or attachment reference (MD-005).
    pub is_image: bool,
    /// Heading or block fragment after `#`.
    pub fragment: Option<String>,
}

impl MarkdownLink {
    /// True when the destination leaves the vault — a URL or a mail address.
    ///
    /// External destinations are never rewritten on rename and never become
    /// graph edges.
    pub fn is_external(&self) -> bool {
        let t = self.target.trim();
        if t.starts_with("//") {
            return true;
        }
        match t.split_once(':') {
            // A scheme is `letter (letter|digit|+|-|.)*`. Checking the shape
            // rather than a list of schemes means `obsidian:` and `zotero:`
            // are recognised too, while a Windows-style `C:/notes` is not
            // mistaken for one.
            Some((scheme, _)) => {
                !scheme.is_empty()
                    && scheme.len() > 1
                    && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            }
            None => false,
        }
    }
}

/// An inline `#tag` occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineTag {
    /// Byte range including the leading `#`.
    pub span: Range<usize>,
    /// Tag text without the `#`, exactly as written. Normalisation to a
    /// canonical form is `marius_model::TagName`'s job, not the scanner's.
    pub name: String,
}

/// An ATX (`## Title`) or setext (`Title\n---`) heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// 1–6.
    pub level: u8,
    /// Heading text with markers and trailing `#`s removed.
    pub text: String,
    /// Byte range of the heading line, or of both setext lines.
    pub span: Range<usize>,
    /// Byte offset where this heading's content begins — just after its span.
    pub content_start: usize,
}

impl Heading {
    /// Normalised key for matching a `[[note#heading]]` fragment.
    ///
    /// Fragments are matched case- and punctuation-insensitively because that
    /// is how people type them, and a link that fails on a stray comma is
    /// worse than one that is slightly too forgiving.
    pub fn fragment_key(&self) -> String {
        fragment_key(&self.text)
    }
}

/// Normalise a heading or fragment for comparison.
pub fn fragment_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// Byte-level record of which parts of the body are plain text.
///
/// `false` means the byte is inside code, math or an HTML comment and must be
/// ignored by every extractor.
struct Mask {
    scannable: Vec<bool>,
}

impl Mask {
    fn new(len: usize) -> Self {
        Self {
            scannable: vec![true; len],
        }
    }

    fn hide(&mut self, range: Range<usize>) {
        let end = range.end.min(self.scannable.len());
        if range.start < end {
            self.scannable[range.start..end].fill(false);
        }
    }

    fn visible(&self, at: usize) -> bool {
        self.scannable.get(at).copied().unwrap_or(false)
    }

    fn visible_range(&self, range: Range<usize>) -> bool {
        range.clone().all(|i| self.visible(i))
    }
}

/// Scan a Markdown body.
///
/// `offset` is added to every reported span, so passing a document's
/// `body_offset` yields spans that address the original file.
pub fn scan(body: &str, offset: usize) -> Scan {
    let mut mask = Mask::new(body.len());
    let mut headings = Vec::new();
    mask_blocks(body, &mut mask, &mut headings);
    mask_inline(body, &mut mask);

    let mut scan = Scan {
        headings,
        ..Scan::default()
    };
    scan.markdown_links = extract_markdown_links(body, &mut mask);
    scan.wiki_links = extract_wiki_links(body, &mut mask);
    scan.inline_tags = extract_inline_tags(body, &mask);

    if offset != 0 {
        shift(&mut scan, offset);
    }
    scan
}

fn shift(scan: &mut Scan, by: usize) {
    let bump = |r: &mut Range<usize>| {
        r.start += by;
        r.end += by;
    };
    for link in &mut scan.wiki_links {
        bump(&mut link.span);
        bump(&mut link.target_span);
    }
    for link in &mut scan.markdown_links {
        bump(&mut link.span);
        bump(&mut link.target_span);
    }
    for tag in &mut scan.inline_tags {
        bump(&mut tag.span);
    }
    for heading in &mut scan.headings {
        bump(&mut heading.span);
        heading.content_start += by;
    }
}

/// Iterate `(start, line_including_newline)`.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    text.split_inclusive('\n').map(move |line| {
        let start = offset;
        offset += line.len();
        (start, line)
    })
}

/// Hide fenced code, `$$` math blocks and HTML comment blocks; collect headings.
fn mask_blocks(body: &str, mask: &mut Mask, headings: &mut Vec<Heading>) {
    // `(fence char, fence length)` while inside a fenced block.
    let mut fence: Option<(u8, usize)> = None;
    let mut in_math_block = false;
    let mut pending_setext: Option<(usize, usize, String)> = None;

    for (start, raw) in lines(body) {
        let line = raw.trim_end_matches(['\n', '\r']);
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        let content = &line[indent..];

        if let Some((ch, len)) = fence {
            if indent <= 3 && closes_fence(content, ch, len) {
                fence = None;
            }
            mask.hide(start..start + raw.len());
            pending_setext = None;
            continue;
        }

        if indent <= 3 {
            if let Some(opened) = opens_fence(content) {
                fence = Some(opened);
                mask.hide(start..start + raw.len());
                pending_setext = None;
                continue;
            }
        }

        if content.trim_end() == "$$" {
            in_math_block = !in_math_block;
            mask.hide(start..start + raw.len());
            pending_setext = None;
            continue;
        }
        if in_math_block {
            mask.hide(start..start + raw.len());
            continue;
        }

        // A setext underline turns the *previous* paragraph line into a heading.
        if let Some((prev_start, prev_len, ref text)) = pending_setext {
            if indent <= 3 && !content.is_empty() {
                let marker = content.trim_end();
                let level = match marker.chars().next() {
                    Some('=') if marker.chars().all(|c| c == '=') => Some(1u8),
                    Some('-') if marker.chars().all(|c| c == '-') => Some(2u8),
                    _ => None,
                };
                if let Some(level) = level {
                    let end = start + raw.len();
                    headings.push(Heading {
                        level,
                        text: text.clone(),
                        span: prev_start..end,
                        content_start: end,
                    });
                    pending_setext = None;
                    let _ = prev_len;
                    continue;
                }
            }
        }
        pending_setext = None;

        if indent <= 3 && content.starts_with('#') {
            let hashes = content.chars().take_while(|c| *c == '#').count();
            let after = &content[hashes..];
            // CommonMark: 1–6 `#` followed by a space or end of line. Anything
            // else — `#tag`, `#######` — is not a heading.
            if (1..=6).contains(&hashes) && (after.is_empty() || after.starts_with([' ', '\t'])) {
                let text = after.trim().trim_end_matches('#').trim_end().to_owned();
                let end = start + raw.len();
                headings.push(Heading {
                    level: hashes as u8,
                    text,
                    span: start..end,
                    content_start: end,
                });
                continue;
            }
        }

        if !content.trim().is_empty() {
            pending_setext = Some((start, raw.len(), content.trim().to_owned()));
        }
    }
}

fn opens_fence(content: &str) -> Option<(u8, usize)> {
    let first = content.as_bytes().first().copied()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let len = content.bytes().take_while(|b| *b == first).count();
    if len < 3 {
        return None;
    }
    // A backtick info string may not itself contain a backtick.
    if first == b'`' && content[len..].contains('`') {
        return None;
    }
    Some((first, len))
}

fn closes_fence(content: &str, ch: u8, open_len: usize) -> bool {
    let len = content.bytes().take_while(|b| *b == ch).count();
    len >= open_len && content[len..].trim().is_empty()
}

/// Hide inline code spans, inline math and HTML comments.
fn mask_inline(body: &str, mask: &mut Mask) {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !mask.visible(i) {
            i += 1;
            continue;
        }
        match bytes[i] {
            b'`' => {
                let run = bytes[i..].iter().take_while(|b| **b == b'`').count();
                match find_backtick_run(bytes, i + run, run) {
                    Some(close) => {
                        mask.hide(i..close + run);
                        i = close + run;
                    }
                    // An unmatched run is literal text; skip just the run so a
                    // stray backtick cannot swallow the rest of the document.
                    None => i += run,
                }
            }
            b'$' => match inline_math_end(body, i) {
                Some(end) => {
                    mask.hide(i..end);
                    i = end;
                }
                None => i += 1,
            },
            b'<' if body[i..].starts_with("<!--") => {
                let end = body[i..]
                    .find("-->")
                    .map_or(body.len(), |rel| i + rel + "-->".len());
                mask.hide(i..end);
                i = end;
            }
            b'<' => {
                // Autolink `<https://example.com/#x>`: hide it so its fragment
                // is not read as a tag.
                if let Some(rel) = body[i..].find('>') {
                    let inner = &body[i + 1..i + rel];
                    if !inner.is_empty()
                        && !inner.contains(char::is_whitespace)
                        && inner.contains(':')
                    {
                        mask.hide(i..i + rel + 1);
                        i += rel + 1;
                        continue;
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
}

fn find_backtick_run(bytes: &[u8], from: usize, run: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let len = bytes[i..].iter().take_while(|b| **b == b'`').count();
            if len == run {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// End offset (exclusive) of an inline math span starting at `start`.
///
/// Follows the usual rule: the opening `$` is not followed by whitespace and the
/// closing `$` is not preceded by whitespace, so prose like "costs $5 and $10"
/// is left alone. A span may not cross a blank line, and is length-capped so a
/// lone `$` cannot mask half a document.
fn inline_math_end(body: &str, start: usize) -> Option<usize> {
    const MAX: usize = 512;
    let bytes = body.as_bytes();
    let double = bytes.get(start + 1) == Some(&b'$');
    let open_len = if double { 2 } else { 1 };
    let first = *bytes.get(start + open_len)?;
    if first.is_ascii_whitespace() {
        return None;
    }
    let mut i = start + open_len;
    let limit = (start + MAX).min(bytes.len());
    while i < limit {
        if bytes[i] == b'\n' && body[i..].trim_start().starts_with('\n') {
            return None;
        }
        if bytes[i] == b'$' {
            let run = bytes[i..].iter().take_while(|b| **b == b'$').count();
            let prev_ok = bytes.get(i - 1).is_some_and(|b| !b.is_ascii_whitespace());
            if run >= open_len && prev_ok {
                return Some(i + open_len);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// Extract `[text](dest)` and `![alt](dest)`, hiding each destination so a URL
/// fragment is never mistaken for a tag.
fn extract_markdown_links(body: &str, mask: &mut Mask) -> Vec<MarkdownLink> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut hide: Vec<Range<usize>> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if !mask.visible(i) || bytes[i] != b'[' {
            i += 1;
            continue;
        }
        // `[[` starts a wiki link, handled separately.
        if bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            continue;
        }
        let is_image = i > 0 && bytes[i - 1] == b'!';
        let Some(text_end) = matching_bracket(bytes, i, b'[', b']') else {
            i += 1;
            continue;
        };
        if bytes.get(text_end + 1) != Some(&b'(') {
            i = text_end + 1;
            continue;
        }
        let Some(dest_end) = matching_bracket(bytes, text_end + 1, b'(', b')') else {
            i = text_end + 1;
            continue;
        };

        let raw_dest = &body[text_end + 2..dest_end];
        let (dest_text, dest_offset) = strip_destination(raw_dest);
        let dest_start = text_end + 2 + dest_offset;
        let (target, fragment) = split_fragment(dest_text);

        let span_start = if is_image { i - 1 } else { i };
        out.push(MarkdownLink {
            span: span_start..dest_end + 1,
            target_span: dest_start..dest_start + target.len(),
            target: decode_destination(target),
            text: body[i + 1..text_end].to_owned(),
            is_image,
            fragment: fragment.map(str::to_owned),
        });
        hide.push(text_end + 1..dest_end + 1);
        i = dest_end + 1;
    }

    // Reference definitions: `[label]: destination`, which also must not leak
    // their fragments into the tag extractor.
    for (start, raw) in lines(body) {
        let line = raw.trim_end_matches(['\n', '\r']);
        let trimmed = line.trim_start();
        if !trimmed.starts_with('[') || !mask.visible(start) {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let Some(close) = trimmed.find("]:") else {
            continue;
        };
        let dest_raw = trimmed[close + 2..].trim_start();
        if dest_raw.is_empty() {
            continue;
        }
        let dest_rel = trimmed.len() - dest_raw.len();
        let dest_text = dest_raw.split_whitespace().next().unwrap_or(dest_raw);
        let (target, fragment) = split_fragment(dest_text);
        let abs = start + indent + dest_rel;
        out.push(MarkdownLink {
            span: start + indent..start + line.len(),
            target_span: abs..abs + target.len(),
            target: decode_destination(target),
            text: trimmed[1..close].to_owned(),
            is_image: false,
            fragment: fragment.map(str::to_owned),
        });
        hide.push(start + indent..start + line.len());
    }

    for range in hide {
        mask.hide(range);
    }
    out.sort_by_key(|l| l.span.start);
    out
}

/// Strip `<…>` wrapping and a trailing `"title"` from a destination, returning
/// the destination text and its offset within the raw slice.
fn strip_destination(raw: &str) -> (&str, usize) {
    let lead = raw.len() - raw.trim_start().len();
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        return (inner, lead + 1);
    }
    // A title is separated by whitespace: `(path "Title")`.
    match trimmed.find(char::is_whitespace) {
        Some(at) => (&trimmed[..at], lead),
        None => (trimmed, lead),
    }
}

fn split_fragment(dest: &str) -> (&str, Option<&str>) {
    match dest.find('#') {
        Some(at) => (&dest[..at], Some(&dest[at + 1..])),
        None => (dest, None),
    }
}

/// Decode the percent-escapes Markdown writers use for spaces in paths.
fn decode_destination(dest: &str) -> String {
    if !dest.contains('%') {
        return dest.to_owned();
    }
    let bytes = dest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&dest[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| dest.to_owned())
}

/// Find the offset of the bracket closing the one at `open`, respecting nesting
/// and backslash escapes.
fn matching_bracket(bytes: &[u8], open: usize, open_ch: u8, close_ch: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            // A link's text or destination cannot span a blank line.
            b'\n' if bytes.get(i + 1) == Some(&b'\n') => return None,
            c if c == open_ch => depth += 1,
            c if c == close_ch => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn extract_wiki_links(body: &str, mask: &mut Mask) -> Vec<WikiLink> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut hide: Vec<Range<usize>> = Vec::new();
    let mut i = 0;

    while i + 3 < bytes.len() {
        if !mask.visible(i) || bytes[i] != b'[' || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let inner_start = i + 2;
        let Some(rel) = body[inner_start..].find("]]") else {
            break;
        };
        let inner_end = inner_start + rel;
        let inner = &body[inner_start..inner_end];
        // A wiki link does not span a blank line, and `]]` must be visible too.
        if inner.contains("\n\n") || !mask.visible(inner_end) {
            i = inner_start;
            continue;
        }

        let is_embed = i > 0 && bytes[i - 1] == b'!' && mask.visible(i - 1);
        let span_start = if is_embed { i - 1 } else { i };

        // Split `target#fragment|display`; `|` binds last so a display text may
        // contain `#`.
        let (before_display, display) = match inner.find('|') {
            Some(at) => (&inner[..at], Some(inner[at + 1..].trim().to_owned())),
            None => (inner, None),
        };
        let (target_raw, heading, block_ref) = match before_display.find('#') {
            Some(at) => {
                let frag = &before_display[at + 1..];
                match frag.strip_prefix('^') {
                    Some(block) => (&before_display[..at], None, Some(block.trim().to_owned())),
                    None => (
                        &before_display[..at],
                        Some(frag.trim().to_owned()).filter(|f| !f.is_empty()),
                        None,
                    ),
                }
            }
            None => (before_display, None, None),
        };

        let lead = target_raw.len() - target_raw.trim_start().len();
        let target = target_raw.trim();
        let target_start = inner_start + lead;

        out.push(WikiLink {
            span: span_start..inner_end + 2,
            target_span: target_start..target_start + target.len(),
            target: target.to_owned(),
            heading,
            block_ref,
            display: display.filter(|d| !d.is_empty()),
            is_embed,
        });
        hide.push(span_start..inner_end + 2);
        i = inner_end + 2;
    }

    for range in hide {
        mask.hide(range);
    }
    out
}

fn extract_inline_tags(body: &str, mask: &Mask) -> Vec<InlineTag> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'#' || !mask.visible(i) {
            i += 1;
            continue;
        }
        if !valid_tag_prefix(body, i) {
            i += 1;
            continue;
        }
        let name_start = i + 1;
        let mut end = name_start;
        for (rel, ch) in body[name_start..].char_indices() {
            if is_tag_char(ch) {
                end = name_start + rel + ch.len_utf8();
            } else {
                break;
            }
        }
        // Trim separators the tag should not end on: `#done.` and `#done/`.
        while end > name_start
            && body[..end]
                .chars()
                .next_back()
                .is_some_and(|c| matches!(c, '-' | '/' | '_'))
        {
            end -= 1;
        }

        let name = &body[name_start..end];
        if is_usable_tag(name) && mask.visible_range(i..end) {
            out.push(InlineTag {
                span: i..end,
                name: name.to_owned(),
            });
            i = end;
        } else {
            i = (name_start).max(i + 1);
        }
    }
    out
}

/// A `#` only starts a tag at the beginning of the text or after a character
/// that cannot be part of a word.
///
/// Excluding `/` and `&` is what keeps `https://example.com/#anchor` and the
/// HTML entity `&#39;` from registering as tags; excluding alphanumerics keeps
/// `issue#42` out.
fn valid_tag_prefix(body: &str, at: usize) -> bool {
    match body[..at].chars().next_back() {
        None => true,
        Some(prev) => {
            !prev.is_alphanumeric() && !matches!(prev, '/' | '&' | '#' | '%' | '=' | '\\' | '~')
        }
    }
}

fn is_tag_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '-' | '_' | '/')
}

/// Reject tags that are empty or purely numeric, so `#1` in "issue #1" and
/// `#2024` in a date are not tags. A tag must carry at least one letter.
fn is_usable_tag(name: &str) -> bool {
    !name.is_empty() && name.chars().any(char::is_alphabetic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn s(body: &str) -> Scan {
        scan(body, 0)
    }

    // ---- wiki links (MD-006) ----

    #[test]
    fn plain_wiki_links_are_found_with_exact_spans() {
        let body = "See [[Alexandria]] today.";
        let scan = s(body);
        assert_eq!(scan.wiki_links.len(), 1);
        let link = &scan.wiki_links[0];
        assert_eq!(link.target, "Alexandria");
        assert_eq!(&body[link.span.clone()], "[[Alexandria]]");
        // MD-007 depends on this span addressing only the target text.
        assert_eq!(&body[link.target_span.clone()], "Alexandria");
        assert!(!link.is_embed);
    }

    #[test]
    fn wiki_link_aliases_headings_and_block_refs_are_decomposed() {
        let scan = s("[[Note|Display]] [[Note#Heading]] [[Note#^abc123]] [[Note#Heading|Shown]]");
        let links = &scan.wiki_links;
        assert_eq!(links.len(), 4);

        assert_eq!(links[0].display.as_deref(), Some("Display"));
        assert_eq!(links[0].display_text(), "Display");

        assert_eq!(links[1].heading.as_deref(), Some("Heading"));
        assert_eq!(links[1].target, "Note");

        assert_eq!(links[2].block_ref.as_deref(), Some("abc123"));
        assert_eq!(links[2].heading, None);

        assert_eq!(links[3].heading.as_deref(), Some("Heading"));
        assert_eq!(links[3].display.as_deref(), Some("Shown"));
    }

    #[test]
    fn embeds_are_distinguished_from_links() {
        let scan = s("![[Diagram.png]] and [[Diagram.png]]");
        assert!(scan.wiki_links[0].is_embed);
        assert!(!scan.wiki_links[1].is_embed);
        assert_eq!(scan.wiki_links[0].target, "Diagram.png");
    }

    #[test]
    fn a_link_to_a_heading_in_this_document_has_no_target() {
        let scan = s("[[#Construction]]");
        assert!(scan.wiki_links[0].is_internal());
        assert_eq!(scan.wiki_links[0].heading.as_deref(), Some("Construction"));
        assert!(scan.wiki_links[0].target_span.is_empty());
    }

    #[test]
    fn paths_and_spaces_in_wiki_targets_are_preserved() {
        let scan = s("[[  research/Lighthouse of Alexandria  |short]]");
        assert_eq!(
            scan.wiki_links[0].target,
            "research/Lighthouse of Alexandria"
        );
        assert_eq!(scan.wiki_links[0].display.as_deref(), Some("short"));
    }

    #[test]
    fn a_display_text_may_contain_a_hash() {
        let scan = s("[[Note|C# notes]]");
        assert_eq!(scan.wiki_links[0].target, "Note");
        assert_eq!(scan.wiki_links[0].display.as_deref(), Some("C# notes"));
    }

    // ---- markdown links (MD-002, MD-005) ----

    #[test]
    fn inline_links_and_images_are_found_with_destination_spans() {
        let body = "![alt](images/pharos.png) and [text](notes/a.md).";
        let scan = s(body);
        assert_eq!(scan.markdown_links.len(), 2);

        let image = &scan.markdown_links[0];
        assert!(image.is_image);
        assert_eq!(image.target, "images/pharos.png");
        assert_eq!(&body[image.span.clone()], "![alt](images/pharos.png)");
        assert_eq!(&body[image.target_span.clone()], "images/pharos.png");

        let link = &scan.markdown_links[1];
        assert!(!link.is_image);
        assert_eq!(link.text, "text");
        assert_eq!(link.target, "notes/a.md");
    }

    #[test]
    fn destinations_with_angle_brackets_titles_and_escapes_are_parsed() {
        let scan = s("[a](<notes/with space.md>) [b](notes/b.md \"Title\") [c](notes/c%20d.md)");
        let targets: Vec<_> = scan
            .markdown_links
            .iter()
            .map(|l| l.target.as_str())
            .collect();
        assert_eq!(
            targets,
            vec!["notes/with space.md", "notes/b.md", "notes/c d.md"]
        );
    }

    #[test]
    fn external_destinations_are_recognised_and_not_rewritable() {
        let scan = s("[a](https://example.com) [b](mailto:x@y.z) [c](notes/a.md) [d](//cdn/x.js) [e](C:/notes/x.md)");
        let external: Vec<bool> = scan
            .markdown_links
            .iter()
            .map(|l| l.is_external())
            .collect();
        assert_eq!(external, vec![true, true, false, true, false]);
    }

    #[test]
    fn link_fragments_are_separated_from_the_path() {
        let scan = s("[a](notes/a.md#construction)");
        assert_eq!(scan.markdown_links[0].target, "notes/a.md");
        assert_eq!(
            scan.markdown_links[0].fragment.as_deref(),
            Some("construction")
        );
    }

    #[test]
    fn reference_definitions_are_captured() {
        let scan = s("Text [ref].\n\n[ref]: notes/target.md#x\n");
        let link = scan
            .markdown_links
            .iter()
            .find(|l| l.text == "ref")
            .unwrap();
        assert_eq!(link.target, "notes/target.md");
        assert_eq!(link.fragment.as_deref(), Some("x"));
    }

    // ---- inline tags (TAG-004) ----

    #[test]
    fn inline_tags_are_found() {
        let body = "A note about #lighthouses and #history/ancient.";
        let scan = s(body);
        assert_eq!(scan.tag_names(), vec!["lighthouses", "history/ancient"]);
        assert_eq!(&body[scan.inline_tags[0].span.clone()], "#lighthouses");
    }

    #[test]
    fn headings_are_not_tags() {
        // The single most common false positive.
        let scan = s("# Title\n\n## Subtitle\n\n###### Deep\n\ntext #real\n");
        assert_eq!(scan.tag_names(), vec!["real"]);
        assert_eq!(scan.headings.len(), 3);
    }

    #[test]
    fn a_tag_at_the_start_of_a_line_is_still_a_tag() {
        let scan = s("#lighthouse at line start\n");
        assert_eq!(scan.tag_names(), vec!["lighthouse"]);
        assert!(scan.headings.is_empty());
    }

    #[test]
    fn url_fragments_are_not_tags() {
        let scan = s("See https://example.com/page#section and [x](http://a.b/#frag).");
        assert!(scan.tag_names().is_empty(), "{:?}", scan.inline_tags);
    }

    #[test]
    fn autolinks_do_not_leak_fragments_as_tags() {
        let scan = s("Visit <https://example.com/#anchor> now.");
        assert!(scan.tag_names().is_empty());
    }

    #[test]
    fn html_entities_are_not_tags() {
        let scan = s("Hello &#39;world&#39; and an em dash &#x2014; here.");
        assert!(scan.tag_names().is_empty(), "{:?}", scan.inline_tags);
    }

    #[test]
    fn a_tag_after_sentence_punctuation_is_still_a_tag() {
        // The counterpart to the entity rule: `;` and `:` must stay legal
        // prefixes, which is why a *double*-escaped entity like `&amp;#x2014;`
        // is knowingly not covered — excluding `;` would cost real tags.
        let scan = s("note this; #important, and this: #urgent (#inline) \"#quoted\"");
        assert_eq!(
            scan.tag_names(),
            vec!["important", "urgent", "inline", "quoted"]
        );
    }

    #[test]
    fn word_internal_hashes_are_not_tags() {
        let scan = s("issue#42 and C#sharp and a=#b");
        assert!(scan.tag_names().is_empty(), "{:?}", scan.inline_tags);
    }

    #[test]
    fn purely_numeric_hashes_are_not_tags() {
        let scan = s("Issue #42 filed in #2024, tagged #v2 and #project.");
        assert_eq!(scan.tag_names(), vec!["v2", "project"]);
    }

    #[test]
    fn tags_do_not_keep_trailing_punctuation_or_separators() {
        let scan = s("ends #sentence. then #nested/ and #snake_ and #dash-");
        assert_eq!(
            scan.tag_names(),
            vec!["sentence", "nested", "snake", "dash"]
        );
    }

    #[test]
    fn tags_inside_code_are_ignored() {
        // TAG-004 must not harvest sample text.
        let body = "Use `#define X` here.\n\n```c\n#include <stdio.h>\n#pragma once\n```\n\nBut #real counts.\n";
        let scan = s(body);
        assert_eq!(scan.tag_names(), vec!["real"]);
    }

    #[test]
    fn tags_inside_tilde_fences_and_nested_fences_are_ignored() {
        let body = "~~~\n#nope\n~~~\n\n````\n```\n#also-nope\n```\n````\n\n#yes\n";
        assert_eq!(s(body).tag_names(), vec!["yes"]);
    }

    #[test]
    fn an_unclosed_fence_does_not_swallow_a_whole_document_silently() {
        // It does mask to the end — that is CommonMark's own rule — but the
        // document before it must still scan.
        let body = "#before\n\n```\n#inside\n";
        assert_eq!(s(body).tag_names(), vec!["before"]);
    }

    #[test]
    fn tags_inside_math_are_ignored() {
        // MD-010.
        let body = "Inline $x_{\\#1}$ and\n\n$$\n\\#block\n$$\n\n#real\n";
        assert_eq!(s(body).tag_names(), vec!["real"]);
    }

    #[test]
    fn currency_amounts_do_not_mask_following_text() {
        // The inline-math rule must not fire on ordinary prose.
        let body = "It costs $5 and $10, see #budget.";
        assert_eq!(s(body).tag_names(), vec!["budget"]);
    }

    #[test]
    fn tags_inside_html_comments_are_ignored() {
        let body = "<!-- #draft\nmultiline #hidden -->\n\n#visible\n";
        assert_eq!(s(body).tag_names(), vec!["visible"]);
    }

    #[test]
    fn a_wiki_link_heading_is_not_harvested_as_a_tag() {
        let scan = s("[[Note#Construction]] and #real");
        assert_eq!(scan.tag_names(), vec!["real"]);
    }

    #[test]
    fn non_latin_tags_are_found() {
        let scan = s("Notes on #Leuchtturm and #灯台 and #φάρος.");
        assert_eq!(scan.tag_names(), vec!["Leuchtturm", "灯台", "φάρος"]);
    }

    #[test]
    fn repeated_tags_are_reported_once_in_tag_names_but_kept_as_occurrences() {
        let scan = s("#a then #a again");
        assert_eq!(scan.inline_tags.len(), 2);
        assert_eq!(scan.tag_names(), vec!["a"]);
    }

    // ---- headings (MD-011) ----

    #[test]
    fn atx_and_setext_headings_are_both_recognised() {
        let body = "Title\n=====\n\n## Second\n\nSub\n---\n\n### Third ###\n";
        let scan = s(body);
        let got: Vec<(u8, &str)> = scan
            .headings
            .iter()
            .map(|h| (h.level, h.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![(1, "Title"), (2, "Second"), (2, "Sub"), (3, "Third")]
        );
    }

    #[test]
    fn headings_inside_code_fences_are_not_headings() {
        let body = "```\n# not a heading\n```\n\n# real\n";
        let scan = s(body);
        assert_eq!(scan.headings.len(), 1);
        assert_eq!(scan.headings[0].text, "real");
    }

    #[test]
    fn seven_hashes_is_not_a_heading() {
        assert!(s("####### too deep\n").headings.is_empty());
    }

    #[test]
    fn heading_spans_and_content_boundaries_are_exact() {
        let body = "# One\nbody text\n";
        let h = &s(body).headings[0];
        assert_eq!(&body[h.span.clone()], "# One\n");
        assert_eq!(&body[h.content_start..], "body text\n");
    }

    #[test]
    fn heading_fragments_match_forgivingly() {
        let h = &s("## Construction, 280 BC\n").headings[0];
        assert_eq!(h.fragment_key(), "construction 280 bc");
        assert_eq!(fragment_key("Construction 280 BC"), h.fragment_key());
        assert_eq!(fragment_key("  construction,  280  bc  "), h.fragment_key());
    }

    // ---- offsets and aggregation ----

    #[test]
    fn an_offset_shifts_every_reported_span() {
        // Spans must address the original file, not the body slice (SRCH-010).
        let body = "#tag [[Link]] [x](y.md)\n# H\n";
        let plain = scan(body, 0);
        let shifted = scan(body, 100);
        assert_eq!(
            shifted.inline_tags[0].span.start,
            plain.inline_tags[0].span.start + 100
        );
        assert_eq!(
            shifted.wiki_links[0].target_span.start,
            plain.wiki_links[0].target_span.start + 100
        );
        assert_eq!(
            shifted.markdown_links[0].span.end,
            plain.markdown_links[0].span.end + 100
        );
        assert_eq!(
            shifted.headings[0].content_start,
            plain.headings[0].content_start + 100
        );
    }

    #[test]
    fn link_targets_are_deduplicated_and_exclude_external_urls() {
        let scan = s("[[A]] [[A]] [[B]] [x](https://e.com) [y](c.md)");
        assert_eq!(scan.link_targets(), vec!["A", "B", "c.md"]);
    }

    #[test]
    fn an_empty_document_scans_to_nothing() {
        let scan = s("");
        assert_eq!(scan, Scan::default());
    }

    #[test]
    fn a_pathological_bracket_soup_terminates_and_finds_what_it_can() {
        let body = "[[[[ ]] ]] [x]( [a](b.md) #t `` ` ``";
        let scan = s(body);
        assert!(scan.markdown_links.iter().any(|l| l.target == "b.md"));
        assert_eq!(scan.tag_names(), vec!["t"]);
    }
}
