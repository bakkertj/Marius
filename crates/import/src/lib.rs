//! # Importers
//!
//! Turn content from outside the vault into ordinary Markdown notes
//! (IM-008 HTML, IM-009 `.eml`, IM-011 `.mbox`). An importer is a pure
//! function from bytes to [`ImportedNote`]s; writing files, indexing and
//! collision handling belong to `marius-core`, which is also where IM-005
//! (index on import) happens for free.
//!
//! The principle throughout: imported metadata lands in frontmatter,
//! where the indexer already treats it as user-authored (IM-004,
//! `Provenance::Imported`), and nothing of the original is silently
//! dropped — what cannot be converted is named, not discarded.

#![forbid(unsafe_code)]

pub mod documents;
pub mod email;
pub mod html;

/// One note ready to be written into a vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedNote {
    /// Suggested file name (no directories), not yet sanitised for
    /// Windows — the core applies `marius_model::sanitize` when it
    /// places the file.
    pub file_name: String,
    /// Complete file content, frontmatter included.
    pub body: String,
    /// Binary attachments to place beside the note (IM-009).
    pub attachments: Vec<Attachment>,
}

/// A file carried by an import (an email attachment, typically).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("cannot parse the message")]
    UnparseableMessage,
    #[error("cannot convert the HTML: {0}")]
    Html(String),
    #[error("{0}")]
    Document(String),
}

/// HTML → Markdown with the chrome stripped: styles, scripts and
/// navigation are markup, not knowledge.
pub(crate) fn html_to_markdown(html: &str) -> std::io::Result<String> {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["style", "script", "noscript", "head", "nav", "iframe"])
        .build()
        .convert(html)
}

/// Escape a string for a double-quoted YAML scalar.
fn yaml_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}
