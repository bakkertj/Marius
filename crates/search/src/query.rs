//! The query language.
//!
//! Requirements: SRCH-001 .. SRCH-006, SRCH-002 (search by tags, path, vault,
//! date, document type and metadata), SRCH-003 (exact phrases, Boolean
//! operators), KB-012 (archived content stays searchable unless filtered),
//! AI-012 (generated notes filterable).
//!
//! ## Shape of the language
//!
//! ```text
//! lighthouse harbour              both words (implicit AND)
//! "harbour mouth"                 exact phrase
//! -draft    NOT draft             exclusion
//! history OR engineering          alternatives
//! tag:lighthouse                  structured filter
//! tag:project/marius              hierarchical tag
//! -tag:draft                      negated filter
//! path:research/                  path prefix
//! vault:work                      vault by name
//! type:email                      document kind
//! title:pharos                    field-scoped text
//! modified:>2024-01-01            date comparison
//! is:archived  is:generated       state filters
//! ```
//!
//! Text terms become an FTS5 MATCH expression; structured filters stay
//! structured, because `tag:` has to respect hierarchy and provenance, and a
//! date needs comparison rather than token matching. Keeping them apart is also
//! what lets a tag-only query skip the full-text index entirely.
//!
//! Unparseable input is never silently dropped: a bare `tag:` with no value, or
//! a date that is not a date, is reported so the user can see why their search
//! did not do what they expected.

use std::fmt;

use marius_model::DocumentKind;
use time::macros::format_description;
use time::{Date, OffsetDateTime};

/// A parsed query.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    /// FTS5 MATCH expression, or `None` when the query has no text terms.
    pub fts: Option<String>,
    /// Tags every result must carry.
    pub all_tags: Vec<String>,
    /// Tags at least one of which every result must carry.
    pub any_tags: Vec<String>,
    /// Tags no result may carry.
    pub none_tags: Vec<String>,
    /// Path prefixes; a result must be under one of them.
    pub paths: Vec<String>,
    /// Vault names or identifiers; empty means every accessible vault
    /// (SRCH-004).
    pub vaults: Vec<String>,
    pub kinds: Vec<DocumentKind>,
    pub created_after: Option<OffsetDateTime>,
    pub created_before: Option<OffsetDateTime>,
    pub modified_after: Option<OffsetDateTime>,
    pub modified_before: Option<OffsetDateTime>,
    /// `Some(true)` for only archived, `Some(false)` to exclude archived,
    /// `None` to include both — the default, per KB-012.
    pub archived: Option<bool>,
    /// `Some(false)` hides AI-generated notes (AI-012).
    pub generated: Option<bool>,
    /// The text as typed, for saved searches and for showing the user back
    /// what was run.
    pub raw: String,
}

impl Query {
    /// True when the query constrains nothing and should match everything.
    pub fn is_empty(&self) -> bool {
        self.fts.is_none()
            && self.all_tags.is_empty()
            && self.any_tags.is_empty()
            && self.none_tags.is_empty()
            && self.paths.is_empty()
            && self.vaults.is_empty()
            && self.kinds.is_empty()
            && self.created_after.is_none()
            && self.created_before.is_none()
            && self.modified_after.is_none()
            && self.modified_before.is_none()
            && self.archived.is_none()
            && self.generated.is_none()
    }

    /// True when the query can be answered without touching the full-text
    /// index, which makes a tag-only or date-only browse much cheaper.
    pub fn needs_full_text(&self) -> bool {
        self.fts.is_some()
    }
}

/// Why a query could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("`{field}:` needs a value")]
    MissingValue { field: String },
    #[error("unknown filter `{field}:` — try one of: {known}")]
    UnknownField { field: String, known: String },
    #[error("`{value}` is not a date; use YYYY-MM-DD")]
    BadDate { value: String },
    #[error("`type:{value}` is not a document type; try markdown, plain_text, email, attachment or generated_note")]
    BadKind { value: String },
    #[error("`is:{value}` is not a state; try archived or generated")]
    BadState { value: String },
    #[error("unbalanced quotation mark")]
    UnbalancedQuote,
    #[error("`OR` needs a term on both sides")]
    DanglingOr,
}

/// Field names the language accepts.
const FIELDS: &[&str] = &[
    "tag", "tags", "path", "folder", "vault", "type", "kind", "title", "body", "alias", "aliases",
    "created", "modified", "is",
];

/// Parse a query.
pub fn parse(input: &str) -> Result<Query, QueryError> {
    let tokens = tokenize(input)?;
    let mut query = Query {
        raw: input.trim().to_owned(),
        ..Query::default()
    };

    // Text terms, as FTS5 fragments. `or_pending` tracks whether the previous
    // token was `OR`, so alternatives group instead of intersecting.
    let mut fts_terms: Vec<String> = Vec::new();
    let mut or_pending = false;
    let mut had_or_target = false;

    for token in tokens {
        match token {
            Token::Or => {
                if fts_terms.is_empty() && query.any_tags.is_empty() && !had_or_target {
                    return Err(QueryError::DanglingOr);
                }
                or_pending = true;
                continue;
            }
            Token::Text {
                text,
                negated,
                phrase,
            } => {
                let fragment = if phrase {
                    marius_storage::search_index::quote_phrase(&text)
                } else {
                    fts_term(&text)
                };
                if negated {
                    fts_terms.push(format!("NOT {fragment}"));
                } else if or_pending {
                    // Fold into the previous term as an alternative.
                    match fts_terms.pop() {
                        Some(previous) => fts_terms.push(format!("({previous} OR {fragment})")),
                        None => fts_terms.push(fragment),
                    }
                } else {
                    fts_terms.push(fragment);
                }
                had_or_target = true;
            }
            Token::Field {
                name,
                op,
                value,
                negated,
            } => {
                apply_field(
                    &mut query,
                    &name,
                    op,
                    &value,
                    negated,
                    or_pending,
                    &mut fts_terms,
                )?;
                had_or_target = true;
            }
        }
        or_pending = false;
    }
    if or_pending {
        return Err(QueryError::DanglingOr);
    }

    if !fts_terms.is_empty() {
        query.fts = Some(join_and(&fts_terms));
    }
    Ok(query)
}

/// Join AND-ed fragments, keeping `NOT` fragments adjacent to what they negate.
///
/// FTS5 requires `a NOT b` rather than `a AND NOT b`, so negations are appended
/// without an explicit `AND`.
fn join_and(terms: &[String]) -> String {
    let mut out = String::new();
    for term in terms {
        if out.is_empty() {
            // A query that is only a negation needs something to subtract from.
            if let Some(rest) = term.strip_prefix("NOT ") {
                out.push_str(&format!("(* NOT {rest})"));
            } else {
                out.push_str(term);
            }
            continue;
        }
        if term.starts_with("NOT ") {
            out.push(' ');
            out.push_str(term);
        } else {
            out.push_str(" AND ");
            out.push_str(term);
        }
    }
    out
}

/// Turn one bare word into an FTS5 fragment.
///
/// A trailing `*` is kept as a prefix search; everything else is quoted,
/// because words like `C++`, `read-later` and `foo:bar` are syntax to FTS5.
fn fts_term(text: &str) -> String {
    match text.strip_suffix('*') {
        Some(stem) if !stem.is_empty() => {
            format!("{}*", marius_storage::search_index::quote_phrase(stem))
        }
        _ => marius_storage::search_index::quote_phrase(text),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_field(
    query: &mut Query,
    name: &str,
    op: Comparison,
    value: &str,
    negated: bool,
    or_pending: bool,
    fts_terms: &mut Vec<String>,
) -> Result<(), QueryError> {
    if value.is_empty() {
        return Err(QueryError::MissingValue {
            field: name.to_owned(),
        });
    }
    match name {
        "tag" | "tags" => {
            if negated {
                query.none_tags.push(value.to_owned());
            } else if or_pending {
                // `tag:a OR tag:b` is an alternation, so the previously
                // required tag moves into the `any` set with this one.
                if let Some(previous) = query.all_tags.pop() {
                    query.any_tags.push(previous);
                }
                query.any_tags.push(value.to_owned());
            } else {
                query.all_tags.push(value.to_owned());
            }
        }
        "path" | "folder" => query.paths.push(value.trim_end_matches('/').to_owned()),
        "vault" => query.vaults.push(value.to_owned()),
        "type" | "kind" => {
            let kind = value
                .parse::<DocumentKind>()
                .map_err(|_| QueryError::BadKind {
                    value: value.to_owned(),
                })?;
            query.kinds.push(kind);
        }
        // Field-scoped text goes to FTS5, which supports column filters.
        "title" | "body" | "alias" | "aliases" => {
            let column = if name.starts_with("alias") {
                "aliases"
            } else {
                name
            };
            let fragment = format!("{column}:{}", fts_term(value));
            fts_terms.push(if negated {
                format!("NOT {fragment}")
            } else {
                fragment
            });
        }
        "created" | "modified" => {
            let date = parse_date(value)?;
            let (after, before) = match name {
                "created" => (&mut query.created_after, &mut query.created_before),
                _ => (&mut query.modified_after, &mut query.modified_before),
            };
            match op {
                Comparison::Greater => *after = Some(date),
                Comparison::Less => *before = Some(date),
                // `modified:2024-03-05` means that whole day.
                Comparison::Equal => {
                    *after = Some(date - time::Duration::seconds(1));
                    *before = Some(date + time::Duration::days(1));
                }
            }
        }
        "is" => match value.to_ascii_lowercase().as_str() {
            "archived" => query.archived = Some(!negated),
            "generated" | "ai" => query.generated = Some(!negated),
            other => {
                return Err(QueryError::BadState {
                    value: other.to_owned(),
                })
            }
        },
        other => {
            return Err(QueryError::UnknownField {
                field: other.to_owned(),
                known: FIELDS.join(", "),
            })
        }
    }
    Ok(())
}

fn parse_date(value: &str) -> Result<OffsetDateTime, QueryError> {
    const DATE: &[time::format_description::FormatItem<'_>] =
        format_description!("[year]-[month]-[day]");
    if let Ok(date) = Date::parse(value, DATE) {
        return Ok(date.midnight().assume_utc());
    }
    if let Ok(dt) = OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339) {
        return Ok(dt);
    }
    // Relative shorthands, because "what did I change this week" is the
    // question people actually ask.
    let now = OffsetDateTime::now_utc();
    let relative = match value.to_ascii_lowercase().as_str() {
        "today" => Some(now.replace_time(time::Time::MIDNIGHT)),
        "yesterday" => Some(now.replace_time(time::Time::MIDNIGHT) - time::Duration::days(1)),
        "week" | "this-week" => Some(now - time::Duration::weeks(1)),
        "month" | "this-month" => Some(now - time::Duration::days(30)),
        "year" | "this-year" => Some(now - time::Duration::days(365)),
        _ => None,
    };
    relative.ok_or_else(|| QueryError::BadDate {
        value: value.to_owned(),
    })
}

/// Comparison in a field filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Equal,
    Greater,
    Less,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Text {
        text: String,
        negated: bool,
        phrase: bool,
    },
    Field {
        name: String,
        op: Comparison,
        value: String,
        negated: bool,
    },
    Or,
}

fn tokenize(input: &str) -> Result<Vec<Token>, QueryError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut negate_next = false;

    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let mut negated = negate_next;
        negate_next = false;

        if chars[i] == '-' && i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
            negated = true;
            i += 1;
        }

        if chars[i] == '"' {
            let (text, next) = read_quoted(&chars, i)?;
            i = next;
            if !text.trim().is_empty() {
                tokens.push(Token::Text {
                    text,
                    negated,
                    phrase: true,
                });
            }
            continue;
        }

        // Read a bare run, stopping at whitespace. A `"` inside the run starts
        // a quoted value, which is how `tag:"two words"` works.
        let start = i;
        let mut colon: Option<usize> = None;
        while i < chars.len() && !chars[i].is_whitespace() {
            if chars[i] == ':' && colon.is_none() {
                colon = Some(i);
            }
            if chars[i] == '"' {
                break;
            }
            i += 1;
        }
        let head: String = chars[start..i].iter().collect();

        if let Some(colon_at) = colon {
            let name: String = chars[start..colon_at].iter().collect();
            let name_lower = name.to_ascii_lowercase();
            let rest_preview: String = chars[colon_at + 1..i].iter().collect();
            // `http://example.com` is text, not a filter called `http`. The
            // distinguishing marks are a URL's `//` and anything non-wordlike
            // in the name.
            let looks_like_filter = !name_lower.is_empty()
                && name_lower
                    .chars()
                    .all(|c| c.is_ascii_alphabetic() || c == '_' || c == '-')
                && !rest_preview.starts_with("//");
            if looks_like_filter && !FIELDS.contains(&name_lower.as_str()) {
                // Reporting beats silently searching for the literal text: a
                // mistyped filter would otherwise return nothing with no
                // explanation. Quoting the term searches for it literally.
                return Err(QueryError::UnknownField {
                    field: name_lower,
                    known: FIELDS.join(", "),
                });
            }
            if looks_like_filter {
                let mut rest: String = chars[colon_at + 1..i].iter().collect();
                if i < chars.len() && chars[i] == '"' {
                    let (quoted, next) = read_quoted(&chars, i)?;
                    rest.push_str(&quoted);
                    i = next;
                }
                let (op, value) = match rest.chars().next() {
                    Some('>') => (Comparison::Greater, rest[1..].to_owned()),
                    Some('<') => (Comparison::Less, rest[1..].to_owned()),
                    Some('=') => (Comparison::Equal, rest[1..].to_owned()),
                    _ => (Comparison::Equal, rest.clone()),
                };
                tokens.push(Token::Field {
                    name: name_lower,
                    op,
                    value: value.trim().to_owned(),
                    negated,
                });
                continue;
            }
        }

        if head.eq_ignore_ascii_case("or") && !negated {
            tokens.push(Token::Or);
            continue;
        }
        if head.eq_ignore_ascii_case("and") && !negated {
            // Implicit already; accept it so people can type what they mean.
            continue;
        }
        if head.eq_ignore_ascii_case("not") && !negated {
            negate_next = true;
            continue;
        }
        if !head.is_empty() {
            tokens.push(Token::Text {
                text: head,
                negated,
                phrase: false,
            });
        } else if i < chars.len() && chars[i] == '"' {
            // A run that ended at a quote with nothing before it.
            let (text, next) = read_quoted(&chars, i)?;
            i = next;
            if !text.trim().is_empty() {
                tokens.push(Token::Text {
                    text,
                    negated,
                    phrase: true,
                });
            }
        }
    }
    Ok(tokens)
}

/// Read a `"quoted"` run starting at `at`, returning its contents and the
/// index after the closing quote.
///
/// `\"` inside a phrase is a literal quotation mark, the convention every
/// shell and search box uses. (FTS5's own doubled-quote escape is an
/// implementation detail of the generated expression, not something a person
/// should have to type.)
fn read_quoted(chars: &[char], at: usize) -> Result<(String, usize), QueryError> {
    let mut out = String::new();
    let mut i = at + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' if chars.get(i + 1) == Some(&'"') => {
                out.push('"');
                i += 2;
            }
            '"' => return Ok((out, i + 1)),
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    Err(QueryError::UnbalancedQuote)
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn q(input: &str) -> Query {
        parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"))
    }

    #[test]
    fn an_empty_query_matches_everything() {
        let query = q("");
        assert!(query.is_empty());
        assert!(!query.needs_full_text());
        assert!(q("   ").is_empty());
    }

    #[test]
    fn bare_words_are_anded() {
        let query = q("lighthouse harbour");
        assert_eq!(query.fts.as_deref(), Some(r#""lighthouse" AND "harbour""#));
    }

    #[test]
    fn words_are_quoted_so_punctuation_is_not_syntax() {
        // `C++`, `read-later` and `a:b` must not break the FTS expression.
        assert_eq!(q("C++").fts.as_deref(), Some(r#""C++""#));
        assert_eq!(q("read-later").fts.as_deref(), Some(r#""read-later""#));
        assert_eq!(
            q(r#"say "hi" there"#).fts.as_deref(),
            Some(r#""say" AND "hi" AND "there""#)
        );
    }

    #[test]
    fn a_quoted_phrase_stays_one_phrase() {
        // SRCH-003.
        assert_eq!(
            q(r#""harbour mouth""#).fts.as_deref(),
            Some(r#""harbour mouth""#)
        );
    }

    #[test]
    fn a_quotation_mark_inside_a_phrase_can_be_escaped() {
        let query = q(r#""she said \"yes\"""#);
        // The phrase reaches FTS5 with its own doubled-quote escaping applied.
        assert_eq!(query.fts.as_deref(), Some(r#""she said ""yes""""#));
    }

    #[test]
    fn an_unbalanced_quote_is_reported() {
        assert_eq!(parse(r#""unclosed"#), Err(QueryError::UnbalancedQuote));
    }

    #[test]
    fn exclusions_are_accepted_in_both_spellings() {
        for input in ["lighthouse -draft", "lighthouse NOT draft"] {
            assert_eq!(
                q(input).fts.as_deref(),
                Some(r#""lighthouse" NOT "draft""#),
                "{input}"
            );
        }
    }

    #[test]
    fn a_hyphen_inside_a_word_is_not_an_exclusion() {
        assert_eq!(q("read-later").fts.as_deref(), Some(r#""read-later""#));
        assert!(q("read-later").none_tags.is_empty());
    }

    #[test]
    fn a_query_that_is_only_an_exclusion_is_still_valid() {
        // FTS5 cannot start an expression with NOT, so it needs a universe.
        let fts = q("-draft").fts.unwrap();
        assert_eq!(fts, r#"(* NOT "draft")"#);
    }

    #[test]
    fn or_groups_alternatives() {
        assert_eq!(
            q("history OR engineering").fts.as_deref(),
            Some(r#"("history" OR "engineering")"#)
        );
        assert_eq!(
            q("lighthouse history OR engineering").fts.as_deref(),
            Some(r#""lighthouse" AND ("history" OR "engineering")"#)
        );
    }

    #[test]
    fn or_is_case_insensitive_and_and_is_accepted_explicitly() {
        assert_eq!(q("a or b").fts, q("a OR b").fts);
        assert_eq!(q("a AND b").fts, q("a b").fts);
    }

    #[test]
    fn a_dangling_or_is_reported() {
        assert_eq!(parse("OR lighthouse"), Err(QueryError::DanglingOr));
        assert_eq!(parse("lighthouse OR"), Err(QueryError::DanglingOr));
    }

    #[test]
    fn prefix_searches_are_preserved() {
        assert_eq!(q("light*").fts.as_deref(), Some(r#""light"*"#));
    }

    #[test]
    fn tag_filters_stay_structured() {
        // They must respect hierarchy and provenance, which FTS cannot.
        let query = q("tag:lighthouse tag:history");
        assert_eq!(query.all_tags, vec!["lighthouse", "history"]);
        assert_eq!(query.fts, None);
        assert!(!query.needs_full_text(), "a tag-only query skips the index");
    }

    #[test]
    fn negated_and_alternative_tag_filters_are_separated() {
        // TAG-003.
        let query = q("tag:a -tag:b");
        assert_eq!(query.all_tags, vec!["a"]);
        assert_eq!(query.none_tags, vec!["b"]);

        let either = q("tag:a OR tag:b");
        assert_eq!(either.any_tags, vec!["a", "b"]);
        assert!(either.all_tags.is_empty());
    }

    #[test]
    fn hierarchical_tags_survive_parsing() {
        assert_eq!(q("tag:project/marius").all_tags, vec!["project/marius"]);
    }

    #[test]
    fn multi_word_filter_values_can_be_quoted() {
        assert_eq!(q(r#"tag:"two words""#).all_tags, vec!["two words"]);
        assert_eq!(q(r#"vault:"My Work""#).vaults, vec!["My Work"]);
    }

    #[test]
    fn path_vault_and_type_filters_parse() {
        // SRCH-002.
        let query = q("path:research/ vault:work type:email");
        assert_eq!(query.paths, vec!["research"]);
        assert_eq!(query.vaults, vec!["work"]);
        assert_eq!(query.kinds, vec![DocumentKind::Email]);
    }

    #[test]
    fn an_unknown_document_type_is_reported_with_help() {
        let err = parse("type:spreadsheet").unwrap_err();
        assert!(matches!(err, QueryError::BadKind { .. }));
        assert!(err.to_string().contains("markdown"));
    }

    #[test]
    fn field_scoped_text_is_handed_to_fts_columns() {
        assert_eq!(q("title:pharos").fts.as_deref(), Some(r#"title:"pharos""#));
        assert_eq!(
            q("alias:pharos").fts.as_deref(),
            Some(r#"aliases:"pharos""#)
        );
        assert_eq!(
            q("-title:draft").fts.as_deref(),
            Some(r#"(* NOT title:"draft")"#)
        );
    }

    #[test]
    fn date_comparisons_parse() {
        let after = q("modified:>2024-01-01");
        assert!(after.modified_after.is_some());
        assert!(after.modified_before.is_none());

        let before = q("created:<2024-06-30");
        assert!(before.created_before.is_some());
        assert!(before.created_after.is_none());
    }

    #[test]
    fn a_bare_date_means_that_whole_day() {
        let query = q("modified:2024-03-05");
        let after = query.modified_after.unwrap();
        let before = query.modified_before.unwrap();
        assert!(before - after > time::Duration::hours(23));
        assert!(before - after < time::Duration::hours(25));
    }

    #[test]
    fn relative_dates_are_understood() {
        for value in ["today", "yesterday", "week", "month", "year"] {
            assert!(
                q(&format!("modified:>{value}")).modified_after.is_some(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_malformed_date_is_reported_not_ignored() {
        let err = parse("modified:>last-tuesday").unwrap_err();
        assert_eq!(
            err,
            QueryError::BadDate {
                value: "last-tuesday".into()
            }
        );
    }

    #[test]
    fn archived_and_generated_are_filterable_in_both_directions() {
        // KB-012 and AI-012.
        assert_eq!(q("is:archived").archived, Some(true));
        assert_eq!(q("-is:archived").archived, Some(false));
        assert_eq!(q("is:generated").generated, Some(true));
        assert_eq!(q("-is:generated").generated, Some(false));
        // And neither is filtered by default.
        assert_eq!(q("lighthouse").archived, None);
        assert_eq!(q("lighthouse").generated, None);
    }

    #[test]
    fn an_unknown_state_is_reported() {
        assert_eq!(
            parse("is:purple").unwrap_err(),
            QueryError::BadState {
                value: "purple".into()
            }
        );
    }

    #[test]
    fn an_empty_filter_value_is_reported_rather_than_dropped() {
        assert_eq!(
            parse("tag:").unwrap_err(),
            QueryError::MissingValue {
                field: "tag".into()
            }
        );
    }

    #[test]
    fn an_unknown_field_is_reported_with_the_known_ones() {
        let err = parse("colour:blue").unwrap_err();
        match &err {
            QueryError::UnknownField { field, known } => {
                assert_eq!(field, "colour");
                assert!(known.contains("tag"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_url_is_text_not_a_filter_called_http() {
        let query = q("http://example.com");
        assert_eq!(query.fts.as_deref(), Some(r#""http://example.com""#));
        assert!(query.vaults.is_empty());
    }

    #[test]
    fn a_mistyped_filter_is_reported_and_quoting_searches_literally() {
        // Silently searching for the literal text would return nothing with no
        // explanation, which is the worse failure.
        assert!(matches!(
            parse("colour:blue"),
            Err(QueryError::UnknownField { .. })
        ));
        // Quoting is the escape hatch for searching such text in a note.
        assert_eq!(
            q(r#""colour:blue""#).fts.as_deref(),
            Some(r#""colour:blue""#)
        );
    }

    #[test]
    fn a_realistic_mixed_query_parses_completely() {
        let query = q(
            r#"lighthouse "harbour mouth" tag:history -tag:draft path:research vault:work modified:>2024-01-01 -is:generated"#,
        );
        assert_eq!(
            query.fts.as_deref(),
            Some(r#""lighthouse" AND "harbour mouth""#)
        );
        assert_eq!(query.all_tags, vec!["history"]);
        assert_eq!(query.none_tags, vec!["draft"]);
        assert_eq!(query.paths, vec!["research"]);
        assert_eq!(query.vaults, vec!["work"]);
        assert!(query.modified_after.is_some());
        assert_eq!(query.generated, Some(false));
        assert!(!query.is_empty());
    }

    #[test]
    fn the_raw_text_is_kept_for_saved_searches() {
        // SRCH-006.
        let query = q("  tag:history lighthouse  ");
        assert_eq!(query.raw, "tag:history lighthouse");
        assert_eq!(query.to_string(), "tag:history lighthouse");
    }

    #[test]
    fn non_latin_queries_parse() {
        assert_eq!(q("灯台").fts.as_deref(), Some(r#""灯台""#));
        assert_eq!(q("tag:φάρος").all_tags, vec!["φάρος"]);
    }
}
