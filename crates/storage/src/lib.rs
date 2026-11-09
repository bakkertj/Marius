//! # Marius storage
//!
//! The local SQLite database: metadata, relations, the search index and the
//! change journal (ARCH-006).
//!
//! ## What is authoritative, and what is not
//!
//! The spec's storage distinction is the organising rule of this crate:
//!
//! * **Markdown files are authoritative for their own content.** The database
//!   caches a document's title, text and hash so that search is fast, but a
//!   disagreement is always resolved in the file's favour (ARCH-005, KB-005).
//! * **The database is authoritative for metadata that exists nowhere else** —
//!   stable identities, AI tag provenance, suppressed relationships, review
//!   decisions, the sync journal. None of this can be recomputed, so it is the
//!   part that must never be lost.
//! * **Derived indexes are rebuildable.** [`rebuild_derived`] drops and
//!   recreates exactly the tables in [`migrations::DERIVED_TABLES`], which is
//!   what makes NFR-007 and integrity rule 3 true rather than aspirational.
//!
//! ## Change journalling
//!
//! Every mutating function in this crate appends a [`marius_model::SyncOperation`]
//! through [`changelog`], whether or not a sync backend is configured. The
//! spec's development note requires sync-ready change tracking from the first
//! phase; until a backend exists the journal is the undo and audit trail that
//! SEC-006 and AI-010 ask for.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod changelog;
pub mod chunks;
pub mod convert;
pub mod documents;
pub mod migrations;
pub mod relationships;
pub mod revisions;
pub mod search_index;
pub mod tags;
pub mod vaults;

use std::path::Path;

use marius_model::DeviceId;
use rusqlite::{OpenFlags, TransactionBehavior};

pub use convert::IdSql;
/// Re-exported so that crates above this one can hold a connection or a
/// transaction without taking a direct dependency on `rusqlite`, keeping the
/// choice of SQLite driver an implementation detail of this crate.
pub use rusqlite::{Connection, Transaction};

/// Anything that can go wrong talking to the database.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// SQLite itself failed.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A stored value could not be interpreted. Distinguished from a plain
    /// SQLite error because it means the database disagrees with this build's
    /// expectations, which is a migration or corruption problem rather than a
    /// transient failure.
    #[error("database contains unreadable data: {0}")]
    Corrupt(String),

    /// The database was written by a newer version of Marius.
    ///
    /// Refusing is the only safe response: an older build does not know what
    /// the newer columns mean, and writing anyway would lose them.
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },

    /// A row that must exist does not.
    #[error("{entity} {id} not found")]
    NotFound { entity: &'static str, id: String },

    /// A uniqueness rule was violated in a way the caller should handle.
    #[error("{entity} {value:?} already exists")]
    Conflict { entity: &'static str, value: String },
}

impl StorageError {
    /// True when the error is a uniqueness violation, which callers often want
    /// to turn into an "already exists" path rather than a failure.
    pub fn is_conflict(&self) -> bool {
        match self {
            Self::Conflict { .. } => true,
            Self::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => {
                e.code == rusqlite::ErrorCode::ConstraintViolation
            }
            _ => false,
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, StorageError>;

/// How durable writes should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// `synchronous = FULL`. Every commit is flushed. The default, because the
    /// database holds metadata that cannot be recomputed from the user's files
    /// — a lost rejection or a lost AI provenance record is lost for good.
    #[default]
    Full,
    /// `synchronous = NORMAL`. Used only for bulk indexing and import, where
    /// everything being written is derived from files that are still on disk
    /// and the work is resumable anyway (NFR-006).
    Bulk,
}

impl Durability {
    fn pragma(self) -> &'static str {
        match self {
            Self::Full => "FULL",
            Self::Bulk => "NORMAL",
        }
    }
}

/// How much space housekeeping reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceSizes {
    pub before_bytes: u64,
    pub after_bytes: u64,
}

/// An open local database.
pub struct Database {
    conn: Connection,
    device: DeviceId,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately minimal: the connection holds the user's whole index,
        // and a derived `Debug` would be a liability in any log line.
        f.debug_struct("Database")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Open or create the database at `path`, applying any pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::prepare(conn)
    }

    /// Open a private in-memory database. Used by tests and by index rebuild
    /// dry runs.
    pub fn open_in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Self> {
        // WAL lets a long read (a graph query, a search) run while the indexer
        // writes, which is what NFR-002 asks for.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", Durability::Full.pragma())?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Prefer waiting over failing when the indexer and the UI collide.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // 64 MiB of page cache; a 100k-document vault's indexes fit comfortably
        // and keeps NFR-004's 200 ms search budget reachable.
        conn.pragma_update(None, "cache_size", -65_536)?;

        let mut db = Self {
            conn,
            device: DeviceId::nil(),
        };
        db.migrate()?;
        db.device = db.load_or_create_device()?;
        Ok(db)
    }

    /// Apply pending migrations.
    fn migrate(&mut self) -> Result<()> {
        let found: u32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found > migrations::CURRENT_VERSION {
            return Err(StorageError::SchemaTooNew {
                found,
                supported: migrations::CURRENT_VERSION,
            });
        }
        for migration in migrations::MIGRATIONS {
            if migration.version <= found {
                continue;
            }
            tracing::info!(
                version = migration.version,
                name = migration.name,
                "applying migration"
            );
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(migration.sql)?;
            tx.pragma_update(None, "user_version", migration.version)?;
            tx.commit()?;
        }
        Ok(())
    }

    fn load_or_create_device(&self) -> Result<DeviceId> {
        if let Some(bytes) = self
            .conn
            .query_row("SELECT id FROM device WHERE singleton = 1", [], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?
        {
            return DeviceId::from_sql_blob(&bytes);
        }
        let device = DeviceId::new();
        self.conn.execute(
            "INSERT INTO device (singleton, id, name, counter) VALUES (1, ?1, ?2, 0)",
            rusqlite::params![device.to_sql_blob(), hostname()],
        )?;
        Ok(device)
    }

    /// This installation's identity, used to stamp every change (SYNC-010).
    pub fn device_id(&self) -> DeviceId {
        self.device
    }

    /// Read-only access for queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Begin a write transaction.
    ///
    /// `Immediate` acquires the write lock up front, so two writers fail fast
    /// instead of one discovering a conflict after doing its work.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        Ok(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    /// Run `f` in a transaction, committing on `Ok` and rolling back on `Err`.
    pub fn write<T>(&mut self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let tx = self.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Temporarily lower durability for a bulk operation, restoring it and
    /// checkpointing the WAL afterwards (NFR-006).
    pub fn with_bulk_durability<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.conn
            .pragma_update(None, "synchronous", Durability::Bulk.pragma())?;
        let out = f(self);
        // Restore durability even if the bulk operation failed.
        let restore = self
            .conn
            .pragma_update(None, "synchronous", Durability::Full.pragma());
        let checkpoint = self.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        let out = out?;
        restore?;
        checkpoint?;
        Ok(out)
    }

    /// Drop and recreate every derived table (NFR-007, integrity rule 3).
    ///
    /// Authoritative metadata is untouched: identities, tags, provenance,
    /// rejections and the journal all survive, so the caller only has to
    /// re-read the source files.
    pub fn rebuild_derived(&mut self) -> Result<()> {
        let tx = self.transaction()?;
        for table in migrations::DERIVED_TABLES {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.commit()?;
        // `document_fts` is a regular (not external-content) FTS5 table, so the
        // DELETE above *is* the rebuild and the caller repopulates it. Merging
        // the now-empty b-tree segments keeps the next bulk insert fast.
        self.conn
            .execute_batch("INSERT INTO document_fts(document_fts) VALUES('optimize')")?;
        Ok(())
    }

    /// Run SQLite's own integrity check (SEC-008).
    pub fn integrity_check(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("PRAGMA integrity_check")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().filter(|r| r != "ok").collect())
    }

    /// Back up the database to `path` while it stays open (SEC-008).
    pub fn backup_to(&self, path: impl AsRef<Path>) -> Result<()> {
        self.conn.backup(rusqlite::DatabaseName::Main, path, None)?;
        Ok(())
    }

    /// Reclaim space and refresh query-planner statistics.
    ///
    /// The order matters: write first (`optimize`, `ANALYZE`), then `VACUUM`
    /// to reclaim, then checkpoint to empty the write-ahead log. Housekeeping
    /// that leaves the database larger is not housekeeping, and getting this
    /// order wrong does exactly that.
    ///
    /// `VACUUM` rewrites the whole file, so this is a deliberate, user-invoked
    /// operation (`marius maintain`), never something that happens behind an
    /// ordinary index pass.
    pub fn maintain(&self) -> Result<MaintenanceSizes> {
        let before = self.file_size();
        self.conn.execute_batch(
            "INSERT INTO document_fts(document_fts) VALUES('optimize');\nANALYZE;",
        )?;
        self.conn.execute_batch("VACUUM;")?;
        // The checkpoint comes *last*: `VACUUM` rewrites the whole database
        // through the write-ahead log, so truncating the log before it runs
        // leaves the reclaimed space sitting in a fresh multi-megabyte `-wal`
        // instead.
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(MaintenanceSizes {
            before_bytes: before,
            after_bytes: self.file_size(),
        })
    }

    /// Size of the database and its write-ahead log, in bytes.
    pub fn file_size(&self) -> u64 {
        // An in-memory database has no path and no size to report.
        let Some(path) = self.conn.path().map(str::to_owned) else {
            return 0;
        };
        let main = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let wal = std::fs::metadata(format!("{path}-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        main + wal
    }
}

/// Commit a transaction, mapping the driver's error into [`StorageError`].
///
/// Exists so that crates above this one can commit without naming `rusqlite`
/// in their own error types.
pub fn commit(tx: Transaction<'_>) -> Result<()> {
    tx.commit()?;
    Ok(())
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().expect("in-memory database")
    }

    #[test]
    fn a_fresh_database_is_migrated_to_the_current_version() {
        let db = db();
        let version: u32 = db
            .conn()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, migrations::CURRENT_VERSION);
    }

    #[test]
    fn every_expected_table_exists() {
        let db = db();
        let mut stmt = db
            .conn()
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view') ORDER BY name")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        for expected in [
            "vault",
            "document",
            "document_alias",
            "document_name",
            "tag",
            "tag_alias",
            "document_tag",
            "rejected_tag",
            "relationship",
            "rejected_relationship",
            "pending_link",
            "document_revision",
            "revision_body",
            "generated_note_provenance",
            "inference_record",
            "document_policy",
            "device",
            "sync_operation",
            "plugin_installation",
            "document_fts",
            "chunk",
            "chunk_embedding",
            "saved_search",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marius.db");
        let first = Database::open(&path).unwrap();
        let device = first.device_id();
        drop(first);

        let second = Database::open(&path).unwrap();
        assert_eq!(
            second.device_id(),
            device,
            "the device identity must be stable across opens"
        );
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_downgraded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marius.db");
        {
            let db = Database::open(&path).unwrap();
            db.conn()
                .pragma_update(None, "user_version", migrations::CURRENT_VERSION + 5)
                .unwrap();
        }
        let err = Database::open(&path).unwrap_err();
        assert!(
            matches!(err, StorageError::SchemaTooNew { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn foreign_keys_are_enforced() {
        // Integrity rules depend on cascades actually firing.
        let db = db();
        let on: i64 = db
            .conn()
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }

    #[test]
    fn the_journal_is_write_ahead_so_reads_do_not_block_writes() {
        // NFR-002.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("marius.db")).unwrap();
        let mode: String = db
            .conn()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn a_device_identity_is_created_once() {
        let db = db();
        assert!(!db.device_id().is_nil());
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM device", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn maintenance_does_not_grow_the_database() {
        // Housekeeping that leaves the file larger is not housekeeping.
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::open(dir.path().join("marius.db")).unwrap();
        db.write(|tx| {
            for i in 0..2_000 {
                tx.execute(
                    "INSERT INTO saved_search (name, query, created_at) VALUES (?1, ?2, 'now')",
                    rusqlite::params![format!("q{i}"), "x".repeat(200)],
                )?;
            }
            Ok(())
        })
        .unwrap();
        db.write(|tx| {
            tx.execute("DELETE FROM saved_search", [])?;
            Ok(())
        })
        .unwrap();

        let sizes = db.maintain().unwrap();
        assert!(
            sizes.after_bytes <= sizes.before_bytes,
            "maintain grew the file from {} to {} bytes",
            sizes.before_bytes,
            sizes.after_bytes
        );
        assert!(db.integrity_check().unwrap().is_empty());
    }

    #[test]
    fn integrity_check_passes_on_a_fresh_database() {
        assert!(db().integrity_check().unwrap().is_empty());
    }

    #[test]
    fn a_backup_can_be_reopened() {
        // SEC-008.
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.db");
        let db = db();
        db.backup_to(&backup).unwrap();

        let restored = Database::open(&backup).unwrap();
        assert!(restored.integrity_check().unwrap().is_empty());
    }

    #[test]
    fn bulk_durability_is_restored_afterwards() {
        let mut db = db();
        db.with_bulk_durability(|_| Ok(())).unwrap();
        let sync: i64 = db
            .conn()
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(sync, 2, "synchronous must return to FULL");
    }

    #[test]
    fn bulk_durability_is_restored_even_when_the_body_fails() {
        let mut db = db();
        let err: Result<()> = db.with_bulk_durability(|_| {
            Err(StorageError::NotFound {
                entity: "document",
                id: "x".into(),
            })
        });
        assert!(err.is_err());
        let sync: i64 = db
            .conn()
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(sync, 2);
    }

    #[test]
    fn a_failed_write_rolls_back() {
        let mut db = db();
        let result: Result<()> = db.write(|tx| {
            tx.execute(
                "INSERT INTO saved_search (name, query, created_at) VALUES ('a', 'b', 'c')",
                [],
            )?;
            Err(StorageError::Corrupt("deliberate".into()))
        });
        assert!(result.is_err());
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM saved_search", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "the failed transaction must leave nothing behind");
    }

    #[test]
    fn constraint_violations_are_recognised_as_conflicts() {
        let mut db = db();
        db.write(|tx| {
            tx.execute(
                "INSERT INTO saved_search (name, query, created_at) VALUES ('a', 'b', 'c')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let err = db
            .write(|tx| {
                tx.execute(
                    "INSERT INTO saved_search (name, query, created_at) VALUES ('a', 'x', 'y')",
                    [],
                )?;
                Ok(())
            })
            .unwrap_err();
        assert!(err.is_conflict(), "{err:?}");
    }
}
