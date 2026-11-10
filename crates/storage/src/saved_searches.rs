//! Saved searches (SRCH-006, TAG-015): named queries in the language
//! `marius-search` parses, nothing more exotic.

use rusqlite::{params, Connection, OptionalExtension};
use time::OffsetDateTime;

use crate::convert::time_to_sql;
use crate::Result;

pub fn save(conn: &Connection, name: &str, query: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO saved_search (name, query, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET query = excluded.query",
        params![name, query, time_to_sql(OffsetDateTime::now_utc())],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT query FROM saved_search WHERE name = ?1",
        params![name],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub fn list(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT name, query FROM saved_search ORDER BY name")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.execute("DELETE FROM saved_search WHERE name = ?1", params![name])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    #[test]
    fn saved_searches_round_trip_and_overwrite_by_name() {
        let db = Database::open_in_memory().unwrap();
        save(db.conn(), "inbox", "tag:inbox is:unresolved").unwrap();
        save(db.conn(), "inbox", "tag:inbox").unwrap();
        assert_eq!(get(db.conn(), "inbox").unwrap().unwrap(), "tag:inbox");
        assert_eq!(list(db.conn()).unwrap().len(), 1);
        assert!(remove(db.conn(), "inbox").unwrap());
        assert!(get(db.conn(), "inbox").unwrap().is_none());
    }
}
