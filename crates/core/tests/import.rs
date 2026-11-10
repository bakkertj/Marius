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

    // Importing the same file again does not overwrite (IM-003).
    let second = h.app.import_file(&vault, &eml, Some(&folder)).unwrap();
    assert_ne!(second.imported[0].path, note.path);
    assert!(second.imported[0].path.as_str().contains("(imported 2)"));
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
fn unknown_formats_are_refused_with_the_supported_list() {
    let mut h = harness();
    let weird = h.inbox.path().join("photo.heic");
    std::fs::write(&weird, b"\x00\x01").unwrap();
    let vault = h.vault.clone();
    let error = h.app.import_file(&vault, &weird, None).unwrap_err();
    assert!(error.to_string().contains("no importer for .heic"));
}
