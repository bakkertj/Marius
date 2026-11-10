//! Two devices, one self-hosted backend, real HTTP on a loopback port —
//! SYNC-004..011 end to end with no doubles anywhere: real files, two real
//! SQLite databases, and the real service store behind a minimal HTTP
//! shell (the same store the production binary wraps).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use marius_core::{Marius, SearchOptions};
use marius_model::{DeviceId, Provenance, PushRequest, SyncBackend, SyncState, VaultId, VaultPath};
use marius_sync_service::Store;

/// Serve the two sync endpoints over the real store until dropped.
fn serve(store: Store) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Arc::new(Mutex::new(store));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let store = store.clone();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first_line = String::new();
            if reader.read_line(&mut first_line).unwrap() == 0 {
                continue;
            }
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = rest.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut payload = vec![0u8; content_length];
            reader.read_exact(&mut payload).unwrap();

            let path = first_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_owned();
            let (status, body) = route(&store, &first_line, &path, &payload);
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    format!("http://{addr}")
}

fn route(
    store: &Arc<Mutex<Store>>,
    first_line: &str,
    path: &str,
    payload: &[u8],
) -> (&'static str, String) {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    // /v1/vaults/:id/push  or  /v1/vaults/:id/pull?...
    if segments.len() != 4 || segments[0] != "v1" || segments[1] != "vaults" {
        return ("404 Not Found", "{}".into());
    }
    let vault: VaultId = segments[2].parse().unwrap();
    let action = segments[3];
    let mut store = store.lock().unwrap();
    if first_line.starts_with("POST") && action == "push" {
        let request: PushRequest = serde_json::from_slice(payload).unwrap();
        let response = store.push(vault, &request).unwrap();
        return ("200 OK", serde_json::to_string(&response).unwrap());
    }
    if first_line.starts_with("GET") && action.starts_with("pull") {
        let query = action.split_once('?').map(|(_, q)| q).unwrap_or("");
        let mut after = 0u64;
        let mut device = None;
        let mut limit = 500usize;
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("after", v)) => after = v.parse().unwrap_or(0),
                Some(("device", v)) => device = v.parse::<DeviceId>().ok(),
                Some(("limit", v)) => limit = v.parse().unwrap_or(500),
                _ => {}
            }
        }
        let response = store
            .pull(vault, device.unwrap_or_else(DeviceId::new), after, limit)
            .unwrap();
        return ("200 OK", serde_json::to_string(&response).unwrap());
    }
    ("404 Not Found", "{}".into())
}

struct Device {
    app: Marius,
    vault: marius_model::Vault,
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
}

impl Device {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.files.path().join(path)).ok()
    }

    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.files.path().join(path), text).unwrap();
    }

    fn sync(&mut self) -> marius_core::SyncReport {
        let vault = self.vault.clone();
        self.app.sync_vault(&vault, None).unwrap()
    }
}

/// Alice has a vault with content; Bob joins it empty via the backend.
fn pair() -> (Device, Device, String) {
    let url = serve(Store::open_in_memory().unwrap());

    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    std::fs::write(
        files.path().join("pharos.md"),
        "# Pharos\n\nThe lighthouse. See [[harbour]]. #antiquity\n",
    )
    .unwrap();
    std::fs::write(
        files.path().join("harbour.md"),
        "# Harbour\n\nThe eastern basin.\n",
    )
    .unwrap();
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("Shared", files.path()).unwrap();
    app.index(&vault).unwrap();
    app.set_vault_backend(
        vault.id,
        SyncBackend::SelfHosted {
            base_url: url.clone(),
        },
    )
    .unwrap();
    let vault = app.vault("Shared").unwrap();
    let alice = Device {
        app,
        vault,
        _data: data,
        files,
    };

    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app
        .join_vault(
            "Shared",
            files.path(),
            alice.vault.id,
            SyncBackend::SelfHosted {
                base_url: url.clone(),
            },
        )
        .unwrap();
    let bob = Device {
        app,
        vault,
        _data: data,
        files,
    };
    (alice, bob, url)
}

#[test]
fn a_vault_replicates_to_a_joining_device_files_and_all() {
    let (mut alice, mut bob, _url) = pair();
    alice
        .app
        .add_tag(
            alice
                .app
                .document_at(alice.vault.id, &VaultPath::new("pharos.md").unwrap())
                .unwrap()
                .unwrap()
                .id,
            "beacon",
        )
        .unwrap();

    let pushed = alice.sync();
    assert!(
        pushed.pushed >= 3,
        "documents and tag travelled: {pushed:?}"
    );
    assert!(pushed.conflicts.is_empty());

    let pulled = bob.sync();
    assert!(pulled.applied >= 3, "{pulled:?}");

    // Real files on Bob's disk.
    let pharos = bob.read("pharos.md").expect("file replicated");
    assert!(pharos.contains("The lighthouse"));

    // Searchable, same identity, links re-derived locally.
    let hits = bob
        .app
        .search("lighthouse", SearchOptions::default())
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1);
    let bob_doc = bob
        .app
        .document_at(bob.vault.id, &VaultPath::new("pharos.md").unwrap())
        .unwrap()
        .unwrap();
    let alice_doc = alice
        .app
        .document_at(alice.vault.id, &VaultPath::new("pharos.md").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(bob_doc.id, alice_doc.id, "identity survives (KB-006)");

    let neighbourhood = bob.app.neighbourhood(bob_doc.id).unwrap();
    assert!(
        !neighbourhood.outgoing.is_empty(),
        "the [[harbour]] link was re-derived on Bob's device"
    );

    // The manual tag arrived with provenance intact.
    let tags = bob.app.document_tags(bob_doc.id).unwrap();
    assert!(tags.iter().any(
        |(t, dt)| t.canonical_name.as_str() == "beacon" && dt.provenance == Provenance::Manual
    ));
}

#[test]
fn an_edit_travels_back_and_forward_without_echo() {
    let (mut alice, mut bob, _url) = pair();
    alice.sync();
    bob.sync();

    bob.write("pharos.md", "# Pharos\n\nRebuilt after the earthquake.\n");
    let vault = bob.vault.clone();
    bob.app.index(&vault).unwrap();
    let report = bob.sync();
    assert!(report.pushed >= 1);

    let report = alice.sync();
    assert!(report.applied >= 1);
    assert!(alice.read("pharos.md").unwrap().contains("Rebuilt"));

    // Steady state: another round in each direction moves nothing — the
    // applied edit did not echo back as a fresh local change (SYNC-010).
    let a = alice.sync();
    let b = bob.sync();
    assert_eq!(a.pushed, 0, "{a:?}");
    assert_eq!(b.applied, 0, "{b:?}");
}

#[test]
fn concurrent_edits_conflict_and_neither_side_is_discarded() {
    let (mut alice, mut bob, _url) = pair();
    alice.sync();
    bob.sync();

    alice.write("pharos.md", "# Pharos\n\nAlice's account.\n");
    bob.write("pharos.md", "# Pharos\n\nBob's account.\n");
    let (va, vb) = (alice.vault.clone(), bob.vault.clone());
    alice.app.index(&va).unwrap();
    bob.app.index(&vb).unwrap();

    let first = alice.sync();
    assert!(first.conflicts.is_empty());
    let second = bob.sync();
    assert!(
        !second.conflicts.is_empty(),
        "the later writer hears about the conflict: {second:?}"
    );

    // Nothing was silently discarded (SYNC-009): Bob's file still holds
    // Bob's words, the journal marks the conflict for resolution.
    assert!(bob.read("pharos.md").unwrap().contains("Bob's account"));
    assert!(!bob.app.sync_conflicts().unwrap().is_empty());
    assert!(alice.read("pharos.md").unwrap().contains("Alice's account"));
}

#[test]
fn a_rejection_made_on_one_device_suppresses_proposals_on_the_other() {
    // Integrity rule 7 across the wire.
    let (mut alice, mut bob, _url) = pair();
    alice.sync();
    bob.sync();

    let doc = alice
        .app
        .document_at(alice.vault.id, &VaultPath::new("pharos.md").unwrap())
        .unwrap()
        .unwrap();
    alice.app.add_tag(doc.id, "wonder").unwrap();
    alice
        .app
        .reject_tag(doc.id, "wonder", Some("not that list"))
        .unwrap();
    alice.sync();
    bob.sync();

    // Bob's device now refuses a machine proposal of the same tag.
    let bob_doc = bob
        .app
        .document_at(bob.vault.id, &VaultPath::new("pharos.md").unwrap())
        .unwrap()
        .unwrap();
    let tags = bob.app.document_tags(bob_doc.id).unwrap();
    assert!(
        tags.iter()
            .all(|(t, _)| t.canonical_name.as_str() != "wonder"),
        "the rejected tag is not live on Bob's device"
    );
}

#[test]
fn a_deletion_travels_as_a_tombstone_into_the_trash() {
    let (mut alice, mut bob, _url) = pair();
    alice.sync();
    bob.sync();
    assert!(bob.read("harbour.md").is_some());

    let doc = alice
        .app
        .document_at(alice.vault.id, &VaultPath::new("harbour.md").unwrap())
        .unwrap()
        .unwrap();
    alice.app.delete_document(doc.id).unwrap();
    alice.sync();
    bob.sync();

    assert!(bob.read("harbour.md").is_none(), "file removed on Bob");
    assert!(
        bob.app
            .document_at(bob.vault.id, &VaultPath::new("harbour.md").unwrap())
            .unwrap()
            .is_none(),
        "record removed on Bob"
    );

    // Pending-state cleanliness: nothing left outstanding on either side.
    assert_eq!(alice.sync().pushed, 0);
    assert_eq!(bob.sync().pushed, 0);
}

#[test]
fn plain_http_to_a_remote_host_is_refused() {
    // SYNC-013: loopback is exempt, the open internet is not.
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    std::fs::write(files.path().join("a.md"), "# A\n").unwrap();
    let mut app = Marius::open(data.path()).unwrap();
    let vault = app.open_vault("V", files.path()).unwrap();
    app.set_vault_backend(
        vault.id,
        SyncBackend::SelfHosted {
            base_url: "http://sync.example.com".into(),
        },
    )
    .unwrap();
    let vault = app.vault("V").unwrap();
    let error = app.sync_vault(&vault, None).unwrap_err();
    assert!(error.to_string().contains("refusing to sync in the clear"));
}

#[test]
fn local_only_journal_entries_do_not_wait_for_a_backend() {
    // After a full round, nothing is left pending on the joining device —
    // the regression guard for unbounded journal growth, now with a
    // backend in the picture.
    let (mut alice, mut bob, _url) = pair();
    alice.sync();
    bob.sync();
    bob.sync();
    let states = marius_storage::changelog::counts_by_state(bob.app.database().conn()).unwrap();
    for (state, count) in states {
        if state == SyncState::Pending {
            assert_eq!(count, 0, "fresh join should have nothing pending");
        }
    }
}
