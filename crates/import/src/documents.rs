//! PDF, Word and EPUB import (IM-008, IM-012's EPUB half).
//!
//! The goal is searchable text with honest provenance, not perfect
//! fidelity: a PDF's layout, a DOCX's styles and an EPUB's typography
//! are deliberately flattened into plain Markdown the indexer already
//! understands. The source file's name rides along in frontmatter.

use std::io::Read;

use crate::{yaml_quote, ImportError, ImportedNote};

/// Extract a PDF's text (IM-008). `pdf-extract` is known to panic on
/// exotic files, so the call is fenced: a crash in the extractor is an
/// import error, never an application abort.
pub fn import_pdf(bytes: &[u8], fallback_title: &str) -> Result<ImportedNote, ImportError> {
    let bytes = bytes.to_vec();
    let extracted = std::panic::catch_unwind(move || pdf_extract::extract_text_from_mem(&bytes))
        .map_err(|_| ImportError::Document("the PDF extractor crashed on this file".into()))?
        .map_err(|e| ImportError::Document(format!("cannot read the PDF: {e}")))?;

    let text = normalise_extracted(&extracted);
    if text.trim().is_empty() {
        return Err(ImportError::Document(
            "the PDF contains no extractable text (a scan needs OCR, which is not built)".into(),
        ));
    }
    Ok(note(fallback_title, "pdf", &text))
}

/// Extract a `.docx`'s paragraphs (IM-008). A DOCX is a zip holding
/// `word/document.xml`; paragraphs are `<w:p>`, text runs `<w:t>`.
pub fn import_docx(bytes: &[u8], fallback_title: &str) -> Result<ImportedNote, ImportError> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| ImportError::Document(format!("not a Word document: {e}")))?;
    let mut xml = String::new();
    archive
        .by_name("word/document.xml")
        .map_err(|e| ImportError::Document(format!("no word/document.xml: {e}")))?
        .read_to_string(&mut xml)
        .map_err(|e| ImportError::Document(e.to_string()))?;

    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut out = String::new();
    let mut paragraph = String::new();
    // Only text inside <w:t> is document text; everything between
    // elements is the XML's own pretty-printing.
    let mut in_text_run = false;
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) if e.local_name().as_ref() == b"p" => {
                paragraph.clear();
            }
            Ok(quick_xml::events::Event::Start(e)) if e.local_name().as_ref() == b"t" => {
                in_text_run = true;
            }
            Ok(quick_xml::events::Event::End(e)) if e.local_name().as_ref() == b"t" => {
                in_text_run = false;
            }
            Ok(quick_xml::events::Event::End(e)) if e.local_name().as_ref() == b"p" => {
                let trimmed = paragraph.trim();
                if !trimmed.is_empty() {
                    out.push_str(trimmed);
                    out.push_str("\n\n");
                }
            }
            Ok(quick_xml::events::Event::Text(t)) if in_text_run => {
                paragraph.push_str(&t.unescape().unwrap_or_default());
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => {
                return Err(ImportError::Document(format!(
                    "malformed document.xml: {e}"
                )))
            }
            _ => {}
        }
    }
    if out.trim().is_empty() {
        return Err(ImportError::Document(
            "the document contains no text".into(),
        ));
    }
    Ok(note(fallback_title, "docx", out.trim()))
}

/// Flatten an EPUB into one note (IM-012's EPUB half): each XHTML
/// chapter converted to Markdown, in archive order.
pub fn import_epub(bytes: &[u8], fallback_title: &str) -> Result<ImportedNote, ImportError> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| ImportError::Document(format!("not an EPUB: {e}")))?;

    // Title from the OPF package, if one declares it.
    let mut title = fallback_title.to_owned();
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_owned()))
        .collect();
    if let Some(opf) = names.iter().find(|n| n.ends_with(".opf")) {
        let mut xml = String::new();
        if archive
            .by_name(opf)
            .ok()
            .and_then(|mut f| f.read_to_string(&mut xml).ok())
            .is_some()
        {
            if let Some(found) = between(&xml, "<dc:title", "</dc:title>")
                .and_then(|t| t.split('>').nth(1).map(str::trim).map(str::to_owned))
            {
                if !found.is_empty() {
                    title = found;
                }
            }
        }
    }

    let mut body = String::new();
    for name in &names {
        if !(name.ends_with(".xhtml") || name.ends_with(".html") || name.ends_with(".htm")) {
            continue;
        }
        let mut html = String::new();
        if archive
            .by_name(name)
            .ok()
            .and_then(|mut f| f.read_to_string(&mut html).ok())
            .is_none()
        {
            continue;
        }
        if let Ok(markdown) = crate::html_to_markdown(&html) {
            let markdown = markdown.trim();
            if !markdown.is_empty() {
                body.push_str(markdown);
                body.push_str("\n\n");
            }
        }
    }
    if body.trim().is_empty() {
        return Err(ImportError::Document(
            "the EPUB contains no chapters".into(),
        ));
    }
    let mut note = note(&title, "epub", body.trim());
    note.file_name = format!("{title}.md");
    Ok(note)
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)?;
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

/// PDF extractors emit hard line breaks mid-sentence; join single
/// newlines, keep blank-line paragraph breaks.
fn normalise_extracted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for paragraph in text.split("\n\n") {
        let joined = paragraph
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            out.push_str(&joined);
            out.push_str("\n\n");
        }
    }
    out.trim().to_owned()
}

fn note(title: &str, kind: &str, body: &str) -> ImportedNote {
    let mut text = String::from("---\n");
    text.push_str(&format!("title: {}\n", yaml_quote(title)));
    text.push_str(&format!("imported: {kind}\n---\n\n# {title}\n\n{body}\n"));
    ImportedNote {
        file_name: format!("{title}.md"),
        body: text,
        attachments: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal, valid, uncompressed one-page PDF with real text.
    fn tiny_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let mut objects = Vec::new();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_owned());
        objects.push("<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned());
        objects.push(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_owned(),
        );
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned());

        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            write!(out, "{} 0 obj\n{object}\nendobj\n", index + 1).unwrap();
        }
        let xref_at = out.len();
        write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).unwrap();
        for offset in &offsets {
            write!(out, "{offset:010} 00000 n ").and_then(|()| writeln!(out)).unwrap();
        }
        write!(
            out,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF",
            objects.len() + 1
        )
        .unwrap();
        out
    }

    fn tiny_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default();
            for (name, content) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(content.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    #[test]
    fn a_pdf_becomes_searchable_text_and_a_scan_is_refused_honestly() {
        let note = import_pdf(&tiny_pdf("Soundings at the harbour entrance."), "survey").unwrap();
        assert!(note.body.contains("Soundings at the harbour entrance."));
        assert!(note.body.contains("imported: pdf"));

        // No text layer → a named refusal, not an empty note.
        let error = import_pdf(&tiny_pdf(""), "scan").unwrap_err();
        assert!(error.to_string().contains("OCR"), "{error}");
        // Garbage is an error, never a crash (the extractor is fenced).
        assert!(import_pdf(b"not a pdf at all", "x").is_err());
    }

    #[test]
    fn a_docx_yields_its_paragraphs() {
        let document_xml = r#"<?xml version="1.0"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:t>The lighthouse committee met</w:t></w:r>
                     <w:r><w:t xml:space="preserve"> on Tuesday.</w:t></w:r></w:p>
                <w:p><w:r><w:t>Dredging was approved.</w:t></w:r></w:p>
              </w:body>
            </w:document>"#;
        let bytes = tiny_zip(&[("word/document.xml", document_xml)]);
        let note = import_docx(&bytes, "minutes").unwrap();
        assert!(note
            .body
            .contains("The lighthouse committee met on Tuesday."));
        assert!(note.body.contains("Dredging was approved."));
        assert!(import_docx(b"junk", "x").is_err());
    }

    #[test]
    fn an_epub_flattens_its_chapters_under_its_own_title() {
        let bytes = tiny_zip(&[
            (
                "content.opf",
                r#"<package xmlns:dc="x"><metadata><dc:title>The Lighthouse Builders</dc:title></metadata></package>"#,
            ),
            (
                "ch1.xhtml",
                "<html><body><h1>One</h1><p>Marius went to sea.</p></body></html>",
            ),
            (
                "ch2.xhtml",
                "<html><body><h1>Two</h1><p>He came back an engineer.</p></body></html>",
            ),
        ]);
        let note = import_epub(&bytes, "fallback").unwrap();
        assert_eq!(note.file_name, "The Lighthouse Builders.md");
        assert!(note.body.contains("Marius went to sea."));
        assert!(note.body.contains("He came back an engineer."));
    }
}
