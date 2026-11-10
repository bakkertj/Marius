//! Importing from outside the vault (IM-005/008/009/011), end to end:
//! real files in, real indexed notes out.

use marius_core::{Marius, SearchOptions};
use marius_model::VaultPath;

struct Harness {
    app: Marius,
    vault: marius_model::Vault,
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
    inbox: tempfile::TempDir,
}

fn harness() -> Harness {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let inbox = tempfile::tempdir().unwrap();
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("I", files.path()).unwrap();
    app.index(&vault).unwrap();
    Harness {
        app,
        vault,
        _data: data,
        files,
        inbox,
    }
}

#[test]
fn an_html_page_arrives_as_a_searchable_markdown_note() {
    let mut h = harness();
    let page = h.inbox.path().join("clip.html");
    std::fs::write(
        &page,
        "<html><head><title>Fresnel lenses</title></head>\
         <body><h1>Optics</h1><p>A <em>stepped</em> lens concentrates the beam.</p></body></html>",
    )
    .unwrap();
    let vault = h.vault.clone();
    let report = h.app.import_file(&vault, &page, None).unwrap();
    assert_eq!(report.imported.len(), 1);
    assert_eq!(report.imported[0].title, "Fresnel lenses");

    // Indexed immediately (IM-005), structure preserved.
    let hits = h
        .app
        .search("stepped", SearchOptions::default())
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1);
    let text = std::fs::read_to_string(report.imported[0].path.to_fs(&h.vault.root)).unwrap();
    assert!(
        text.contains("_stepped_") || text.contains("*stepped*"),
        "{text}"
    );
    assert!(text.contains("source:"));
}

#[test]
fn an_eml_lands_with_headers_in_frontmatter_and_attachments_beside_it() {
    let mut h = harness();
    let eml = h.inbox.path().join("survey.eml");
    std::fs::write(
        &eml,
        "From: Marius Michel <pasha@phares.example>\r\n\
         Subject: Re: harbour soundings\r\n\
         Message-ID: <s1@phares.example>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain\r\n\r\nThe depths attached.\r\n\
         --b\r\nContent-Type: text/csv; name=\"depths.csv\"\r\n\
         Content-Disposition: attachment; filename=\"depths.csv\"\r\n\r\n\
         m,7\r\n--b--\r\n",
    )
    .unwrap();
    let vault = h.vault.clone();
    let folder = VaultPath::new("mail").unwrap();
    let report = h.app.import_file(&vault, &eml, Some(&folder)).unwrap();
    assert_eq!(report.imported.len(), 1);
    let note = &report.imported[0];
    // "Re:" is unportable on Windows; the name was sanitised, under mail/.
    assert!(note.path.as_str().starts_with("mail/"), "{}", note.path);
    assert!(!note.path.as_str().contains(':'), "{}", note.path);

    // The frontmatter rode in as user-authored metadata (IM-004).
    assert_eq!(note.title, "Re: harbour soundings");
    let attachment_dir = note.path.as_str().trim_end_matches(".md").to_owned() + " attachments";
    let attachment = h.files.path().join(&attachment_dir).join("depths.csv");
    assert!(attachment.exists(), "{}", attachment.display());

    // The note ends with a clickable list of what rode along (IM-011).
    let text = std::fs::read_to_string(note.path.to_fs(&h.vault.root)).unwrap();
    assert!(text.contains("## Attachments"), "{text}");
    assert!(
        text.contains("[depths.csv](<") && text.contains(" attachments/depths.csv>)"),
        "{text}"
    );

    // Importing the same file again does not overwrite (IM-003).
    let second = h.app.import_file(&vault, &eml, Some(&folder)).unwrap();
    assert_ne!(second.imported[0].path, note.path);
    assert!(second.imported[0].path.as_str().contains("(imported 2)"));
}

#[test]
fn a_pdf_attachment_is_searchable_by_its_contents() {
    let mut h = harness();
    // A PDF dropped beside the notes — the exact shape an imported email's
    // attachment lands in — must be findable by what it says, not only by
    // its filename (IM-008).
    std::fs::write(
        h.files.path().join("harbour report.pdf"),
        tiny_pdf("Dredging the western channel to nine metres."),
    )
    .unwrap();
    let vault = h.vault.clone();
    h.app.index(&vault).unwrap();

    let hits = h
        .app
        .search("dredging", SearchOptions::default())
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0]
        .document
        .path
        .as_str()
        .ends_with("harbour report.pdf"));
}

/// A minimal but structurally honest PDF: one page, one text object,
/// a correct xref. Mirrors the builder in marius-import's unit tests.
fn tiny_pdf(text: &str) -> Vec<u8> {
    use std::io::Write;
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
        write!(out, "{offset:010} 00000 n ")
            .and_then(|()| writeln!(out))
            .unwrap();
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF",
        objects.len() + 1
    )
    .unwrap();
    out
}

#[test]
fn an_mbox_becomes_one_note_per_message_with_thread_identifiers() {
    let mut h = harness();
    let mbox = h.inbox.path().join("archive.mbox");
    std::fs::write(
        &mbox,
        "From pasha@phares.example Tue Jun  1 10:00:00 1869\n\
         From: pasha@phares.example\n\
         Subject: Soundings\n\
         Message-ID: <m1@x>\n\
         Content-Type: text/plain\n\n\
         Seven metres at the buoy.\n\
         \n\
         From office@phares.example Wed Jun  2 09:00:00 1869\n\
         From: office@phares.example\n\
         Subject: Re: Soundings\n\
         In-Reply-To: <m1@x>\n\
         Content-Type: text/plain\n\n\
         Approved.\n",
    )
    .unwrap();
    let vault = h.vault.clone();
    let report = h.app.import_file(&vault, &mbox, None).unwrap();
    assert_eq!(report.skipped, 0);
    assert_eq!(report.imported.len(), 2);

    let reply = &report.imported[1];
    let text = std::fs::read_to_string(reply.path.to_fs(&h.vault.root)).unwrap();
    assert!(text.contains(r#"in_reply_to: "m1@x""#), "{text}");
    let hits = h.app.search("buoy", SearchOptions::default()).unwrap().hits;
    assert_eq!(hits.len(), 1);
}

#[test]
fn an_outlook_msg_lands_with_its_pdf_searchable_by_content() {
    use std::io::Write;
    let mut h = harness();

    // The shape work Outlook exports: an OLE compound file with MAPI
    // property streams and an attachment storage.
    let mut file = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    let unicode =
        |file: &mut cfb::CompoundFile<std::io::Cursor<Vec<u8>>>, path: &str, value: &str| {
            let mut stream = file.create_stream(path).unwrap();
            for unit in value.encode_utf16() {
                stream.write_all(&unit.to_le_bytes()).unwrap();
            }
        };
    unicode(&mut file, "/__substg1.0_0037001F", "Harbour works");
    unicode(
        &mut file,
        "/__substg1.0_1000001F",
        "The survey is attached.",
    );
    file.create_storage("/__attach_version1.0_#00000000")
        .unwrap();
    unicode(
        &mut file,
        "/__attach_version1.0_#00000000/__substg1.0_3707001F",
        "survey.pdf",
    );
    let mut data = file
        .create_stream("/__attach_version1.0_#00000000/__substg1.0_37010102")
        .unwrap();
    data.write_all(&tiny_pdf("Nine metres at the western mole."))
        .unwrap();
    drop(data);
    let msg = h.inbox.path().join("harbour works.msg");
    std::fs::write(&msg, file.into_inner().into_inner()).unwrap();

    let vault = h.vault.clone();
    let report = h.app.import_file(&vault, &msg, None).unwrap();
    assert_eq!(report.imported.len(), 1);
    let note = &report.imported[0];
    assert_eq!(note.title, "Harbour works");

    // The note links its attachment and the PDF's own words are indexed.
    let text = std::fs::read_to_string(note.path.to_fs(&h.vault.root)).unwrap();
    assert!(text.contains("## Attachments"), "{text}");
    assert!(text.contains("[survey.pdf](<"), "{text}");
    let hits = h
        .app
        .search("western mole", SearchOptions::default())
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].document.path.as_str().ends_with("survey.pdf"));
}

#[test]
fn unknown_formats_are_refused_with_the_supported_list() {
    let mut h = harness();
    let weird = h.inbox.path().join("photo.heic");
    std::fs::write(&weird, b"\x00\x01").unwrap();
    let vault = h.vault.clone();
    let error = h.app.import_file(&vault, &weird, None).unwrap_err();
    assert!(error.to_string().contains("no importer for .heic"));
}

#[test]
fn a_docx_and_an_epub_arrive_as_notes() {
    use std::io::Write;
    let mut h = harness();
    // Minimal DOCX: a zip holding word/document.xml.
    let docx = {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            writer
                .start_file(
                    "word/document.xml",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(
                    br#"<w:document xmlns:w="w"><w:body>
                        <w:p><w:r><w:t>Minutes of the lighthouse committee.</w:t></w:r></w:p>
                        </w:body></w:document>"#,
                )
                .unwrap();
            writer.finish().unwrap();
        }
        buffer.into_inner()
    };
    let path = h.inbox.path().join("minutes.docx");
    std::fs::write(&path, docx).unwrap();
    let vault = h.vault.clone();
    let report = h.app.import_file(&vault, &path, None).unwrap();
    assert_eq!(report.imported.len(), 1);
    let hits = h
        .app
        .search("committee", SearchOptions::default())
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1, "DOCX text is searchable (IM-008)");
}
