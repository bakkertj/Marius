//! Email import: single `.eml` messages (IM-009) and `.mbox` archives
//! (IM-011), headers and attachments included.
//!
//! The message's own headers become frontmatter: subject as title, from,
//! to, date, and the message/thread identifiers — `message_id` and
//! `in_reply_to` are what let threads be reconstructed later (IM-011),
//! so they are kept even though no view uses them yet.

use mail_parser::{MessageParser, MimeHeaders};

use crate::{yaml_quote, Attachment, ImportError, ImportedNote};

/// One `.eml` message into one note.
pub fn import_eml(raw: &[u8]) -> Result<ImportedNote, ImportError> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or(ImportError::UnparseableMessage)?;

    let subject = message
        .subject()
        .map(str::to_owned)
        .unwrap_or_else(|| "Untitled message".to_owned());

    let mut front = String::from("---\n");
    front.push_str(&format!("title: {}\n", yaml_quote(&subject)));
    if let Some(from) = message.from().and_then(|a| a.first()) {
        let display = match (from.name(), from.address()) {
            (Some(name), Some(address)) => format!("{name} <{address}>"),
            (_, Some(address)) => address.to_owned(),
            (Some(name), None) => name.to_owned(),
            _ => String::new(),
        };
        if !display.is_empty() {
            front.push_str(&format!("from: {}\n", yaml_quote(&display)));
        }
    }
    if let Some(to) = message
        .to()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
    {
        front.push_str(&format!("to: {}\n", yaml_quote(to)));
    }
    if let Some(date) = message.date() {
        front.push_str(&format!("date: {}\n", date.to_rfc3339()));
    }
    if let Some(id) = message.message_id() {
        front.push_str(&format!("message_id: {}\n", yaml_quote(id)));
    }
    if let Some(reply) = message.in_reply_to().as_text() {
        front.push_str(&format!("in_reply_to: {}\n", yaml_quote(reply)));
    }
    front.push_str("imported: eml\n");

    // Prefer the HTML body converted to Markdown — it keeps emphasis,
    // lists and links that the auto-derived text rendition flattens —
    // and fall back to plain text. A message with neither still
    // imports: the headers alone are knowledge.
    let body_markdown = match message.body_html(0) {
        Some(html) => htmd::convert(&html)
            .map_err(|e| ImportError::Html(e.to_string()))?
            .trim()
            .to_owned(),
        None => message
            .body_text(0)
            .map(|text| text.trim().to_owned())
            .unwrap_or_default(),
    };

    let mut attachments = Vec::new();
    for (index, part) in message.attachments().enumerate() {
        let name = part
            .attachment_name()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("attachment-{}", index + 1));
        attachments.push(Attachment {
            file_name: name,
            bytes: part.contents().to_vec(),
        });
    }
    if !attachments.is_empty() {
        front.push_str("attachments:\n");
        for attachment in &attachments {
            front.push_str(&format!("  - {}\n", yaml_quote(&attachment.file_name)));
        }
    }
    front.push_str("---\n\n");

    let mut body = front;
    body.push_str(&format!("# {subject}\n\n"));
    body.push_str(&body_markdown);
    body.push('\n');

    Ok(ImportedNote {
        file_name: format!("{subject}.md"),
        body,
        attachments,
    })
}

/// Every message of an `.mbox` archive, in order. A message that cannot
/// be parsed is skipped and counted, never silently dropped.
pub fn import_mbox(raw: &[u8]) -> (Vec<ImportedNote>, usize) {
    let mut notes = Vec::new();
    let mut failed = 0;
    for message in mail_parser::mailbox::mbox::MessageIterator::new(std::io::Cursor::new(raw)) {
        let Ok(message) = message else {
            failed += 1;
            continue;
        };
        match import_eml(message.contents()) {
            Ok(note) => notes.push(note),
            Err(_) => failed += 1,
        }
    }
    (notes, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EML: &str = "From: Marius Michel <pasha@phares.example>\r\n\
        To: office@phares.example\r\n\
        Subject: Harbour survey, 1869\r\n\
        Date: Tue, 1 Jun 1869 10:00:00 +0200\r\n\
        Message-ID: <survey-1@phares.example>\r\n\
        MIME-Version: 1.0\r\n\
        Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
        --b\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\r\n\
        Soundings attached. The eastern basin silts.\r\n\
        --b\r\n\
        Content-Type: application/octet-stream; name=\"soundings.csv\"\r\n\
        Content-Disposition: attachment; filename=\"soundings.csv\"\r\n\r\n\
        depth,mark\r\n7,buoy\r\n\
        --b--\r\n";

    #[test]
    fn an_eml_keeps_headers_body_and_attachments() {
        let note = import_eml(EML.as_bytes()).unwrap();
        assert_eq!(note.file_name, "Harbour survey, 1869.md");
        assert!(note.body.contains(r#"title: "Harbour survey, 1869""#));
        assert!(note
            .body
            .contains(r#"from: "Marius Michel <pasha@phares.example>""#));
        assert!(note.body.contains("date: 1869-06-01T10:00:00+02:00"));
        assert!(note
            .body
            .contains(r#"message_id: "survey-1@phares.example""#));
        assert!(note.body.contains("The eastern basin silts."));
        assert_eq!(note.attachments.len(), 1);
        assert_eq!(note.attachments[0].file_name, "soundings.csv");
        assert!(note.attachments[0].bytes.starts_with(b"depth,mark"));
        assert!(note.body.contains("- \"soundings.csv\""));
    }

    #[test]
    fn an_mbox_splits_into_messages_and_keeps_thread_identifiers() {
        let reply = "From: office@phares.example\r\n\
            Subject: Re: Harbour survey, 1869\r\n\
            Message-ID: <survey-2@phares.example>\r\n\
            In-Reply-To: <survey-1@phares.example>\r\n\
            Content-Type: text/plain\r\n\r\n\
            Noted; dredging approved.\r\n";
        let mbox = format!(
            "From pasha@phares.example Tue Jun  1 10:00:00 1869\n{}\nFrom office@phares.example Wed Jun  2 09:00:00 1869\n{}\n",
            EML.replace("\r\n", "\n"),
            reply.replace("\r\n", "\n"),
        );
        let (notes, failed) = import_mbox(mbox.as_bytes());
        assert_eq!(failed, 0);
        assert_eq!(notes.len(), 2);
        assert!(notes[1]
            .body
            .contains(r#"in_reply_to: "survey-1@phares.example""#));
        assert!(notes[1].body.contains("dredging approved"));
    }

    #[test]
    fn an_html_only_message_converts_its_body() {
        let eml = "From: a@b.c\r\nSubject: HTML only\r\n\
            Content-Type: text/html; charset=utf-8\r\n\r\n\
            <p>Hello <strong>world</strong></p>\r\n";
        let note = import_eml(eml.as_bytes()).unwrap();
        assert!(note.body.contains("Hello **world**"));
    }
}
