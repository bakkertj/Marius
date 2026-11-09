//! YAML frontmatter: parsed for querying, preserved verbatim for writing.
//!
//! Requirements: MD-008 (support YAML frontmatter *without discarding unknown
//! fields*), TAG-004 (frontmatter tags), TAG-009 (AI tags stay out of the file
//! unless the user asks for a writeback), TAG-014, KB-014, AC-002, AC-005.
//!
//! The central decision: the block's original text is kept and edited
//! surgically, never reserialised. Round-tripping through a YAML emitter would
//! reorder keys, drop comments, requote strings and reflow lists — all of which
//! would show up as spurious diffs in the user's own git history. "Preserve
//! unknown fields" is read here as "preserve the bytes".

use std::collections::BTreeMap;
use std::ops::Range;

use marius_model::MetadataValue;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, OffsetDateTime, PrimitiveDateTime};

/// A document split into its frontmatter block and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitDocument<'a> {
    /// The frontmatter block, if the document opened with one.
    pub frontmatter: Option<FrontmatterBlock<'a>>,
    /// The body text, with frontmatter removed.
    pub body: &'a str,
    /// Byte offset of `body` within the original text. Every span this crate
    /// reports is absolute in the original document, so citations and link
    /// rewriting address the real file (SRCH-010, MD-007).
    pub body_offset: usize,
}

/// A frontmatter block, located and sliced but not yet interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterBlock<'a> {
    /// The YAML between the delimiters, excluding the delimiter lines.
    pub yaml: &'a str,
    /// Byte range of `yaml` in the original document.
    pub yaml_span: Range<usize>,
    /// Byte range of the whole block including both delimiter lines and the
    /// newline that ends the closing delimiter.
    pub block_span: Range<usize>,
}

/// Split leading YAML frontmatter from a document.
///
/// A block is recognised only when `---` is the very first line, which is what
/// every Markdown tool in this ecosystem requires; a `---` further down is a
/// thematic break and is left alone.
pub fn split(text: &str) -> SplitDocument<'_> {
    // A UTF-8 BOM before the opening fence is common on Windows and must not
    // prevent recognition.
    let bom = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let rest = &text[bom..];

    let Some(after_open) = open_delimiter(rest) else {
        return SplitDocument {
            frontmatter: None,
            body: text,
            body_offset: 0,
        };
    };

    let yaml_start = bom + after_open;
    let Some((yaml_len, close_len)) = close_delimiter(&text[yaml_start..]) else {
        // An unterminated block is not frontmatter. Treating the rest of the
        // file as YAML would hide the user's entire document.
        return SplitDocument {
            frontmatter: None,
            body: text,
            body_offset: 0,
        };
    };

    let yaml_span = yaml_start..yaml_start + yaml_len;
    let block_end = yaml_span.end + close_len;
    SplitDocument {
        frontmatter: Some(FrontmatterBlock {
            yaml: &text[yaml_span.clone()],
            yaml_span,
            block_span: bom..block_end,
        }),
        body: &text[block_end..],
        body_offset: block_end,
    }
}

/// Length of the opening `---` line, including its newline.
fn open_delimiter(text: &str) -> Option<usize> {
    let line_end = text.find('\n').map_or(text.len(), |i| i + 1);
    let line = text[..line_end].trim_end_matches(['\n', '\r']);
    (line.trim_end() == "---").then_some(line_end)
}

/// Find the closing delimiter, returning `(yaml_len, close_line_len)`.
fn close_delimiter(after_open: &str) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    while offset < after_open.len() {
        let line_end = after_open[offset..]
            .find('\n')
            .map_or(after_open.len(), |i| offset + i + 1);
        let line = after_open[offset..line_end].trim_end_matches(['\n', '\r']);
        // `...` is YAML's document-end marker and some exporters emit it.
        if matches!(line.trim_end(), "---" | "...") {
            return Some((offset, line_end - offset));
        }
        offset = line_end;
    }
    None
}

/// Error from interpreting a frontmatter block as YAML.
#[derive(Debug, thiserror::Error)]
#[error("frontmatter is not valid YAML: {source}")]
pub struct FrontmatterError {
    #[from]
    source: serde_yaml::Error,
}

/// Interpreted frontmatter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frontmatter {
    /// Every top-level key, in sorted order, typed for querying (TAG-014).
    pub values: BTreeMap<String, MetadataValue>,
}

impl Frontmatter {
    /// Interpret a block's YAML.
    ///
    /// A block that is not a mapping (a bare list, say) yields no keys rather
    /// than an error: it is still the user's content, still preserved verbatim,
    /// and simply has nothing queryable in it.
    pub fn parse(yaml: &str) -> Result<Self, FrontmatterError> {
        if yaml.trim().is_empty() {
            return Ok(Self::default());
        }
        let value: serde_yaml::Value = serde_yaml::from_str(yaml)?;
        let mut values = BTreeMap::new();
        if let serde_yaml::Value::Mapping(map) = value {
            for (k, v) in map {
                if let Some(key) = yaml_key(&k) {
                    values.insert(key, convert(v));
                }
            }
        }
        Ok(Self { values })
    }

    /// Interpret a block, falling back to a lenient reading when it is not
    /// valid YAML.
    ///
    /// Pasted and imported frontmatter is frequently *almost* YAML. The
    /// commonest case by far is an unquoted value containing a colon — an
    /// email subject, `title: Re: harbour survey` — which a strict parser
    /// rejects outright, discarding every other key in the block with it.
    /// IM-004 asks for imported metadata to be preserved where possible, so a
    /// failed strict parse is followed by a line-oriented recovery that reads
    /// what it can. The returned warning says that recovery happened, and the
    /// original bytes are preserved for writeback either way (MD-008).
    pub fn parse_or_recover(yaml: &str) -> (Self, Option<String>) {
        match Self::parse(yaml) {
            Ok(frontmatter) => (frontmatter, None),
            Err(error) => {
                let recovered = Self::recover(yaml);
                let warning = if recovered.values.is_empty() {
                    error.to_string()
                } else {
                    format!("{error} (read {} key(s) leniently)", recovered.values.len())
                };
                (recovered, Some(warning))
            }
        }
    }

    /// Line-oriented reading of a block that is not valid YAML.
    ///
    /// Deliberately simple and total: it never fails, and it only understands
    /// what appears in hand-written frontmatter — top-level `key: value`, an
    /// inline `[a, b]` list, and `- item` continuation lines.
    pub fn recover(yaml: &str) -> Self {
        let mut values: BTreeMap<String, MetadataValue> = BTreeMap::new();
        let mut current: Option<(String, Vec<String>)> = None;

        let flush = |values: &mut BTreeMap<String, MetadataValue>,
                     entry: Option<(String, Vec<String>)>| {
            if let Some((key, items)) = entry {
                if !items.is_empty() {
                    values.insert(
                        key,
                        MetadataValue::List(items.into_iter().map(MetadataValue::Text).collect()),
                    );
                }
            }
        };

        for line in yaml.lines() {
            let trimmed = line.trim_end();
            if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('#') {
                continue;
            }
            // A `- item` line continues the previous key's list.
            if let Some(item) = trimmed.trim_start().strip_prefix("- ") {
                if let Some((_, items)) = current.as_mut() {
                    items.push(unquote(item.trim()).to_owned());
                }
                continue;
            }
            // Indented lines belong to a structure this reading cannot
            // represent; they are kept out rather than guessed at.
            if trimmed.starts_with([' ', '\t']) {
                continue;
            }
            let Some((key, rest)) = trimmed.split_once(':') else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            flush(&mut values, current.take());

            let rest = rest.trim();
            if rest.is_empty() {
                // Either a null, or a list whose items follow.
                current = Some((key.to_owned(), Vec::new()));
                values.insert(key.to_owned(), MetadataValue::Null);
                continue;
            }
            // A value opening with `[` was meant as a list. The closing
            // bracket may be missing — that is often exactly why the strict
            // parse failed — so it is optional here: reading `[unclosed` as
            // the literal text `[unclosed` would preserve the bytes while
            // losing the intent.
            if let Some(opened) = rest.strip_prefix('[') {
                let inner = opened.strip_suffix(']').unwrap_or(opened);
                let items: Vec<MetadataValue> = inner
                    .split(',')
                    .map(|item| unquote(item.trim()))
                    .filter(|item| !item.is_empty())
                    .map(|item| MetadataValue::Text(item.to_owned()))
                    .collect();
                values.insert(key.to_owned(), MetadataValue::List(items));
                continue;
            }
            // Everything else is text — including the colons that made the
            // block invalid in the first place.
            let text = unquote(rest);
            let value = match parse_date(text) {
                Some(date) => MetadataValue::Date(date),
                None => MetadataValue::Text(text.to_owned()),
            };
            values.insert(key.to_owned(), value);
        }
        flush(&mut values, current.take());
        Self { values }
    }

    /// Look up a key case-insensitively.
    ///
    /// Users write `Tags:`, `tags:` and `TAGS:` interchangeably, and an import
    /// from another tool may use any of them (AC-002).
    pub fn get(&self, key: &str) -> Option<&MetadataValue> {
        self.values.get(key).or_else(|| {
            self.values
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v)
        })
    }

    /// The document's declared title, if any (KB-007).
    pub fn title(&self) -> Option<String> {
        match self.get("title") {
            Some(MetadataValue::Text(t)) if !t.trim().is_empty() => Some(t.trim().to_owned()),
            Some(other @ (MetadataValue::Number(_) | MetadataValue::Boolean(_))) => {
                Some(other.to_index_text())
            }
            _ => None,
        }
    }

    /// Declared tags, accepting `tags`, `tag` and `keywords`, each of which may
    /// hold a scalar or a list (TAG-004).
    pub fn tags(&self) -> Vec<String> {
        for key in ["tags", "tag", "keywords"] {
            if let Some(value) = self.get(key) {
                let list = value.to_string_list();
                if !list.is_empty() {
                    // A single string holding several tags — `tags: a, b` or
                    // `tags: #a #b` — is common in hand-written frontmatter.
                    return list
                        .iter()
                        .flat_map(|item| {
                            item.split([',', ';'])
                                .flat_map(str::split_whitespace)
                                .map(|t| t.trim_start_matches('#').to_owned())
                        })
                        .filter(|t| !t.is_empty())
                        .collect();
                }
            }
        }
        Vec::new()
    }

    /// Declared aliases (KB-010, MD-006).
    pub fn aliases(&self) -> Vec<String> {
        for key in ["aliases", "alias"] {
            if let Some(value) = self.get(key) {
                let list: Vec<String> = value
                    .to_string_list()
                    .into_iter()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !list.is_empty() {
                    return list;
                }
            }
        }
        Vec::new()
    }

    /// The stable ID recorded in the file, for vaults that opted into writeback
    /// (see `marius_model::FRONTMATTER_ID_KEY`).
    pub fn stable_id(&self) -> Option<String> {
        match self.get(marius_model::FRONTMATTER_ID_KEY) {
            Some(MetadataValue::Text(t)) => Some(t.trim().to_owned()),
            _ => None,
        }
    }

    /// Keys Marius interprets itself. Everything else is the user's and is only
    /// ever read, indexed and preserved (MD-008).
    pub const RECOGNISED_KEYS: &'static [&'static str] = &[
        "title",
        "tags",
        "tag",
        "keywords",
        "aliases",
        "alias",
        marius_model::FRONTMATTER_ID_KEY,
    ];

    /// Keys this document carries that Marius does not interpret.
    pub fn unrecognised_keys(&self) -> Vec<&str> {
        self.values
            .keys()
            .map(String::as_str)
            .filter(|k| {
                !Self::RECOGNISED_KEYS
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(k))
            })
            .collect()
    }
}

/// Strip one layer of matching quotes.
fn unquote(text: &str) -> &str {
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return &text[1..text.len() - 1];
        }
    }
    text
}

fn yaml_key(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn convert(value: serde_yaml::Value) -> MetadataValue {
    match value {
        serde_yaml::Value::Null => MetadataValue::Null,
        serde_yaml::Value::Bool(b) => MetadataValue::Boolean(b),
        serde_yaml::Value::Number(n) => n
            .as_f64()
            .map_or(MetadataValue::Text(n.to_string()), MetadataValue::Number),
        serde_yaml::Value::String(s) => match parse_date(&s) {
            Some(date) => MetadataValue::Date(date),
            None => MetadataValue::Text(s),
        },
        serde_yaml::Value::Sequence(items) => {
            MetadataValue::List(items.into_iter().map(convert).collect())
        }
        serde_yaml::Value::Mapping(map) => MetadataValue::Map(
            map.into_iter()
                .filter_map(|(k, v)| yaml_key(&k).map(|k| (k, convert(v))))
                .collect(),
        ),
        // Tagged values (`!!custom foo`) keep their text form rather than being
        // dropped; the verbatim block is still authoritative for writeback.
        serde_yaml::Value::Tagged(tagged) => convert(tagged.value),
    }
}

/// Recognise the date shapes that appear in real frontmatter.
///
/// Only an exact, whole-string match counts — a note titled "2024 in review"
/// must stay text.
fn parse_date(s: &str) -> Option<OffsetDateTime> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = OffsetDateTime::parse(s, &Rfc3339) {
        return Some(dt);
    }
    const DATE: &[time::format_description::FormatItem<'_>] =
        format_description!("[year]-[month]-[day]");
    if let Ok(date) = Date::parse(s, DATE) {
        return Some(date.midnight().assume_utc());
    }
    for fmt in [
        format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]"),
        format_description!("[year]-[month]-[day] [hour]:[minute]"),
        format_description!("[year]-[month]-[day]T[hour]:[minute]"),
    ] {
        if let Ok(dt) = PrimitiveDateTime::parse(s, fmt) {
            return Some(dt.assume_utc());
        }
    }
    None
}

/// Replace or insert one top-level key in a frontmatter block, leaving every
/// other byte of the block untouched.
///
/// This is the only way Marius ever writes to a user's frontmatter, and it only
/// runs on an explicit, user-authorised writeback (TAG-009, AC-005). `value` is
/// inserted verbatim, so callers pass already-formatted YAML.
pub fn upsert_key(yaml: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(yaml.len() + key.len() + value.len() + 4);
    let mut replaced = false;
    let mut lines = yaml.split_inclusive('\n').peekable();

    while let Some(line) = lines.next() {
        if !replaced && top_level_key(line).is_some_and(|k| k.eq_ignore_ascii_case(key)) {
            out.push_str(&format!("{key}: {value}\n"));
            replaced = true;
            // Skip the old value's continuation lines (block scalars, nested
            // mappings, `-` list items) so the replacement does not leave
            // orphaned fragments behind.
            while lines
                .peek()
                .is_some_and(|next| top_level_key(next).is_none() && !next.trim().is_empty())
            {
                lines.next();
            }
            continue;
        }
        out.push_str(line);
    }

    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{key}: {value}\n"));
    }
    out
}

/// Remove one top-level key and its value block.
pub fn remove_key(yaml: &str, key: &str) -> String {
    let mut out = String::with_capacity(yaml.len());
    let mut lines = yaml.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if top_level_key(line).is_some_and(|k| k.eq_ignore_ascii_case(key)) {
            while lines
                .peek()
                .is_some_and(|next| top_level_key(next).is_none() && !next.trim().is_empty())
            {
                lines.next();
            }
            continue;
        }
        out.push_str(line);
    }
    out
}

/// The key name if `line` starts a top-level mapping entry.
///
/// Indented lines, list items and comments belong to the previous key's value.
fn top_level_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.starts_with([' ', '\t', '-', '#']) || trimmed.is_empty() {
        return None;
    }
    let (key, _) = trimmed.split_once(':')?;
    let key = key.trim();
    (!key.is_empty() && !key.contains(' ') || key.chars().all(|c| !c.is_control()))
        .then_some(key)
        .filter(|k| !k.is_empty())
}

/// Render a complete document from a frontmatter block and a body.
pub fn render(yaml: Option<&str>, body: &str) -> String {
    match yaml {
        Some(yaml) => {
            let yaml = yaml.strip_suffix('\n').unwrap_or(yaml);
            format!("---\n{yaml}\n---\n{body}")
        }
        None => body.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_document_without_frontmatter_is_left_alone() {
        let text = "# Alexandria\n\nThe Pharos.\n";
        let split = split(text);
        assert!(split.frontmatter.is_none());
        assert_eq!(split.body, text);
        assert_eq!(split.body_offset, 0);
    }

    #[test]
    fn frontmatter_is_located_with_exact_spans() {
        let text = "---\ntitle: Pharos\n---\n# Body\n";
        let split = split(text);
        let fm = split.frontmatter.unwrap();
        assert_eq!(fm.yaml, "title: Pharos\n");
        assert_eq!(&text[fm.yaml_span.clone()], "title: Pharos\n");
        assert_eq!(&text[fm.block_span.clone()], "---\ntitle: Pharos\n---\n");
        assert_eq!(split.body, "# Body\n");
        assert_eq!(split.body_offset, "---\ntitle: Pharos\n---\n".len());
    }

    #[test]
    fn a_thematic_break_mid_document_is_not_frontmatter() {
        let text = "# Heading\n\n---\n\nmore\n";
        assert!(split(text).frontmatter.is_none());
    }

    #[test]
    fn an_unterminated_block_is_not_treated_as_frontmatter() {
        // Otherwise a stray `---` on line 1 would swallow the whole note.
        let text = "---\ntitle: Pharos\n\n# Body\n";
        let split = split(text);
        assert!(split.frontmatter.is_none());
        assert_eq!(split.body, text);
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_frontmatter() {
        let text = "\u{feff}---\ntitle: Pharos\n---\nbody\n";
        let fm = split(text).frontmatter.expect("BOM must be tolerated");
        assert_eq!(fm.yaml, "title: Pharos\n");
    }

    #[test]
    fn windows_line_endings_are_handled() {
        let text = "---\r\ntitle: Pharos\r\n---\r\nbody\r\n";
        let split = split(text);
        let fm = split.frontmatter.expect("CRLF must be tolerated");
        assert_eq!(fm.yaml, "title: Pharos\r\n");
        assert_eq!(split.body, "body\r\n");
    }

    #[test]
    fn an_empty_block_parses_to_no_keys() {
        let text = "---\n---\nbody";
        let split = split(text);
        let fm = split.frontmatter.unwrap();
        assert_eq!(fm.yaml, "");
        assert!(Frontmatter::parse(fm.yaml).unwrap().values.is_empty());
        assert_eq!(split.body, "body");
    }

    #[test]
    fn a_yaml_end_marker_closes_the_block() {
        let text = "---\ntitle: x\n...\nbody";
        assert_eq!(split(text).body, "body");
    }

    #[test]
    fn values_are_typed_for_querying() {
        // TAG-014.
        let fm = Frontmatter::parse(
            "title: Pharos\nheight: 100\nstanding: false\nbuilt: 1869-05-01\nnothing:\n",
        )
        .unwrap();
        assert_eq!(fm.title().as_deref(), Some("Pharos"));
        assert_eq!(fm.get("height"), Some(&MetadataValue::Number(100.0)));
        assert_eq!(fm.get("standing"), Some(&MetadataValue::Boolean(false)));
        assert_eq!(fm.get("built").unwrap().type_name(), "date");
        assert_eq!(fm.get("nothing"), Some(&MetadataValue::Null));
    }

    #[test]
    fn only_whole_string_dates_become_dates() {
        let fm = Frontmatter::parse("a: 2024-01-01\nb: 2024 in review\nc: v1.2.3\n").unwrap();
        assert_eq!(fm.get("a").unwrap().type_name(), "date");
        assert_eq!(fm.get("b").unwrap().type_name(), "text");
        assert_eq!(fm.get("c").unwrap().type_name(), "text");
    }

    #[test]
    fn datetime_shapes_round_trip_to_dates() {
        for input in [
            "2024-03-05T10:00:00Z",
            "2024-03-05 10:00:00",
            "2024-03-05T10:00",
            "2024-03-05",
        ] {
            assert!(parse_date(input).is_some(), "{input} should parse");
        }
        for input in ["", "not a date", "2024", "03/05/2024"] {
            assert!(parse_date(input).is_none(), "{input} should not parse");
        }
    }

    #[test]
    fn keys_are_found_whatever_their_case() {
        // AC-002: imports from other tools vary.
        let fm = Frontmatter::parse("Title: Pharos\nTAGS: [a, b]\n").unwrap();
        assert_eq!(fm.title().as_deref(), Some("Pharos"));
        assert_eq!(fm.tags(), vec!["a", "b"]);
    }

    #[test]
    fn tags_are_accepted_in_every_shape_people_write_them() {
        // TAG-004.
        for (yaml, expected) in [
            ("tags: [lighthouse, history]", vec!["lighthouse", "history"]),
            (
                "tags:\n  - lighthouse\n  - history",
                vec!["lighthouse", "history"],
            ),
            ("tags: lighthouse", vec!["lighthouse"]),
            ("tags: lighthouse, history", vec!["lighthouse", "history"]),
            (
                "tags: \"#lighthouse #history\"",
                vec!["lighthouse", "history"],
            ),
            ("tag: lighthouse", vec!["lighthouse"]),
            ("keywords: [lighthouse]", vec!["lighthouse"]),
        ] {
            assert_eq!(Frontmatter::parse(yaml).unwrap().tags(), expected, "{yaml}");
        }
    }

    #[test]
    fn aliases_are_read_for_link_resolution() {
        // KB-010.
        let fm =
            Frontmatter::parse("aliases:\n  - Pharos\n  - Lighthouse of Alexandria\n").unwrap();
        assert_eq!(fm.aliases(), vec!["Pharos", "Lighthouse of Alexandria"]);
        assert_eq!(
            Frontmatter::parse("alias: Pharos").unwrap().aliases(),
            vec!["Pharos"]
        );
    }

    #[test]
    fn nested_mappings_are_kept_as_structure() {
        let fm = Frontmatter::parse("meta:\n  source: letter\n  year: 1869\n").unwrap();
        let MetadataValue::Map(map) = fm.get("meta").unwrap() else {
            panic!("expected a map, got {:?}", fm.get("meta"));
        };
        assert_eq!(
            map.get("source"),
            Some(&MetadataValue::Text("letter".into()))
        );
        assert_eq!(map.get("year"), Some(&MetadataValue::Number(1869.0)));
    }

    #[test]
    fn unknown_keys_are_kept_and_reported() {
        // MD-008 — the requirement this module exists for.
        let fm = Frontmatter::parse(
            "title: Pharos\ncssclass: wide\nobsidian-plugin-field: 7\npublish: true\n",
        )
        .unwrap();
        let mut unknown = fm.unrecognised_keys();
        unknown.sort_unstable();
        assert_eq!(
            unknown,
            vec!["cssclass", "obsidian-plugin-field", "publish"]
        );
        assert_eq!(
            fm.get("cssclass"),
            Some(&MetadataValue::Text("wide".into()))
        );
    }

    #[test]
    fn a_non_mapping_block_yields_no_keys_rather_than_an_error() {
        let fm = Frontmatter::parse("- just\n- a list\n").unwrap();
        assert!(fm.values.is_empty());
    }

    #[test]
    fn broken_yaml_is_reported_not_silently_dropped() {
        assert!(Frontmatter::parse("title: [unclosed\n").is_err());
    }

    #[test]
    fn a_pasted_email_header_block_is_recovered_rather_than_discarded() {
        // IM-004: an unquoted colon is the commonest flaw in pasted
        // frontmatter, and it must not cost every other key in the block.
        let yaml = "title: Re: harbour survey\nfrom: a colleague\ndate: 2024-03-05\n";
        assert!(
            Frontmatter::parse(yaml).is_err(),
            "this really is invalid YAML"
        );

        let (recovered, warning) = Frontmatter::parse_or_recover(yaml);
        assert!(warning.is_some());
        assert_eq!(
            recovered.get("title"),
            Some(&MetadataValue::Text("Re: harbour survey".into()))
        );
        assert_eq!(
            recovered.get("from"),
            Some(&MetadataValue::Text("a colleague".into()))
        );
        assert_eq!(recovered.get("date").unwrap().type_name(), "date");
    }

    #[test]
    fn valid_yaml_is_never_sent_down_the_recovery_path() {
        let (parsed, warning) = Frontmatter::parse_or_recover("title: Pharos\ntags: [a, b]\n");
        assert_eq!(warning, None);
        assert_eq!(parsed.title().as_deref(), Some("Pharos"));
        assert_eq!(parsed.tags(), vec!["a", "b"]);
    }

    #[test]
    fn recovery_reads_lists_in_both_spellings() {
        let recovered = Frontmatter::recover("tags: [a, b]\naliases:\n  - one\n  - two\n");
        assert_eq!(recovered.tags(), vec!["a", "b"]);
        assert_eq!(recovered.aliases(), vec!["one", "two"]);
    }

    #[test]
    fn recovery_reads_an_unclosed_list_as_a_list() {
        let recovered = Frontmatter::recover("tags: [history, lighthouse\n");
        assert_eq!(recovered.tags(), vec!["history", "lighthouse"]);
        // And the key is not mistaken for a title string.
        assert_eq!(Frontmatter::recover("title: [unclosed\n").title(), None);
    }

    #[test]
    fn recovery_strips_quotes_and_keeps_nothing_it_cannot_read() {
        let recovered =
            Frontmatter::recover("quoted: \"with spaces\"\nempty:\nnested:\n  deep: value\n");
        assert_eq!(
            recovered.get("quoted"),
            Some(&MetadataValue::Text("with spaces".into()))
        );
        assert_eq!(recovered.get("empty"), Some(&MetadataValue::Null));
        // A nested block is not guessed at.
        assert!(!recovered.values.contains_key("deep"));
    }

    #[test]
    fn recovery_never_panics_on_arbitrary_text() {
        for input in ["", ":::", "- just a list", "no colon here", "a: b: c: d"] {
            let _ = Frontmatter::recover(input);
        }
    }

    #[test]
    fn writeback_replaces_one_key_and_touches_nothing_else() {
        // TAG-009: the only sanctioned write into a user's frontmatter.
        let yaml = "# a comment\ntitle: Pharos\ntags:\n  - old\ncssclass: wide\n";
        let updated = upsert_key(yaml, "tags", "[new, newer]");
        assert_eq!(
            updated,
            "# a comment\ntitle: Pharos\ntags: [new, newer]\ncssclass: wide\n"
        );
    }

    #[test]
    fn writeback_appends_a_missing_key() {
        let updated = upsert_key("title: Pharos\n", "marius-id", "abc");
        assert_eq!(updated, "title: Pharos\nmarius-id: abc\n");
        assert_eq!(upsert_key("", "marius-id", "abc"), "marius-id: abc\n");
    }

    #[test]
    fn writeback_matches_the_users_capitalisation_of_the_key() {
        let updated = upsert_key("Tags: [old]\n", "tags", "[new]");
        assert_eq!(updated, "tags: [new]\n");
    }

    #[test]
    fn removing_a_key_takes_its_whole_value_block() {
        let yaml = "title: Pharos\ntags:\n  - a\n  - b\ncssclass: wide\n";
        assert_eq!(remove_key(yaml, "tags"), "title: Pharos\ncssclass: wide\n");
    }

    #[test]
    fn a_full_document_round_trips_through_split_and_render() {
        let original = "---\ntitle: Pharos\ncssclass: wide\n---\n# Body\n\nText.\n";
        let split = split(original);
        let fm = split.frontmatter.as_ref().unwrap();
        assert_eq!(render(Some(fm.yaml), split.body), original);
    }

    #[test]
    fn a_document_with_no_frontmatter_round_trips_unchanged() {
        let original = "# Body\n";
        let split = split(original);
        assert_eq!(render(None, split.body), original);
    }

    #[test]
    fn stable_ids_are_read_back_from_the_file() {
        let fm = Frontmatter::parse("marius-id: 0192f0c4-1234-7000-8000-000000000001\n").unwrap();
        assert_eq!(
            fm.stable_id().as_deref(),
            Some("0192f0c4-1234-7000-8000-000000000001")
        );
    }
}
