//! # The sync service, minus the web server
//!
//! Everything the self-hosted backend (SYNC-004) decides — ordering,
//! idempotency, conflict detection — lives here, over SQLite, so the whole
//! protocol is tested in the ordinary workspace on every platform. The
//! `marius-sync-server` binary is a thin HTTP shell around this crate; a
//! PostgreSQL store can implement the same two entry points behind the
//! same tests when multi-writer scale demands it.
//!
//! ## The rules
//!
//! * Operations are appended in arrival order under one global sequence;
//!   pulling is "everything after your cursor" (SYNC-006).
//! * A push is idempotent by operation id: replaying a batch re-accepts
//!   it without duplicating anything (SYNC-010).
//! * For revisioned entities (documents, tags, relationships), a pushed
//!   revision not newer than the stored head **from a different device**
//!   is a conflict, answered with the operation the server holds — never
//!   resolved by discarding either side (SYNC-009).
//! * Assignment-shaped entities (document tags, rejections) replicate as
//!   last-writer-wins upserts and never conflict: both devices adding the
//!   same tag is agreement, not an edit war, and a rejection is durable by
//!   design wherever it lands.

#![forbid(unsafe_code)]

use marius_model::{
    DeviceId, EntityKind, PullResponse, PushConflict, PushRequest, PushResponse, SyncEnvelope,
    VaultId,
};
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("malformed envelope: {0}")]
    Malformed(String),
}

pub type Result<T> = std::result::Result<T, ServiceError>;

/// The service's store: one SQLite file (`:memory:` in tests).
pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ops (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    vault_id    TEXT NOT NULL,
    op_id       TEXT NOT NULL UNIQUE,
    device      TEXT NOT NULL,
    entity_kind TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    revision    INTEGER NOT NULL,
    envelope    TEXT NOT NULL,
    received_at TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS ops_vault_seq ON ops(vault_id, seq);

-- Latest accepted revision per entity, the conflict-detection head.
CREATE TABLE IF NOT EXISTS entity_head (
    vault_id    TEXT NOT NULL,
    entity_kind TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    revision    INTEGER NOT NULL,
    device      TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    PRIMARY KEY (vault_id, entity_kind, entity_id)
) STRICT;
"#;

/// Entity kinds whose revisions mean "edit generation" and therefore
/// conflict; everything else is a last-writer-wins upsert.
fn is_revisioned(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Document | EntityKind::Tag | EntityKind::Relationship
    )
}

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Apply one push batch atomically.
    pub fn push(&mut self, vault: VaultId, request: &PushRequest) -> Result<PushResponse> {
        let tx = self.conn.transaction()?;
        let mut accepted = Vec::new();
        let mut conflicts = Vec::new();

        for envelope in &request.ops {
            let op = &envelope.op;

            // Replay of something we already hold: idempotent accept.
            let known: Option<i64> = tx
                .query_row(
                    "SELECT seq FROM ops WHERE op_id = ?1",
                    params![op.id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if known.is_some() {
                accepted.push(op.id);
                continue;
            }

            if is_revisioned(op.entity_kind) {
                let head: Option<(i64, String, i64)> = tx
                    .query_row(
                        "SELECT revision, device, seq FROM entity_head
                         WHERE vault_id = ?1 AND entity_kind = ?2 AND entity_id = ?3",
                        params![vault.to_string(), op.entity_kind.as_str(), op.entity_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                if let Some((head_revision, head_device, head_seq)) = head {
                    let same_device = head_device == request.device.to_string();
                    if op.revision as i64 <= head_revision && !same_device {
                        // Concurrent edit: hand back the server's side
                        // (SYNC-009) and accept nothing for this entity.
                        let theirs: String = tx.query_row(
                            "SELECT envelope FROM ops WHERE seq = ?1",
                            params![head_seq],
                            |row| row.get(0),
                        )?;
                        let theirs: SyncEnvelope = serde_json::from_str(&theirs)
                            .map_err(|e| ServiceError::Malformed(e.to_string()))?;
                        conflicts.push(PushConflict { op: op.id, theirs });
                        continue;
                    }
                }
            }

            let json = serde_json::to_string(envelope)
                .map_err(|e| ServiceError::Malformed(e.to_string()))?;
            tx.execute(
                "INSERT INTO ops
                    (vault_id, op_id, device, entity_kind, entity_id, revision, envelope,
                     received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    vault.to_string(),
                    op.id.to_string(),
                    request.device.to_string(),
                    op.entity_kind.as_str(),
                    op.entity_id,
                    op.revision as i64,
                    json,
                    now(),
                ],
            )?;
            let seq = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO entity_head (vault_id, entity_kind, entity_id, revision, device, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(vault_id, entity_kind, entity_id) DO UPDATE SET
                    revision = excluded.revision,
                    device = excluded.device,
                    seq = excluded.seq",
                params![
                    vault.to_string(),
                    op.entity_kind.as_str(),
                    op.entity_id,
                    op.revision as i64,
                    request.device.to_string(),
                    seq,
                ],
            )?;
            accepted.push(op.id);
        }

        tx.commit()?;
        Ok(PushResponse {
            accepted,
            conflicts,
        })
    }

    /// Everything after `after`, oldest first, skipping the puller's own
    /// operations — a device does not need to be told what it did.
    pub fn pull(
        &self,
        vault: VaultId,
        device: DeviceId,
        after: u64,
        limit: usize,
    ) -> Result<PullResponse> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, envelope FROM ops
             WHERE vault_id = ?1 AND seq > ?2 AND device <> ?3
             ORDER BY seq LIMIT ?4",
        )?;
        let ops = stmt
            .query_map(
                params![
                    vault.to_string(),
                    after as i64,
                    device.to_string(),
                    limit as i64
                ],
                |row| {
                    let seq: i64 = row.get(0)?;
                    let json: String = row.get(1)?;
                    Ok((seq as u64, json))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut decoded = Vec::with_capacity(ops.len());
        for (seq, json) in ops {
            let envelope: SyncEnvelope =
                serde_json::from_str(&json).map_err(|e| ServiceError::Malformed(e.to_string()))?;
            decoded.push((seq, envelope));
        }
        let head: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM ops WHERE vault_id = ?1",
            params![vault.to_string()],
            |row| row.get(0),
        )?;
        Ok(PullResponse {
            ops: decoded,
            head: head as u64,
        })
    }

    /// Operations stored for a vault, for the status view.
    pub fn count(&self, vault: VaultId) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM ops WHERE vault_id = ?1",
            params![vault.to_string()],
            |row| row.get(0),
        )?;
        Ok(n as u64)
    }
}

// Ids travel as text in this store: text keys make the server's database
// inspectable with any SQLite shell, which a self-hoster wants on day one.

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use marius_model::{ChangeKind, LogicalTimestamp, SyncOperation, SyncPayload};

    fn envelope(device: DeviceId, kind: EntityKind, entity: &str, revision: u64) -> SyncEnvelope {
        SyncEnvelope {
            op: SyncOperation::new(
                VaultId::nil(),
                kind,
                entity,
                ChangeKind::Upsert,
                revision,
                LogicalTimestamp::new(revision, device),
                "self-hosted",
            ),
            payload: SyncPayload::Opaque {
                json: format!("{{\"entity\":\"{entity}\",\"rev\":{revision}}}"),
            },
        }
    }

    fn push_one(
        store: &mut Store,
        device: DeviceId,
        kind: EntityKind,
        entity: &str,
        revision: u64,
    ) -> PushResponse {
        store
            .push(
                VaultId::nil(),
                &PushRequest {
                    device,
                    ops: vec![envelope(device, kind, entity, revision)],
                },
            )
            .unwrap()
    }

    #[test]
    fn replaying_a_batch_is_idempotent() {
        // SYNC-010: at-least-once delivery must not duplicate.
        let mut store = Store::open_in_memory().unwrap();
        let device = DeviceId::new();
        let batch = PushRequest {
            device,
            ops: vec![envelope(device, EntityKind::Document, "d1", 1)],
        };
        let first = store.push(VaultId::nil(), &batch).unwrap();
        let second = store.push(VaultId::nil(), &batch).unwrap();
        assert_eq!(first.accepted, second.accepted);
        assert_eq!(store.count(VaultId::nil()).unwrap(), 1);
    }

    #[test]
    fn a_concurrent_edit_conflicts_and_returns_the_other_side() {
        // SYNC-009: same entity, same revision, different devices.
        let mut store = Store::open_in_memory().unwrap();
        let alice = DeviceId::new();
        let bob = DeviceId::new();
        push_one(&mut store, alice, EntityKind::Document, "d1", 2);
        let response = push_one(&mut store, bob, EntityKind::Document, "d1", 2);
        assert!(response.accepted.is_empty());
        assert_eq!(response.conflicts.len(), 1);
        // The server hands back Alice's envelope, so Bob can show both.
        assert_eq!(response.conflicts[0].theirs.op.revision, 2);
        assert_eq!(
            response.conflicts[0].theirs.op.timestamp.device, alice,
            "the stored head is the other device's"
        );

        // A genuinely newer revision from Bob is fine.
        let response = push_one(&mut store, bob, EntityKind::Document, "d1", 3);
        assert_eq!(response.accepted.len(), 1);
    }

    #[test]
    fn the_same_device_may_republish_its_own_entity() {
        // Crash between accept and acknowledge: the device re-pushes a
        // fresh op for the same revision. Not a conflict with itself.
        let mut store = Store::open_in_memory().unwrap();
        let alice = DeviceId::new();
        push_one(&mut store, alice, EntityKind::Document, "d1", 2);
        let response = push_one(&mut store, alice, EntityKind::Document, "d1", 2);
        assert_eq!(response.accepted.len(), 1);
        assert!(response.conflicts.is_empty());
    }

    #[test]
    fn assignment_shaped_entities_never_conflict() {
        // Both devices tagging the same note is agreement, not an edit war.
        let mut store = Store::open_in_memory().unwrap();
        let alice = DeviceId::new();
        let bob = DeviceId::new();
        push_one(&mut store, alice, EntityKind::DocumentTag, "d1/t1", 1);
        let response = push_one(&mut store, bob, EntityKind::DocumentTag, "d1/t1", 1);
        assert_eq!(response.accepted.len(), 1);
        assert!(response.conflicts.is_empty());
    }

    #[test]
    fn pull_pages_in_order_and_skips_the_pullers_own_operations() {
        let mut store = Store::open_in_memory().unwrap();
        let alice = DeviceId::new();
        let bob = DeviceId::new();
        push_one(&mut store, alice, EntityKind::Document, "a1", 1);
        push_one(&mut store, bob, EntityKind::Document, "b1", 1);
        push_one(&mut store, alice, EntityKind::Document, "a2", 1);

        let page = store.pull(VaultId::nil(), bob, 0, 10).unwrap();
        assert_eq!(page.ops.len(), 2, "bob does not re-hear his own op");
        assert!(page.ops.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(page.head, 3);

        // Cursor resume from the first entry.
        let after = page.ops[0].0;
        let rest = store.pull(VaultId::nil(), bob, after, 10).unwrap();
        assert_eq!(rest.ops.len(), 1);
    }
}
