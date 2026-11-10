//! Plugin installations (PLUG-001, PLUG-007).
//!
//! An installation starts disabled with no capabilities; the user's
//! grants are the only thing the runtime consults, and a manifest
//! request is never a grant.

use marius_model::{Capability, PluginInstallation, PluginInstallationId, PluginManifest};
use rusqlite::{params, Connection, OptionalExtension};

use crate::convert::{time_from_row, time_to_sql, IdSql};
use crate::{Result, StorageError};

const COLUMNS: &str =
    "id, plugin_id, manifest, install_path, enabled, granted, diagnostics, installed_at";

pub fn upsert(conn: &Connection, installation: &PluginInstallation) -> Result<()> {
    conn.execute(
        "INSERT INTO plugin_installation
            (id, plugin_id, manifest, install_path, enabled, granted, diagnostics, installed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(plugin_id) DO UPDATE SET
            manifest = excluded.manifest,
            install_path = excluded.install_path,
            enabled = excluded.enabled,
            granted = excluded.granted,
            diagnostics = excluded.diagnostics",
        params![
            installation.id.to_sql_blob(),
            installation.manifest.id,
            json(&installation.manifest)?,
            installation.install_path.to_string_lossy(),
            installation.enabled as i64,
            json(&installation.granted)?,
            json(&installation.diagnostics)?,
            time_to_sql(installation.installed_at),
        ],
    )?;
    Ok(())
}

pub fn list(conn: &Connection) -> Result<Vec<PluginInstallation>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM plugin_installation ORDER BY plugin_id"
    ))?;
    let rows = stmt
        .query_map([], row_to_installation)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn by_plugin_id(conn: &Connection, plugin_id: &str) -> Result<Option<PluginInstallation>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM plugin_installation WHERE plugin_id = ?1"),
        params![plugin_id],
        row_to_installation,
    )
    .optional()
    .map_err(Into::into)
}

pub fn remove(conn: &Connection, plugin_id: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM plugin_installation WHERE plugin_id = ?1",
        params![plugin_id],
    )? > 0)
}

fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| StorageError::Corrupt(e.to_string()))
}

fn row_to_installation(
    row: &rusqlite::Row<'_>,
) -> std::result::Result<PluginInstallation, rusqlite::Error> {
    let manifest: String = row.get(2)?;
    let manifest: PluginManifest =
        serde_json::from_str(&manifest).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let granted: String = row.get(5)?;
    let granted: Vec<Capability> = serde_json::from_str(&granted).unwrap_or_default();
    let diagnostics: String = row.get(6)?;
    let install_path: String = row.get(3)?;
    Ok(PluginInstallation {
        id: {
            let raw: Vec<u8> = row.get(0)?;
            PluginInstallationId::from_sql_blob(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?
        },
        manifest,
        install_path: install_path.into(),
        enabled: row.get::<_, i64>(4)? != 0,
        granted,
        diagnostics: serde_json::from_str(&diagnostics).unwrap_or_default(),
        installed_at: time_from_row(row, 7)?,
    })
}
