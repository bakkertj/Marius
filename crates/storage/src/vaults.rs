//! Vault records and their inference policy.
//!
//! Requirements: KB-001, KB-008, KB-009, SEC-005, AI-016, SYNC-007, SYNC-012.

use marius_model::{
    ChangeKind, EntityKind, InferenceTask, Sensitivity, SyncBackend, Vault, VaultId, VaultPolicy,
};
use rusqlite::{params, Connection, OptionalExtension};

use crate::convert::{
    bool_to_sql, id_from_row, json_to_sql, parsed_from_row, time_from_row, time_to_sql, IdSql,
};
use crate::{changelog, Result, StorageError};

const COLUMNS: &str = "id, name, root, sync_backend, created_at, available, \
                       write_ids_to_frontmatter, default_sensitivity, disabled_tasks, \
                       allowed_providers, denied_providers, auto_classify";

/// Create or update a vault and its policy.
pub fn upsert(conn: &Connection, vault: &Vault, policy: &VaultPolicy) -> Result<u64> {
    if policy.vault != vault.id {
        return Err(StorageError::Corrupt(format!(
            "policy belongs to vault {} but was stored against {}",
            policy.vault, vault.id
        )));
    }
    let revision: i64 = conn.query_row(
        "INSERT INTO vault
            (id, name, root, sync_backend, created_at, available, write_ids_to_frontmatter,
             default_sensitivity, disabled_tasks, allowed_providers, denied_providers,
             auto_classify, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            root = excluded.root,
            sync_backend = excluded.sync_backend,
            available = excluded.available,
            write_ids_to_frontmatter = excluded.write_ids_to_frontmatter,
            default_sensitivity = excluded.default_sensitivity,
            disabled_tasks = excluded.disabled_tasks,
            allowed_providers = excluded.allowed_providers,
            denied_providers = excluded.denied_providers,
            auto_classify = excluded.auto_classify,
            revision = vault.revision + 1
         RETURNING revision",
        params![
            vault.id.to_sql_blob(),
            vault.name,
            vault.root.to_string_lossy(),
            json_to_sql(&vault.sync_backend)?,
            time_to_sql(vault.created_at),
            bool_to_sql(vault.available),
            bool_to_sql(vault.write_ids_to_frontmatter),
            policy.default_sensitivity.as_str(),
            json_to_sql(
                &policy
                    .disabled_tasks
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
            )?,
            policy
                .allowed_providers
                .as_ref()
                .map(json_to_sql)
                .transpose()?,
            json_to_sql(&policy.denied_providers)?,
            bool_to_sql(policy.auto_classify),
        ],
        |row| row.get(0),
    )?;

    changelog::record(
        conn,
        vault.id,
        EntityKind::Vault,
        &vault.id.to_string(),
        ChangeKind::Upsert,
        revision as u64,
    )?;
    Ok(revision as u64)
}

/// Fetch a vault and its policy.
pub fn get(conn: &Connection, id: VaultId) -> Result<Option<(Vault, VaultPolicy)>> {
    let sql = format!("SELECT {COLUMNS} FROM vault WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![id.to_sql_blob()], row_to_vault)
        .optional()?)
}

/// Fetch a vault by its root directory, which is how an "open this folder"
/// action finds an existing registration (KB-002).
pub fn by_root(conn: &Connection, root: &std::path::Path) -> Result<Option<(Vault, VaultPolicy)>> {
    let sql = format!("SELECT {COLUMNS} FROM vault WHERE root = ?1");
    Ok(conn
        .query_row(&sql, params![root.to_string_lossy()], row_to_vault)
        .optional()?)
}

/// Every registered vault, by name (KB-009).
pub fn list(conn: &Connection) -> Result<Vec<(Vault, VaultPolicy)>> {
    let sql = format!("SELECT {COLUMNS} FROM vault ORDER BY name COLLATE NOCASE");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], row_to_vault)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Mark a vault reachable or not, without touching its content (SYNC-012).
///
/// An unavailable vault keeps every row it owns, so cross-vault edges into it
/// survive an unmounted drive or a detached device (AC-009).
pub fn set_available(conn: &Connection, id: VaultId, available: bool) -> Result<()> {
    let revision: Option<i64> = conn
        .query_row(
            "UPDATE vault SET available = ?2, revision = revision + 1 WHERE id = ?1
             RETURNING revision",
            params![id.to_sql_blob(), bool_to_sql(available)],
            |row| row.get(0),
        )
        .optional()?;
    let Some(revision) = revision else {
        return Err(StorageError::NotFound {
            entity: "vault",
            id: id.to_string(),
        });
    };
    changelog::record(
        conn,
        id,
        EntityKind::Vault,
        &id.to_string(),
        ChangeKind::Upsert,
        revision as u64,
    )?;
    Ok(())
}

/// Remove a vault's registration and everything scoped to it.
///
/// The user's files on disk are never touched — KB-001 is explicit that
/// removing a vault does not delete its content.
pub fn remove(conn: &Connection, id: VaultId) -> Result<()> {
    let removed = conn.execute("DELETE FROM vault WHERE id = ?1", params![id.to_sql_blob()])?;
    if removed == 0 {
        return Err(StorageError::NotFound {
            entity: "vault",
            id: id.to_string(),
        });
    }
    changelog::record(
        conn,
        id,
        EntityKind::Vault,
        &id.to_string(),
        ChangeKind::Delete,
        0,
    )?;
    Ok(())
}

fn row_to_vault(row: &rusqlite::Row<'_>) -> rusqlite::Result<(Vault, VaultPolicy)> {
    let id: VaultId = id_from_row(row, 0)?;
    let backend_json: String = row.get(3)?;
    let sync_backend: SyncBackend = serde_json::from_str(&backend_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let disabled_raw: Vec<String> = crate::convert::json_from_row(row, 8)?;
    let disabled_tasks = disabled_raw
        .iter()
        .filter_map(|t| t.parse::<InferenceTask>().ok())
        .collect();

    let vault = Vault {
        id,
        name: row.get(1)?,
        root: std::path::PathBuf::from(row.get::<_, String>(2)?),
        sync_backend,
        created_at: time_from_row(row, 4)?,
        available: row.get::<_, i64>(5)? != 0,
        write_ids_to_frontmatter: row.get::<_, i64>(6)? != 0,
    };
    let policy = VaultPolicy {
        vault: id,
        default_sensitivity: parsed_from_row::<Sensitivity>(row, 7)?,
        disabled_tasks,
        allowed_providers: crate::convert::opt_json_from_row(row, 9)?,
        denied_providers: crate::convert::json_from_row(row, 10)?,
        auto_classify: row.get::<_, i64>(11)? != 0,
    };
    Ok((vault, policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use marius_model::{EntityKind, InferenceTask};

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn sample() -> (Vault, VaultPolicy) {
        let vault = Vault::new_local("Research", "/vaults/research");
        let policy = VaultPolicy::conservative(vault.id);
        (vault, policy)
    }

    #[test]
    fn a_vault_round_trips_with_its_policy() {
        let db = db();
        let (vault, mut policy) = sample();
        policy.default_sensitivity = Sensitivity::Confidential;
        policy.disabled_tasks = vec![InferenceTask::Summarization];
        policy.allowed_providers = Some(vec!["ollama".into()]);
        policy.denied_providers = vec!["openrouter".into()];
        policy.auto_classify = true;

        upsert(db.conn(), &vault, &policy).unwrap();
        let (got_vault, got_policy) = get(db.conn(), vault.id).unwrap().unwrap();
        assert_eq!(got_vault, vault);
        assert_eq!(got_policy, policy);
    }

    #[test]
    fn a_new_vault_defaults_to_conservative_policy() {
        // AI-007.
        let db = db();
        let (vault, policy) = sample();
        upsert(db.conn(), &vault, &policy).unwrap();
        let (_, got) = get(db.conn(), vault.id).unwrap().unwrap();
        assert_eq!(got.default_sensitivity, Sensitivity::Personal);
        assert!(!got.auto_classify, "classification is opt-in");
    }

    #[test]
    fn updating_a_vault_bumps_its_revision_and_journals_the_change() {
        let db = db();
        let (mut vault, policy) = sample();
        assert_eq!(upsert(db.conn(), &vault, &policy).unwrap(), 1);

        vault.name = "Renamed".into();
        assert_eq!(upsert(db.conn(), &vault, &policy).unwrap(), 2);

        let history =
            changelog::history(db.conn(), EntityKind::Vault, &vault.id.to_string()).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].revision, 2);
        assert_eq!(history[1].backend, "local-only");
    }

    #[test]
    fn the_journal_records_the_backend_a_vault_syncs_to() {
        // SYNC-008: a migration has to tell replayed history apart.
        let db = db();
        let (mut vault, policy) = sample();
        vault.sync_backend = SyncBackend::CloudKit {
            container: "iCloud.marius".into(),
        };
        upsert(db.conn(), &vault, &policy).unwrap();

        let history =
            changelog::history(db.conn(), EntityKind::Vault, &vault.id.to_string()).unwrap();
        assert_eq!(history[0].backend, "cloudkit");
    }

    #[test]
    fn a_policy_for_the_wrong_vault_is_refused() {
        let db = db();
        let (vault, _) = sample();
        let other = VaultPolicy::conservative(VaultId::new());
        assert!(upsert(db.conn(), &vault, &other).is_err());
    }

    #[test]
    fn vaults_are_found_by_root_so_reopening_a_folder_reuses_its_identity() {
        // KB-002: opening an existing directory must not mint a second vault.
        let db = db();
        let (vault, policy) = sample();
        upsert(db.conn(), &vault, &policy).unwrap();
        let found = by_root(db.conn(), std::path::Path::new("/vaults/research"))
            .unwrap()
            .unwrap();
        assert_eq!(found.0.id, vault.id);
        assert!(by_root(db.conn(), std::path::Path::new("/elsewhere"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn two_vaults_cannot_share_a_root() {
        let db = db();
        let (vault, policy) = sample();
        upsert(db.conn(), &vault, &policy).unwrap();

        let duplicate = Vault::new_local("Copy", "/vaults/research");
        let err = upsert(
            db.conn(),
            &duplicate,
            &VaultPolicy::conservative(duplicate.id),
        )
        .unwrap_err();
        assert!(err.is_conflict(), "{err:?}");
    }

    #[test]
    fn vaults_list_in_name_order() {
        // KB-009.
        let db = db();
        for name in ["Zettel", "archive", "Research"] {
            let vault = Vault::new_local(name, format!("/vaults/{name}"));
            upsert(db.conn(), &vault, &VaultPolicy::conservative(vault.id)).unwrap();
        }
        let names: Vec<String> = list(db.conn())
            .unwrap()
            .into_iter()
            .map(|(v, _)| v.name)
            .collect();
        assert_eq!(names, vec!["archive", "Research", "Zettel"]);
    }

    #[test]
    fn a_vault_can_be_marked_unavailable_without_losing_anything() {
        // SYNC-012 / AC-009.
        let db = db();
        let (vault, policy) = sample();
        upsert(db.conn(), &vault, &policy).unwrap();

        set_available(db.conn(), vault.id, false).unwrap();
        let (got, _) = get(db.conn(), vault.id).unwrap().unwrap();
        assert!(!got.available);
        assert_eq!(got.name, vault.name, "nothing else may change");

        set_available(db.conn(), vault.id, true).unwrap();
        assert!(get(db.conn(), vault.id).unwrap().unwrap().0.available);
    }

    #[test]
    fn removing_a_vault_journals_a_tombstone() {
        let db = db();
        let (vault, policy) = sample();
        upsert(db.conn(), &vault, &policy).unwrap();
        remove(db.conn(), vault.id).unwrap();

        assert!(get(db.conn(), vault.id).unwrap().is_none());
        let history =
            changelog::history(db.conn(), EntityKind::Vault, &vault.id.to_string()).unwrap();
        assert_eq!(history.last().unwrap().change, ChangeKind::Delete);
    }

    #[test]
    fn operations_on_a_missing_vault_report_not_found() {
        let db = db();
        let missing = VaultId::new();
        assert!(matches!(
            set_available(db.conn(), missing, false),
            Err(StorageError::NotFound { .. })
        ));
        assert!(matches!(
            remove(db.conn(), missing),
            Err(StorageError::NotFound { .. })
        ));
        assert!(get(db.conn(), missing).unwrap().is_none());
    }
}
