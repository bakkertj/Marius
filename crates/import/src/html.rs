//! HTML → Markdown (IM-008).

use crate::{yaml_quote, ImportError, ImportedNote};

/// Convert an HTML document into a note. `source` names where it came
/// from (a path or URL) and lands in frontmatter; the title is the
//  document's own <title> when it has one.
pub fn import(
    html: &str,
    fallback_title: &str,
    source: Option<&str>,
) -> Result<ImportedNote, ImportError> {
    let markdown = htmd::convert(html).map_err(|e| ImportError::Html(e.to_string()))?;
    let title = extract_title(html).unwrap_or_else(|| fallback_title.to_owned());

    let mut body = String::from("---\n");
    body.push_str(&format!("title: {}\n", yaml_quote(&title)));
    if let Some(source) = source {
        body.push_str(&format!("source: {}\n", yaml_quote(source)));
    }
    body.push_str("imported: html\n---\n\n");
    body.push_str(markdown.trim());
    body.push('\n');

    Ok(ImportedNote {
        file_name: format!("{title}.md"),
        body,
        attachments: Vec::new(),
    })
}

/// The document's own <title>, decoded just enough for real pages.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let open = lower.find("<title")?;
    let start = lower[open..].find('>')? + open + 1;
    let end = lower[start..].find("</title>")? + start;
    let raw = html[start..end].trim();
    if raw.is_empty() {
        return None;
    }
    let decoded = raw
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace(['\n', '\r'], " ");
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_becomes_a_frontmattered_markdown_note() {
        let html = r#"<html><head><title>Lighthouse &amp; Harbour</title></head>
            <body><h1>Pharos</h1><p>It stood for <strong>sixteen</strong> centuries.</p>
            <ul><li>limestone</li><li>lead</li></ul></body></html>"#;
        let note = import(html, "fallback", Some("https://example.org/pharos")).unwrap();
        assert_eq!(note.file_name, "Lighthouse & Harbour.md");
        assert!(note.body.starts_with("---\n"));
        assert!(note.body.contains(r#"title: "Lighthouse & Harbour""#));
        assert!(note
            .body
            .contains(r#"source: "https://example.org/pharos""#));
        assert!(note.body.contains("# Pharos"));
        assert!(note.body.contains("**sixteen**"));
        assert!(note.body.contains("limestone"));
    }

    #[test]
    fn a_titleless_page_uses_the_fallback() {
        let note = import("<p>just text</p>", "clipping", None).unwrap();
        assert_eq!(note.file_name, "clipping.md");
        assert!(note.body.contains("just text"));
    }
}
