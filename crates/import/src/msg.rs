//! Outlook `.msg` import (IM-009's Outlook half).
//!
//! A `.msg` is an OLE compound file ([MS-OXMSG]): MAPI properties live
//! in streams named `__substg1.0_IIIITTTT` (`IIII` the property id,
//! `TTTT` the type — 001F UTF-16, 001E 8-bit, 0102 binary), attachments
//! in `__attach_version1.0_#NNNNNNNN` storages. This reads exactly the
//! properties a note needs — subject, sender, body, identifiers and the
//! attachments — because work Outlook offers only "save as .msg", and
//! that must be enough to get a message into the vault.

use std::io::{Cursor, Read, Seek};

use crate::{yaml_quote, Attachment, ImportError, ImportedNote};

/// MAPI property ids ([MS-OXPROPS]).
const SUBJECT: u16 = 0x0037;
const SENDER_NAME: u16 = 0x0C1A;
const SENDER_ADDRESS: u16 = 0x0C1F;
const SENDER_SMTP: u16 = 0x5D01;
const DISPLAY_TO: u16 = 0x0E04;
const MESSAGE_ID: u16 = 0x1035;
const IN_REPLY_TO: u16 = 0x1042;
const BODY_PLAIN: u16 = 0x1000;
const ATTACH_LONG_NAME: u16 = 0x3707;
const ATTACH_SHORT_NAME: u16 = 0x3704;
/// PR_CLIENT_SUBMIT_TIME, a PT_SYSTIME in the fixed-property stream.
const SUBMIT_TIME_TAG: u32 = 0x0039_0040;

/// One Outlook `.msg` into one note, attachments included.
pub fn import_msg(raw: &[u8]) -> Result<ImportedNote, ImportError> {
    let mut file =
        cfb::CompoundFile::open(Cursor::new(raw)).map_err(|_| ImportError::UnparseableMessage)?;

    let subject = string_prop(&mut file, "", SUBJECT)
        .unwrap_or_else(|| "Untitled message".to_owned())
        .trim()
        .to_owned();
    let title = if subject.is_empty() {
        "Untitled message".to_owned()
    } else {
        subject
    };

    let mut front = String::from("---\n");
    front.push_str(&format!("title: {}\n", yaml_quote(&title)));
    let name = string_prop(&mut file, "", SENDER_NAME).unwrap_or_default();
    let address = string_prop(&mut file, "", SENDER_SMTP)
        .or_else(|| string_prop(&mut file, "", SENDER_ADDRESS))
        .filter(|a| a.contains('@'))
        .unwrap_or_default();
    let from = match (name.is_empty(), address.is_empty()) {
        (false, false) => format!("{name} <{address}>"),
        (false, true) => name,
        (true, false) => address,
        (true, true) => String::new(),
    };
    if !from.is_empty() {
        front.push_str(&format!("from: {}\n", yaml_quote(&from)));
    }
    if let Some(to) = string_prop(&mut file, "", DISPLAY_TO) {
        if !to.trim().is_empty() {
            front.push_str(&format!("to: {}\n", yaml_quote(to.trim())));
        }
    }
    if let Some(date) = submit_time(&mut file) {
        front.push_str(&format!("date: {date}\n"));
    }
    if let Some(id) = string_prop(&mut file, "", MESSAGE_ID) {
        front.push_str(&format!("message_id: {}\n", yaml_quote(&id)));
    }
    if let Some(reply) = string_prop(&mut file, "", IN_REPLY_TO) {
        front.push_str(&format!("in_reply_to: {}\n", yaml_quote(&reply)));
    }
    front.push_str("imported: msg\n");

    // Prefer PR_HTML (binary) converted to Markdown, like .eml import;
    // fall back to the plain-text body. Headers alone still import.
    let body_markdown = match stream_bytes(&mut file, "/__substg1.0_10130102") {
        Some(html) => crate::html_to_markdown(&String::from_utf8_lossy(&html))
            .map_err(|e| ImportError::Html(e.to_string()))?
            .trim()
            .to_owned(),
        None => string_prop(&mut file, "", BODY_PLAIN)
            .map(|text| text.trim().to_owned())
            .unwrap_or_default(),
    };

    // Attachment storages. Collect the names first: the iterator borrows
    // the compound file that reading each attachment also needs.
    let storages: Vec<String> = file
        .read_storage("/")
        .map_err(|_| ImportError::UnparseableMessage)?
        .filter(|entry| entry.is_storage() && entry.name().starts_with("__attach_version1.0_"))
        .map(|entry| format!("/{}", entry.name()))
        .collect();
    let mut attachments = Vec::new();
    for (index, storage) in storages.iter().enumerate() {
        // No data stream means an embedded message or OLE object; those
        // are beyond a note's reach and are skipped, not errors.
        let Some(bytes) = stream_bytes(&mut file, &format!("{storage}/__substg1.0_37010102"))
        else {
            continue;
        };
        let file_name = string_prop(&mut file, storage, ATTACH_LONG_NAME)
            .or_else(|| string_prop(&mut file, storage, ATTACH_SHORT_NAME))
            .unwrap_or_else(|| format!("attachment-{}", index + 1));
        attachments.push(Attachment { file_name, bytes });
    }
    if !attachments.is_empty() {
        front.push_str("attachments:\n");
        for attachment in &attachments {
            front.push_str(&format!("  - {}\n", yaml_quote(&attachment.file_name)));
        }
    }
    front.push_str("---\n\n");

    let mut body = front;
    body.push_str(&format!("# {title}\n\n"));
    body.push_str(&body_markdown);
    body.push('\n');

    Ok(ImportedNote {
        file_name: format!("{title}.md"),
        body,
        attachments,
    })
}

/// A whole stream, or `None` when the message simply does not carry it.
fn stream_bytes<F: Read + Seek>(file: &mut cfb::CompoundFile<F>, path: &str) -> Option<Vec<u8>> {
    let mut stream = file.open_stream(path).ok()?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// A string property: the UTF-16 variant (001F) first, then 8-bit (001E).
fn string_prop<F: Read + Seek>(
    file: &mut cfb::CompoundFile<F>,
    storage: &str,
    id: u16,
) -> Option<String> {
    if let Some(bytes) = stream_bytes(file, &format!("{storage}/__substg1.0_{id:04X}001F")) {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        return Some(
            String::from_utf16_lossy(&units)
                .trim_end_matches('\0')
                .to_owned(),
        );
    }
    stream_bytes(file, &format!("{storage}/__substg1.0_{id:04X}001E")).map(|bytes| {
        String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .to_owned()
    })
}

/// PR_CLIENT_SUBMIT_TIME from the fixed-width property stream: a 32-byte
/// header, then 16-byte records of tag, flags and an 8-byte value — here
/// a FILETIME (100 ns ticks since 1601), printed as RFC 3339.
fn submit_time<F: Read + Seek>(file: &mut cfb::CompoundFile<F>) -> Option<String> {
    let bytes = stream_bytes(file, "/__properties_version1.0")?;
    for record in bytes.get(32..)?.as_chunks::<16>().0 {
        let tag = u32::from_le_bytes([record[0], record[1], record[2], record[3]]);
        if tag != SUBMIT_TIME_TAG {
            continue;
        }
        let ticks = u64::from_le_bytes(record[8..16].try_into().ok()?);
        let unix = (ticks / 10_000_000) as i64 - 11_644_473_600;
        let moment = time::OffsetDateTime::from_unix_timestamp(unix).ok()?;
        return moment
            .format(&time::format_description::well_known::Rfc3339)
            .ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Build a real compound file the way Outlook lays one out — the cfb
    /// crate writes as well as it reads, so the fixture is honest.
    struct MsgBuilder {
        file: cfb::CompoundFile<Cursor<Vec<u8>>>,
        attachment_count: usize,
    }

    impl MsgBuilder {
        fn new() -> Self {
            Self {
                file: cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap(),
                attachment_count: 0,
            }
        }

        fn unicode(mut self, id_and_type: &str, value: &str) -> Self {
            let mut stream = self
                .file
                .create_stream(format!("/__substg1.0_{id_and_type}"))
                .unwrap();
            if id_and_type.ends_with("001F") {
                for unit in value.encode_utf16() {
                    stream.write_all(&unit.to_le_bytes()).unwrap();
                }
            } else {
                stream.write_all(value.as_bytes()).unwrap();
            }
            drop(stream);
            self
        }

        fn submit_time(mut self, ticks: u64) -> Self {
            let mut bytes = vec![0u8; 32];
            bytes.extend_from_slice(&SUBMIT_TIME_TAG.to_le_bytes());
            bytes.extend_from_slice(&[0u8; 4]);
            bytes.extend_from_slice(&ticks.to_le_bytes());
            let mut stream = self.file.create_stream("/__properties_version1.0").unwrap();
            stream.write_all(&bytes).unwrap();
            drop(stream);
            self
        }

        fn attachment(mut self, name: &str, bytes: &[u8]) -> Self {
            let storage = format!("/__attach_version1.0_#{:08X}", self.attachment_count);
            self.attachment_count += 1;
            self.file.create_storage(&storage).unwrap();
            let mut data = self
                .file
                .create_stream(format!("{storage}/__substg1.0_37010102"))
                .unwrap();
            data.write_all(bytes).unwrap();
            drop(data);
            let mut named = self
                .file
                .create_stream(format!("{storage}/__substg1.0_3707001F"))
                .unwrap();
            for unit in name.encode_utf16() {
                named.write_all(&unit.to_le_bytes()).unwrap();
            }
            drop(named);
            self
        }

        fn build(self) -> Vec<u8> {
            self.file.into_inner().into_inner()
        }
    }

    #[test]
    fn an_outlook_message_arrives_with_headers_body_and_attachments() {
        let raw = MsgBuilder::new()
            .unicode("0037001F", "Quarterly soundings")
            .unicode("0C1A001F", "Marius Michel")
            .unicode("5D01001F", "pasha@phares.example")
            .unicode("0E04001F", "harbour-board@ville.example")
            .unicode("1035001F", "<q3@phares.example>")
            .unicode(
                "1000001F",
                "The depths are attached.\nFour charts this quarter.",
            )
            .submit_time(132_444_736_000_000_000) // unix 1 600 000 000
            .attachment("chart-west.pdf", b"%PDF-1.4 fake")
            .attachment("chart-east.pdf", b"%PDF-1.4 fake too")
            .build();

        let note = import_msg(&raw).unwrap();
        assert_eq!(note.file_name, "Quarterly soundings.md");
        assert!(
            note.body.contains("title: \"Quarterly soundings\""),
            "{}",
            note.body
        );
        assert!(
            note.body
                .contains("from: \"Marius Michel <pasha@phares.example>\""),
            "{}",
            note.body
        );
        assert!(note.body.contains("to: \"harbour-board@ville.example\""));
        assert!(note.body.contains("message_id: \"<q3@phares.example>\""));
        assert!(
            note.body.contains("date: 2020-09-13T12:26:40Z"),
            "{}",
            note.body
        );
        assert!(note.body.contains("imported: msg"));
        assert!(note.body.contains("Four charts this quarter."));
        assert_eq!(note.attachments.len(), 2);
        assert_eq!(note.attachments[0].file_name, "chart-west.pdf");
        assert_eq!(note.attachments[1].bytes, b"%PDF-1.4 fake too");
    }

    #[test]
    fn an_html_body_beats_the_plain_one_and_becomes_markdown() {
        let mut builder = MsgBuilder::new().unicode("0037001F", "Styled");
        let mut html = builder.file.create_stream("/__substg1.0_10130102").unwrap();
        html.write_all(b"<html><body><p>The <em>western</em> channel.</p></body></html>")
            .unwrap();
        drop(html);
        let raw = builder.unicode("1000001F", "plain fallback").build();

        let note = import_msg(&raw).unwrap();
        assert!(
            note.body.contains("_western_") || note.body.contains("*western*"),
            "{}",
            note.body
        );
        assert!(!note.body.contains("plain fallback"));
    }

    #[test]
    fn garbage_is_an_error_and_a_bare_message_still_imports() {
        assert!(matches!(
            import_msg(b"not ole at all"),
            Err(ImportError::UnparseableMessage)
        ));
        let raw = MsgBuilder::new().build();
        let note = import_msg(&raw).unwrap();
        assert_eq!(note.file_name, "Untitled message.md");
        assert!(note.attachments.is_empty());
    }
}
