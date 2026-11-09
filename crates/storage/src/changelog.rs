//! The change journal.
//!
//! Requirements: SYNC-005, SYNC-009, SYNC-010, SYNC-011, SEC-006, AI-010, and
//! the spec's development note that sync-ready change tracking must exist from
//! the first phase.
//!
//! Every mutation in this crate appends exactly one entry here before it
//! returns. With no sync backend configured the journal is a local audit and
//! undo trail; when a backend is attached later the same rows are the
//! replication queue, so no retrofit is needed and no history is missing for
//! the period before sync was switched on.

use marius_model::{
    ChangeKind, DeviceId, EntityKind, LogicalTimestamp, OperationId, SyncOperation, SyncState,
    VaultId,
};
use rusqlite::{params, Connection, OptionalExtension};
use time::OffsetDateTime;

use crate::convert::{id_from_row, time_from_row, time_to_sql, IdSql};
use crate::{Result, StorageError};

/// The backend label used when a vault replicates nowhere.
pub const LOCAL_ONLY: &str = "local-only";

/// Append one entry to the journal.
///
/// `revision` is the entity's new revision counter, which is what a peer
/// compares against to detect a conflict (SYNC-009).
///
/// An operation for a vault with no sync backend is recorded as
/// [`SyncState::Acknowledged`] rather than `Pending`: with no remote, *this*
/// database is the authoritative backend, and the operation was accepted the
/// moment it committed. Leaving such rows `Pending` forever would be both
/// wrong — nothing is waiting to send them — and unbounded, since
/// [`prune_acknowledged`] could never reclaim them.
pub fn record(
    conn: &Connection,
    vault: VaultId,
    entity_kind: EntityKind,
    entity_id: &str,
    change: ChangeKind,
    revision: u64,
) -> Result<SyncOperation> {
    let timestamp = next_timestamp(conn)?;
    let backend = backend_label(conn, vault)?;
    let mut op = SyncOperation::new(
        vault,
        entity_kind,
        entity_id,
        change,
        revision,
        timestamp,
        backend,
    );
    if op.backend == LOCAL_ONLY {
        op.state = SyncState::Acknowledged;
    }
    insert(conn, &op)?;
    Ok(op)
}

fn insert(conn: &Connection, op: &SyncOperation) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_operation
            (id, vault_id, entity_kind, entity_id, change, revision,
             counter, device, state, backend, recorded_at, last_error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            op.id.to_sql_blob(),
            op.vault.to_sql_blob(),
            op.entity_kind.as_str(),
            op.entity_id,
            op.change.as_str(),
            op.revision as i64,
            op.timestamp.counter as i64,
            op.timestamp.device.to_sql_blob(),
            op.state.as_str(),
            op.backend,
            time_to_sql(op.recorded_at),
            op.last_error,
        ],
    )?;
    Ok(())
}

/// Advance this device's logical clock and return the new stamp.
///
/// The counter lives in the database rather than in memory so that it keeps
/// increasing across restarts and crashes; a counter that went backwards would
/// make two genuinely sequential edits look concurrent.
pub fn next_timestamp(conn: &Connection) -> Result<LogicalTimestamp> {
    let (counter, device): (i64, Vec<u8>) = conn.query_row(
        "UPDATE device SET counter = counter + 1 WHERE singleton = 1
         RETURNING counter, id",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(LogicalTimestamp::new(
        counter as u64,
        DeviceId::from_sql_blob(&device)?,
    ))
}

/// Observe a peer's clock, so the next local stamp sorts after it (SYNC-010).
pub fn observe(conn: &Connection, remote: LogicalTimestamp) -> Result<()> {
    conn.execute(
        "UPDATE device SET counter = MAX(counter, ?1) WHERE singleton = 1",
        params![remote.counter as i64],
    )?;
    Ok(())
}

/// The backend label a vault's operations are destined for (SYNC-008).
fn backend_label(conn: &Connection, vault: VaultId) -> Result<String> {
    let json: Option<String> = conn
        .query_row(
            "SELECT sync_backend FROM vault WHERE id = ?1",
            params![vault.to_sql_blob()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(json) = json else {
        // A vault row that is being created or has just been removed still
        // produces journal entries; label them local rather than failing.
        return Ok(LOCAL_ONLY.to_owned());
    };
    let backend: marius_model::SyncBackend = serde_json::from_str(&json)
        .map_err(|e| StorageError::Corrupt(format!("unreadable sync backend: {e}")))?;
    Ok(backend.label().to_owned())
}

/// Operations still needing attention from the sync engine (SYNC-011).
pub fn outstanding(conn: &Connection, limit: usize) -> Result<Vec<SyncOperation>> {
    let mut stmt = conn.prepare(
        "SELECT id, vault_id, entity_kind, entity_id, change, revision,
                counter, device, state, backend, recorded_at, last_error
         FROM sync_operation
         WHERE state IN ('pending', 'failed', 'conflicted')
         ORDER BY counter ASC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], row_to_operation)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every journal entry for one entity, oldest first. The undo trail.
pub fn history(
    conn: &Connection,
    entity_kind: EntityKind,
    entity_id: &str,
) -> Result<Vec<SyncOperation>> {
    let mut stmt = conn.prepare(
        "SELECT id, vault_id, entity_kind, entity_id, change, revision,
                counter, device, state, backend, recorded_at, last_error
         FROM sync_operation
         WHERE entity_kind = ?1 AND entity_id = ?2
         ORDER BY counter ASC",
    )?;
    let rows = stmt
        .query_map(params![entity_kind.as_str(), entity_id], row_to_operation)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Record the outcome of attempting to replicate an operation.
pub fn set_state(
    conn: &Connection,
    id: OperationId,
    state: SyncState,
    error: Option<&str>,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE sync_operation SET state = ?2, last_error = ?3 WHERE id = ?1",
        params![id.to_sql_blob(), state.as_str(), error],
    )?;
    if changed == 0 {
        return Err(StorageError::NotFound {
            entity: "sync operation",
            id: id.to_string(),
        });
    }
    Ok(())
}

/// Total journal rows, for the maintenance report.
pub fn count(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM sync_operation", [], |row| row.get(0))?;
    Ok(n as u64)
}

/// How many operations are in each state, for the sync status view (SYNC-011).
pub fn counts_by_state(conn: &Connection) -> Result<Vec<(SyncState, u64)>> {
    let mut stmt =
        conn.prepare("SELECT state, COUNT(*) FROM sync_operation GROUP BY state ORDER BY state")?;
    let rows = stmt
        .query_map([], |row| {
            let state: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((state, count))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(state, count)| {
            state
                .parse::<SyncState>()
                .map(|s| (s, count as u64))
                .map_err(|e| StorageError::Corrupt(e.to_string()))
        })
        .collect()
}

/// Discard acknowledged history older than `before`, keeping anything
/// outstanding.
///
/// Conflicted and failed operations are never pruned: they are the ones a user
/// still has to make a decision about (SYNC-009). Neither are pending ones,
/// which are still queued for a backend.
///
/// Pruning pre-sync history is safe: attaching a backend to a vault uploads
/// its *current state*, it does not replay the journal, so history older than
/// the retention window has no replication value. What it does have is audit
/// value (SEC-006), which is why the window exists rather than the journal
/// being discarded outright.
pub fn prune_acknowledged(conn: &Connection, before: OffsetDateTime) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM sync_operation WHERE state = 'acknowledged' AND recorded_at < ?1",
        params![time_to_sql(before)],
    )?)
}

fn row_to_operation(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncOperation> {
    Ok(SyncOperation {
        id: id_from_row(row, 0)?,
        vault: id_from_row(row, 1)?,
        entity_kind: crate::convert::parsed_from_row(row, 2)?,
        entity_id: row.get(3)?,
        change: crate::convert::parsed_from_row(row, 4)?,
        revision: row.get::<_, i64>(5)? as u64,
        timestamp: LogicalTimestamp::new(row.get::<_, i64>(6)? as u64, id_from_row(row, 7)?),
        state: crate::convert::parsed_from_row(row, 8)?,
        backend: row.get(9)?,
        recorded_at: time_from_row(row, 10)?,
        last_error: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use marius_model::DocumentId;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn the_logical_clock_only_ever_advances() {
        let db = db();
        let a = next_timestamp(db.conn()).unwrap();
        let b = next_timestamp(db.conn()).unwrap();
        assert!(b.counter > a.counter);
        assert_eq!(a.device, db.device_id());
    }

    #[test]
    fn the_clock_survives_a_reopen() {
        // A counter that restarted would make sequential edits look concurrent.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let highest = {
            let db = Database::open(&path).unwrap();
            next_timestamp(db.conn()).unwrap();
            next_timestamp(db.conn()).unwrap().counter
        };
        let db = Database::open(&path).unwrap();
        assert!(next_timestamp(db.conn()).unwrap().counter > highest);
    }

    #[test]
    fn observing_a_peer_pushes_the_local_clock_forward() {
        let db = db();
        observe(db.conn(), LogicalTimestamp::new(500, DeviceId::new())).unwrap();
        assert!(next_timestamp(db.conn()).unwrap().counter > 500);
    }

    #[test]
    fn observing_an_older_peer_does_not_rewind_the_clock() {
        let db = db();
        for _ in 0..5 {
            next_timestamp(db.conn()).unwrap();
        }
        observe(db.conn(), LogicalTimestamp::new(1, DeviceId::new())).unwrap();
        assert!(next_timestamp(db.conn()).unwrap().counter > 5);
    }

    #[test]
    fn operations_are_recorded_and_read_back() {
        let db = db();
        let doc = DocumentId::new();
        let op = record(
            db.conn(),
            VaultId::nil(),
            EntityKind::Document,
            &doc.to_string(),
            ChangeKind::Upsert,
            1,
        )
        .unwrap();
        // No backend means this database is the authority, so the operation
        // is acknowledged on commit rather than queued forever.
        assert_eq!(op.state, SyncState::Acknowledged);
        assert_eq!(op.backend, LOCAL_ONLY);

        let history = history(db.conn(), EntityKind::Document, &doc.to_string()).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, op.id);
        assert_eq!(history[0].change, ChangeKind::Upsert);
    }

    #[test]
    fn history_is_ordered_oldest_first() {
        let db = db();
        let doc = DocumentId::new().to_string();
        for revision in 1..=3 {
            record(
                db.conn(),
                VaultId::nil(),
                EntityKind::Document,
                &doc,
                ChangeKind::Upsert,
                revision,
            )
            .unwrap();
        }
        let revisions: Vec<u64> = history(db.conn(), EntityKind::Document, &doc)
            .unwrap()
            .iter()
            .map(|o| o.revision)
            .collect();
        assert_eq!(revisions, vec![1, 2, 3]);
    }

    #[test]
    fn deletions_are_journalled_as_tombstones() {
        // SYNC-010, integrity rule 7.
        let db = db();
        let doc = DocumentId::new().to_string();
        record(
            db.conn(),
            VaultId::nil(),
            EntityKind::Document,
            &doc,
            ChangeKind::Upsert,
            1,
        )
        .unwrap();
        record(
            db.conn(),
            VaultId::nil(),
            EntityKind::Document,
            &doc,
            ChangeKind::Delete,
            2,
        )
        .unwrap();
        let kinds: Vec<ChangeKind> = history(db.conn(), EntityKind::Document, &doc)
            .unwrap()
            .iter()
            .map(|o| o.change)
            .collect();
        assert_eq!(kinds, vec![ChangeKind::Upsert, ChangeKind::Delete]);
    }

    #[test]
    fn outstanding_lists_everything_not_yet_acknowledged() {
        let db = db();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(
                record(
                    db.conn(),
                    VaultId::nil(),
                    EntityKind::Tag,
                    &DocumentId::new().to_string(),
                    ChangeKind::Upsert,
                    1,
                )
                .unwrap()
                .id,
            );
        }
        // Local-only operations are acknowledged on commit, so nothing is
        // outstanding until something actually fails or is queued.
        assert!(outstanding(db.conn(), 10).unwrap().is_empty());

        set_state(db.conn(), ids[1], SyncState::Failed, Some("offline")).unwrap();
        set_state(db.conn(), ids[2], SyncState::Pending, None).unwrap();

        let out = outstanding(db.conn(), 10).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|o| o.state.is_outstanding()));
        assert_eq!(
            out.iter()
                .find(|o| o.id == ids[1])
                .unwrap()
                .last_error
                .as_deref(),
            Some("offline")
        );
    }

    #[test]
    fn outstanding_is_ordered_by_logical_time() {
        let db = db();
        for _ in 0..5 {
            record(
                db.conn(),
                VaultId::nil(),
                EntityKind::Document,
                &DocumentId::new().to_string(),
                ChangeKind::Upsert,
                1,
            )
            .unwrap();
        }
        let out = outstanding(db.conn(), 10).unwrap();
        assert!(out
            .windows(2)
            .all(|w| w[0].timestamp.counter < w[1].timestamp.counter));
    }

    #[test]
    fn setting_state_on_a_missing_operation_is_an_error() {
        let db = db();
        let err =
            set_state(db.conn(), OperationId::new(), SyncState::Acknowledged, None).unwrap_err();
        assert!(matches!(err, StorageError::NotFound { .. }));
    }

    #[test]
    fn pruning_keeps_everything_still_unresolved() {
        // SYNC-009: a conflict must not be silently discarded by housekeeping.
        let db = db();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(
                record(
                    db.conn(),
                    VaultId::nil(),
                    EntityKind::Document,
                    &DocumentId::new().to_string(),
                    ChangeKind::Upsert,
                    1,
                )
                .unwrap()
                .id,
            );
        }
        set_state(
            db.conn(),
            ids[1],
            SyncState::Conflicted,
            Some("both edited"),
        )
        .unwrap();
        set_state(db.conn(), ids[2], SyncState::Pending, None).unwrap();

        // ids[0] is still acknowledged and is the only prunable row.
        let future = OffsetDateTime::now_utc() + time::Duration::days(1);
        assert_eq!(prune_acknowledged(db.conn(), future).unwrap(), 1);

        let remaining = counts_by_state(db.conn()).unwrap();
        let total: u64 = remaining.iter().map(|(_, n)| n).sum();
        assert_eq!(total, 2);
        assert!(remaining.contains(&(SyncState::Conflicted, 1)));
        assert!(remaining.contains(&(SyncState::Pending, 1)));
    }

    #[test]
    fn a_vault_with_a_backend_still_queues_its_operations() {
        // The local-only shortcut must not leak into a vault that really does
        // replicate somewhere.
        let db = db();
        let vault = marius_model::Vault {
            sync_backend: marius_model::SyncBackend::SelfHosted {
                base_url: "https://sync.example".into(),
            },
            ..marius_model::Vault::new_local("Synced", "/synced")
        };
        crate::vaults::upsert(
            db.conn(),
            &vault,
            &marius_model::VaultPolicy::conservative(vault.id),
        )
        .unwrap();

        let op = record(
            db.conn(),
            vault.id,
            EntityKind::Document,
            &DocumentId::new().to_string(),
            ChangeKind::Upsert,
            1,
        )
        .unwrap();
        assert_eq!(op.state, SyncState::Pending);
        assert_eq!(op.backend, "self-hosted");

        // Registering the vault journalled an operation of its own, and that
        // one is queued too — both are genuinely waiting for the backend.
        let queued = outstanding(db.conn(), 10).unwrap();
        assert!(queued.iter().any(|o| o.id == op.id), "{queued:#?}");
        assert!(queued.iter().all(|o| o.backend == "self-hosted"));
        assert_eq!(queued.len(), 2);
    }

    #[test]
    fn a_local_only_journal_can_actually_be_pruned() {
        // The bug this fixes: with nothing ever acknowledging local-only rows,
        // `prune_acknowledged` reclaimed nothing and the journal grew without
        // bound — 73,263 rows for a single 10,000-document index.
        let db = db();
        for _ in 0..50 {
            record(
                db.conn(),
                VaultId::nil(),
                EntityKind::Document,
                &DocumentId::new().to_string(),
                ChangeKind::Upsert,
                1,
            )
            .unwrap();
        }
        assert_eq!(count(db.conn()).unwrap(), 50);

        let future = OffsetDateTime::now_utc() + time::Duration::days(1);
        assert_eq!(prune_acknowledged(db.conn(), future).unwrap(), 50);
        assert_eq!(count(db.conn()).unwrap(), 0);
    }
}
