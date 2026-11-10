//! The sync client (SYNC-004..011): pull, apply, push against a
//! self-hosted backend.
//!
//! The dance, in order: **pull first** — applying the other side's history
//! before offering ours minimises server-side conflicts — then push every
//! outstanding journal entry that describes *source* state. Derived rows
//! (inline-tag assignments, link edges, revision history) never travel:
//! each device re-derives them from the replicated documents, which is
//! what decision 6's rebuildability already guarantees. Their journal
//! entries are acknowledged as "not replicated" rather than pushed.
//!
//! Conflicts are surfaced, never auto-resolved (SYNC-009): a pulled
//! operation touching an entity with local pending changes leaves the
//! local side in place and marks the journal entry `Conflicted`; a push
//! the server refuses comes back with the other device's envelope and is
//! recorded the same way.

use std::collections::{HashMap, HashSet};

use marius_indexing::{files, indexer};
use marius_model::{
    ChangeKind, Document, DocumentId, EntityKind, PullResponse, PushRequest, PushResponse,
    RelationshipId, SyncBackend, SyncEnvelope, SyncOperation, SyncPayload, SyncState, TagId, Vault,
    VaultId,
};
use marius_storage::{changelog, documents, relationships, replica, search_index, tags};

use crate::app::{CoreError, Marius, Result};

/// What one `sync` run did (SYNC-011).
#[derive(Debug, Default)]
pub struct SyncReport {
    pub pulled: usize,
    pub applied: usize,
    pub pushed: usize,
    /// Journal entries acknowledged without pushing: derived or
    /// unsupported entity kinds, re-derived on every device.
    pub not_replicated: usize,
    /// Entities in conflict, each with a human-readable description.
    pub conflicts: Vec<String>,
    /// Apply-side problems that skipped an operation (a path collision,
    /// a malformed payload). The operation stays on the server; nothing
    /// is lost, and the message says what to do.
    pub problems: Vec<String>,
    /// The server sequence this vault has now seen.
    pub cursor: u64,
}

const BACKEND: &str = "self-hosted";
const PULL_PAGE: usize = 500;
const PUSH_PAGE: usize = 500;

impl Marius {
    /// Point a vault at a backend (SYNC-007: one authority per vault;
    /// SYNC-008: switching resets the pull cursor, and the next sync
    /// bootstraps full state to an empty server).
    pub fn set_vault_backend(&mut self, vault: VaultId, backend: SyncBackend) -> Result<()> {
        let mut record = self.vault_by_id(vault)?;
        let policy = self.vault_policy(vault)?;
        record.sync_backend = backend;
        marius_storage::vaults::upsert(self.db.conn(), &record, &policy)?;
        Ok(())
    }

    /// Join a vault that already exists on a sync backend (SYNC-004).
    ///
    /// Unlike `open_vault`, the vault's *identity* is given, not minted:
    /// a synced vault is the same vault on every device, and documents,
    /// relationships and policies all embed that id (KB-006). The first
    /// sync pulls the backend's full history into the empty directory.
    pub fn join_vault(
        &mut self,
        name: &str,
        root: impl AsRef<std::path::Path>,
        id: VaultId,
        backend: SyncBackend,
    ) -> Result<Vault> {
        let root = root.as_ref();
        std::fs::create_dir_all(root).map_err(|source| crate::config::ConfigError::Write {
            path: root.to_path_buf(),
            source,
        })?;
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut vault = marius_model::Vault::new_local(name, &root);
        vault.id = id;
        vault.sync_backend = backend;
        let policy = marius_model::VaultPolicy::conservative(vault.id);
        marius_storage::vaults::upsert(self.db.conn(), &vault, &policy)?;
        Ok(vault)
    }

    /// One full pull-apply-push round against the vault's backend.
    ///
    /// `token` is the bearer secret the server was started with; it is
    /// passed by the caller (from an environment variable, SEC-001) and
    /// never stored.
    pub fn sync_vault(&mut self, vault: &Vault, token: Option<&str>) -> Result<SyncReport> {
        let SyncBackend::SelfHosted { base_url } = &vault.sync_backend else {
            return Err(CoreError::Inference(format!(
                "vault {:?} has no self-hosted backend configured; `marius vault backend {} <url>` sets one",
                vault.name, vault.name
            )));
        };
        let base_url = base_url.trim_end_matches('/').to_owned();
        require_transport_security(&base_url)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("constructing an HTTP client cannot fail with these options");
        let device = self.db.device_id();
        let mut report = SyncReport::default();

        // ------------------------------------------------------- pull
        let mut cursor = replica::cursor(self.db.conn(), vault.id, BACKEND)?;
        loop {
            let url = format!(
                "{base_url}/v1/vaults/{}/pull?after={cursor}&device={device}&limit={PULL_PAGE}",
                vault.id
            );
            let mut request = client.get(&url);
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .map_err(|e| CoreError::Inference(format!("cannot reach {base_url}: {e}")))?;
            let status = response.status();
            let body = response.text().unwrap_or_default();
            if !status.is_success() {
                return Err(CoreError::Inference(format!(
                    "{base_url} answered HTTP {status}: {}",
                    body.chars().take(200).collect::<String>()
                )));
            }
            let page: PullResponse = serde_json::from_str(&body)
                .map_err(|e| CoreError::Inference(format!("malformed pull response: {e}")))?;
            let page_len = page.ops.len();
            for (seq, envelope) in page.ops {
                report.pulled += 1;
                self.apply_envelope(vault, &envelope, &mut report)?;
                changelog::observe(self.db.conn(), envelope.op.timestamp)?;
                cursor = seq;
                replica::set_cursor(self.db.conn(), vault.id, BACKEND, cursor)?;
            }
            if page_len < PULL_PAGE {
                break;
            }
        }
        report.cursor = cursor;

        // ------------------------------------------------------- push
        // Bootstrap: an empty server and a fresh cursor mean this vault
        // has never replicated — offer everything, not just the journal.
        let bootstrap = cursor == 0;
        let envelopes = if bootstrap {
            self.materialize_everything(vault, &mut report)?
        } else {
            self.materialize_outstanding(vault, &mut report)?
        };

        for chunk in envelopes.chunks(PUSH_PAGE) {
            let request_body = PushRequest {
                device,
                ops: chunk.to_vec(),
            };
            let url = format!("{base_url}/v1/vaults/{}/push", vault.id);
            let mut request = client.post(&url).json(&request_body);
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .map_err(|e| CoreError::Inference(format!("cannot reach {base_url}: {e}")))?;
            let status = response.status();
            let body = response.text().unwrap_or_default();
            if !status.is_success() {
                return Err(CoreError::Inference(format!(
                    "{base_url} answered HTTP {status}: {}",
                    body.chars().take(200).collect::<String>()
                )));
            }
            let reply: PushResponse = serde_json::from_str(&body)
                .map_err(|e| CoreError::Inference(format!("malformed push response: {e}")))?;

            let conflicted: HashSet<_> = reply.conflicts.iter().map(|c| c.op).collect();
            for envelope in chunk {
                if conflicted.contains(&envelope.op.id) {
                    continue;
                }
                report.pushed += 1;
                // Bootstrap envelopes are synthetic; only journal-backed
                // ones have a row to acknowledge.
                let _ = changelog::set_state(
                    self.db.conn(),
                    envelope.op.id,
                    SyncState::Acknowledged,
                    None,
                );
            }
            for conflict in reply.conflicts {
                let theirs = &conflict.theirs.op;
                let description = format!(
                    "{} {}: this device and {} both changed it (their revision {})",
                    theirs.entity_kind, theirs.entity_id, theirs.timestamp.device, theirs.revision
                );
                let _ = changelog::set_state(
                    self.db.conn(),
                    conflict.op,
                    SyncState::Conflicted,
                    Some(&description),
                );
                report.conflicts.push(description);
            }
        }

        // The pull advanced past our own just-pushed rows next time; the
        // server never replays our own device's ops to us, so the cursor
        // stays wherever the pull left it.
        Ok(report)
    }

    // ---------------------------------------------------------- apply

    fn apply_envelope(
        &mut self,
        vault: &Vault,
        envelope: &SyncEnvelope,
        report: &mut SyncReport,
    ) -> Result<()> {
        let op = &envelope.op;

        // A local pending edit to the same entity: conflict, keep ours,
        // leave theirs on the server for the user to resolve (SYNC-009).
        if let Some(local) = self.pending_local_op(op)? {
            let description = format!(
                "{} {}: local unpushed change (revision {}) vs remote (revision {})",
                op.entity_kind, op.entity_id, local.revision, op.revision
            );
            let _ = changelog::set_state(
                self.db.conn(),
                local.id,
                SyncState::Conflicted,
                Some(&description),
            );
            report.conflicts.push(description);
            return Ok(());
        }

        match &envelope.payload {
            SyncPayload::Document { document, content } => {
                self.apply_document(vault, op, document, content.as_deref(), report)?;
            }
            SyncPayload::Tombstone => self.apply_tombstone(vault, op, report)?,
            SyncPayload::Tag { tag } => {
                replica::upsert_tag(self.db.conn(), tag, op.revision)?;
                report.applied += 1;
            }
            SyncPayload::DocumentTag {
                assignment,
                tag_name,
            } => {
                if !replica::document_exists(self.db.conn(), assignment.document)? {
                    report.problems.push(format!(
                        "assignment of #{tag_name} skipped: document {} not present",
                        assignment.document
                    ));
                    return Ok(());
                }
                replica::ensure_tag_id(self.db.conn(), assignment.tag, Some(vault.id), tag_name)?;
                tags::assign_replica(self.db.conn(), assignment)?;
                report.applied += 1;
            }
            SyncPayload::RejectedTag {
                document,
                tag,
                tag_name,
                reason,
            } => {
                replica::ensure_tag_id(self.db.conn(), *tag, Some(vault.id), tag_name)?;
                replica::reject_tag(self.db.conn(), *document, *tag, reason.as_deref())?;
                report.applied += 1;
            }
            SyncPayload::Relationship { relationship } => {
                replica::upsert_relationship(self.db.conn(), relationship, op.revision)?;
                report.applied += 1;
            }
            SyncPayload::RejectedRelationship { rejection } => {
                replica::reject_relationship(self.db.conn(), rejection)?;
                report.applied += 1;
            }
            SyncPayload::Opaque { .. } => {
                // A newer client's data. Stored on the server, not ours to
                // interpret — and not ours to lose, so nothing happens here.
            }
        }
        Ok(())
    }

    fn apply_document(
        &mut self,
        vault: &Vault,
        op: &SyncOperation,
        document: &Document,
        content: Option<&str>,
        report: &mut SyncReport,
    ) -> Result<()> {
        // A different local document already owns this path: applying
        // would overwrite a file the remote never saw. Keep ours, say so.
        if let Some(local) = documents::by_path(self.db.conn(), vault.id, &document.path)? {
            if local.id != document.id {
                report.problems.push(format!(
                    "{}: a different local note owns this path; remote {} not applied",
                    document.path, document.id
                ));
                return Ok(());
            }
        }

        let mut incoming = document.clone();
        incoming.vault = vault.id;

        if let Some(content) = content {
            let absolute = incoming.path.to_fs(&vault.root);
            files::write_atomic(&absolute, content).map_err(CoreError::from)?;
        }
        replica::upsert_document(self.db.conn(), &incoming, op.revision)?;

        // Derived rows and FTS, via a replica-mode index pass: keeps the
        // remote revision, journals nothing (no echo).
        if content.is_some() {
            let mut options = self.index_options(vault)?;
            options.replica = true;
            indexer::index_path(&mut self.db, vault, &incoming.path, &options)?;
        } else if let Some(seq) = documents::seq_of(self.db.conn(), incoming.id)? {
            search_index::index(
                self.db.conn(),
                seq,
                &marius_storage::search_index::IndexedText {
                    title: incoming.title.clone(),
                    aliases: String::new(),
                    tags: String::new(),
                    path: incoming.path.to_string(),
                    body: String::new(),
                },
            )?;
        }
        report.applied += 1;
        Ok(())
    }

    fn apply_tombstone(
        &mut self,
        vault: &Vault,
        op: &SyncOperation,
        report: &mut SyncReport,
    ) -> Result<()> {
        match op.entity_kind {
            EntityKind::Document => {
                let Ok(id) = op.entity_id.parse::<DocumentId>() else {
                    report
                        .problems
                        .push(format!("unparseable id {}", op.entity_id));
                    return Ok(());
                };
                if let Some(document) = documents::get(self.db.conn(), id)? {
                    // To the trash, not gone: a deletion that arrived over
                    // the wire deserves the same escape hatch as a local one.
                    let _ = files::trash(&vault.root, document.path.as_str());
                    replica::delete_document(self.db.conn(), id)?;
                }
                report.applied += 1;
            }
            EntityKind::Tag => {
                if let Ok(id) = op.entity_id.parse::<TagId>() {
                    replica::delete_tag(self.db.conn(), id)?;
                    report.applied += 1;
                }
            }
            EntityKind::DocumentTag => {
                if let Some((document, tag)) = parse_pair(&op.entity_id) {
                    replica::unassign_tag(self.db.conn(), document, tag)?;
                    report.applied += 1;
                }
            }
            EntityKind::Relationship => {
                if let Ok(id) = op.entity_id.parse::<RelationshipId>() {
                    replica::delete_relationship(self.db.conn(), id)?;
                    report.applied += 1;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn pending_local_op(&self, remote: &SyncOperation) -> Result<Option<SyncOperation>> {
        let outstanding = changelog::outstanding(self.db.conn(), 10_000)?;
        Ok(outstanding.into_iter().find(|local| {
            local.state == SyncState::Pending
                && local.entity_kind == remote.entity_kind
                && local.entity_id == remote.entity_id
                && replicates(local.entity_kind)
        }))
    }

    // ---------------------------------------------------- materialise

    /// Envelopes for every outstanding journal entry that replicates,
    /// newest state per entity, acknowledging the rest.
    fn materialize_outstanding(
        &mut self,
        vault: &Vault,
        report: &mut SyncReport,
    ) -> Result<Vec<SyncEnvelope>> {
        let outstanding = changelog::outstanding(self.db.conn(), 100_000)?;
        let mut latest: HashMap<(EntityKind, String), SyncOperation> = HashMap::new();
        for op in outstanding {
            if op.state != SyncState::Pending || op.vault != vault.id {
                continue;
            }
            if !replicates(op.entity_kind) {
                changelog::set_state(self.db.conn(), op.id, SyncState::Acknowledged, None)?;
                report.not_replicated += 1;
                continue;
            }
            // Later journal entries supersede earlier ones for the same
            // entity; the superseded rows are acknowledged unsent.
            if let Some(previous) = latest.insert((op.entity_kind, op.entity_id.clone()), op) {
                changelog::set_state(
                    self.db.conn(),
                    previous.id,
                    SyncState::Acknowledged,
                    Some("superseded by a later local change"),
                )?;
            }
        }

        let mut envelopes = Vec::new();
        for ((_, _), op) in latest {
            match self.payload_for(vault, &op)? {
                Some((payload, revision)) => {
                    let mut op = op;
                    op.revision = revision;
                    envelopes.push(SyncEnvelope { op, payload });
                }
                None => {
                    changelog::set_state(
                        self.db.conn(),
                        op.id,
                        SyncState::Acknowledged,
                        Some("entity no longer replicates"),
                    )?;
                    report.not_replicated += 1;
                }
            }
        }
        sort_for_apply(&mut envelopes);
        Ok(envelopes)
    }

    /// Bootstrap (SYNC-008): synthesise an upsert for every replicating
    /// entity the vault holds, journal or no journal.
    fn materialize_everything(
        &mut self,
        vault: &Vault,
        report: &mut SyncReport,
    ) -> Result<Vec<SyncEnvelope>> {
        // The journal-backed set first, so acknowledgements happen.
        let mut envelopes = self.materialize_outstanding(vault, report)?;
        let mut covered: HashSet<(EntityKind, String)> = envelopes
            .iter()
            .map(|e| (e.op.entity_kind, e.op.entity_id.clone()))
            .collect();

        let filter = documents::Filter {
            vaults: vec![vault.id],
            ..Default::default()
        };
        for document in documents::list(self.db.conn(), &filter)? {
            let key = (EntityKind::Document, document.id.to_string());
            if covered.contains(&key) {
                continue;
            }
            let op = self.synthetic_op(vault, EntityKind::Document, &key.1)?;
            if let Some((payload, revision)) = self.payload_for(vault, &op)? {
                let mut op = op;
                op.revision = revision;
                covered.insert(key);
                envelopes.push(SyncEnvelope { op, payload });
            }
        }
        for (tag, count) in tags::counts(self.db.conn(), Some(vault.id))? {
            let _ = count;
            let key = (EntityKind::Tag, tag.id.to_string());
            if covered.contains(&key) {
                continue;
            }
            let op = self.synthetic_op(vault, EntityKind::Tag, &key.1)?;
            if let Some((payload, revision)) = self.payload_for(vault, &op)? {
                let mut op = op;
                op.revision = revision;
                covered.insert(key);
                envelopes.push(SyncEnvelope { op, payload });
            }
        }
        for relationship in relationships::in_vaults(self.db.conn(), &[vault.id], &[], None)? {
            if relationship.provenance == marius_model::Provenance::Derived {
                continue;
            }
            let key = (EntityKind::Relationship, relationship.id.to_string());
            if covered.contains(&key) {
                continue;
            }
            let op = self.synthetic_op(vault, EntityKind::Relationship, &key.1)?;
            if let Some((payload, revision)) = self.payload_for(vault, &op)? {
                let mut op = op;
                op.revision = revision;
                covered.insert(key);
                envelopes.push(SyncEnvelope { op, payload });
            }
        }
        // Assignments and rejections ride on the document listing.
        for document in documents::list(self.db.conn(), &filter)? {
            for (tag, assignment) in self.document_tags(document.id)? {
                if assignment.provenance == marius_model::Provenance::Derived {
                    continue;
                }
                let key = (
                    EntityKind::DocumentTag,
                    format!("{}/{}", document.id, tag.id),
                );
                if covered.contains(&key) {
                    continue;
                }
                let op = self.synthetic_op(vault, EntityKind::DocumentTag, &key.1)?;
                if let Some((payload, revision)) = self.payload_for(vault, &op)? {
                    let mut op = op;
                    op.revision = revision;
                    covered.insert(key);
                    envelopes.push(SyncEnvelope { op, payload });
                }
            }
        }
        sort_for_apply(&mut envelopes);
        Ok(envelopes)
    }

    fn synthetic_op(
        &mut self,
        vault: &Vault,
        kind: EntityKind,
        entity_id: &str,
    ) -> Result<SyncOperation> {
        let timestamp = changelog::next_timestamp(self.db.conn())?;
        let mut op = SyncOperation::new(
            vault.id,
            kind,
            entity_id,
            ChangeKind::Upsert,
            1,
            timestamp,
            BACKEND,
        );
        op.state = SyncState::Pending;
        Ok(op)
    }

    /// The current full state for one journaled entity, or `None` when it
    /// does not replicate (derived) or no longer exists as described.
    fn payload_for(
        &self,
        _vault: &Vault,
        op: &SyncOperation,
    ) -> Result<Option<(SyncPayload, u64)>> {
        if op.change == ChangeKind::Delete {
            return Ok(Some((SyncPayload::Tombstone, op.revision.max(1))));
        }
        Ok(match op.entity_kind {
            EntityKind::Document => {
                let Ok(id) = op.entity_id.parse::<DocumentId>() else {
                    return Ok(None);
                };
                match documents::get(self.db.conn(), id)? {
                    Some(document) => {
                        let revision =
                            replica::document_revision(self.db.conn(), id)?.unwrap_or(op.revision);
                        let content = if document.kind == marius_model::DocumentKind::Markdown {
                            Some(self.read(&document)?)
                        } else {
                            None
                        };
                        Some((SyncPayload::Document { document, content }, revision))
                    }
                    // Journaled, then deleted before the push: tombstone.
                    None => Some((SyncPayload::Tombstone, op.revision.max(1))),
                }
            }
            EntityKind::Tag => {
                let Ok(id) = op.entity_id.parse::<TagId>() else {
                    return Ok(None);
                };
                match replica::get_tag(self.db.conn(), id)? {
                    Some((tag, revision)) => Some((SyncPayload::Tag { tag }, revision)),
                    None => Some((SyncPayload::Tombstone, op.revision.max(1))),
                }
            }
            EntityKind::DocumentTag => {
                let Some((document, tag)) = parse_pair(&op.entity_id) else {
                    return Ok(None);
                };
                match replica::get_assignment(self.db.conn(), document, tag)? {
                    Some(assignment)
                        if assignment.provenance != marius_model::Provenance::Derived =>
                    {
                        let Some((tag_row, _)) = replica::get_tag(self.db.conn(), tag)? else {
                            return Ok(None);
                        };
                        Some((
                            SyncPayload::DocumentTag {
                                assignment,
                                tag_name: tag_row.canonical_name,
                            },
                            1,
                        ))
                    }
                    Some(_) => None, // derived: re-derived remotely
                    None => Some((SyncPayload::Tombstone, 1)),
                }
            }
            EntityKind::RejectedTag => {
                let Some((document, tag)) = parse_pair(&op.entity_id) else {
                    return Ok(None);
                };
                let Some(reason) = replica::get_rejected_tag(self.db.conn(), document, tag)? else {
                    return Ok(Some((SyncPayload::Tombstone, 1)));
                };
                let Some((tag_row, _)) = replica::get_tag(self.db.conn(), tag)? else {
                    return Ok(None);
                };
                Some((
                    SyncPayload::RejectedTag {
                        document,
                        tag,
                        tag_name: tag_row.canonical_name,
                        reason,
                    },
                    1,
                ))
            }
            EntityKind::Relationship => {
                let Ok(id) = op.entity_id.parse::<RelationshipId>() else {
                    return Ok(None);
                };
                match relationships::get(self.db.conn(), id)? {
                    Some(relationship)
                        if relationship.provenance != marius_model::Provenance::Derived =>
                    {
                        let revision = replica::relationship_revision(self.db.conn(), id)?
                            .unwrap_or(op.revision);
                        Some((SyncPayload::Relationship { relationship }, revision))
                    }
                    Some(_) => None,
                    None => Some((SyncPayload::Tombstone, op.revision.max(1))),
                }
            }
            EntityKind::RejectedRelationship => {
                // Materialised from the rejection table via its entity id.
                let parts: Vec<&str> = op.entity_id.split('/').collect();
                if parts.len() != 3 {
                    return Ok(None);
                }
                let (Ok(source), Ok(target)) = (
                    parts[0].parse::<DocumentId>(),
                    parts[1].parse::<DocumentId>(),
                ) else {
                    return Ok(None);
                };
                let Ok(kind) = parts[2].parse::<marius_model::RelationshipKind>() else {
                    return Ok(None);
                };
                let all = relationships::rejections(self.db.conn(), source)?;
                all.into_iter()
                    .find(|r| r.endpoints.target.document == target && r.kind == kind)
                    .map(|rejection| (SyncPayload::RejectedRelationship { rejection }, 1))
            }
            _ => None,
        })
    }

    /// The conflicted journal entries, for `marius sync status` (SYNC-011).
    pub fn sync_conflicts(&self) -> Result<Vec<SyncOperation>> {
        let outstanding = changelog::outstanding(self.db.conn(), 10_000)?;
        Ok(outstanding
            .into_iter()
            .filter(|op| op.state == SyncState::Conflicted)
            .collect())
    }
}

/// Order a batch so that what references comes after what is referenced:
/// documents, then tags, then everything that points at them. Within one
/// rank, journal order (the logical clock) is preserved.
fn sort_for_apply(envelopes: &mut [SyncEnvelope]) {
    fn rank(kind: EntityKind) -> u8 {
        match kind {
            EntityKind::Document => 0,
            EntityKind::Tag => 1,
            EntityKind::DocumentTag => 2,
            EntityKind::RejectedTag => 3,
            EntityKind::Relationship => 4,
            EntityKind::RejectedRelationship => 5,
            _ => 6,
        }
    }
    envelopes.sort_by_key(|e| (rank(e.op.entity_kind), e.op.timestamp.counter));
}

/// Entity kinds a device pushes. Everything else is derived or unsupported
/// and re-derived locally on each device.
fn replicates(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Document
            | EntityKind::Tag
            | EntityKind::DocumentTag
            | EntityKind::RejectedTag
            | EntityKind::Relationship
            | EntityKind::RejectedRelationship
    )
}

fn parse_pair(entity_id: &str) -> Option<(DocumentId, TagId)> {
    let (document, tag) = entity_id.split_once('/')?;
    Some((document.parse().ok()?, tag.parse().ok()?))
}

/// SYNC-013: remote communication is TLS. Loopback is exempt — a server on
/// this machine crosses no wire — and that is also what makes tests real.
fn require_transport_security(base_url: &str) -> Result<()> {
    if base_url.starts_with("https://") {
        return Ok(());
    }
    let loopback = base_url.starts_with("http://127.0.0.1")
        || base_url.starts_with("http://localhost")
        || base_url.starts_with("http://[::1]");
    if loopback {
        return Ok(());
    }
    Err(CoreError::Inference(format!(
        "{base_url} is neither https nor loopback; refusing to sync in the clear (SYNC-013)"
    )))
}
