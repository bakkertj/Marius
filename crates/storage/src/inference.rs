//! The inference audit log (AI-015, SEC-006, AC-016).
//!
//! Every request the policy engine *considers* gets a row — including the
//! refused ones. A denial is the most important entry to be able to show a
//! user who asks whether their confidential vault stayed on the device.

use marius_model::{
    GlobalDocumentId, InferenceId, InferenceOutcome, InferenceRecord, Locality, Sensitivity,
};
use rusqlite::{params, Connection};

use crate::convert::{id_from_row, time_from_row, time_to_sql, IdSql};
use crate::{Result, StorageError};

const COLUMNS: &str = "id, task, provider, model, locality, sensitivity, documents, outcome, \
                       reason, prompt_tokens, completion_tokens, cost, currency, started_at, \
                       duration_ms";

/// Append one audit row. The log is append-only by convention: nothing in
/// the application updates or deletes a record once written.
pub fn record(conn: &Connection, record: &InferenceRecord) -> Result<()> {
    let documents = serde_json::to_string(&record.documents)
        .map_err(|e| StorageError::Corrupt(format!("documents not serialisable: {e}")))?;
    conn.execute(
        "INSERT INTO inference_record (id, task, provider, model, locality, sensitivity, \
         documents, outcome, reason, prompt_tokens, completion_tokens, cost, currency, \
         started_at, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            record.id.to_sql_blob(),
            record.task.as_str(),
            record.provider,
            record.model,
            record.locality.map(|l| l.as_str()),
            record.sensitivity.as_str(),
            documents,
            record.outcome.as_str(),
            record.reason,
            record.prompt_tokens,
            record.completion_tokens,
            record.cost,
            record.currency,
            time_to_sql(record.started_at),
            record.duration_ms.map(|d| d as i64),
        ],
    )?;
    Ok(())
}

/// The most recent rows, newest first.
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<InferenceRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM inference_record ORDER BY started_at DESC, id DESC LIMIT ?1"
    ))?;
    let rows = stmt
        .query_map([limit as i64], row_to_record)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One record by id.
pub fn get(conn: &Connection, id: InferenceId) -> Result<Option<InferenceRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM inference_record WHERE id = ?1"
    ))?;
    let mut rows = stmt.query_map([id.to_sql_blob()], row_to_record)?;
    rows.next().transpose().map_err(Into::into)
}

/// How many rows record content actually leaving the device — the number a
/// privacy audit starts from (SEC-006).
pub fn transmissions(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM inference_record
         WHERE outcome IN ('completed', 'provider_error')
           AND locality IN ('self_hosted', 'remote')",
        [],
        |row| row.get(0),
    )?;
    Ok(n as u64)
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<InferenceRecord> {
    let documents: String = row.get(6)?;
    let documents: Vec<GlobalDocumentId> = serde_json::from_str(&documents).unwrap_or_default();
    Ok(InferenceRecord {
        id: id_from_row(row, 0)?,
        task: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        locality: row
            .get::<_, Option<String>>(4)?
            .map(|s| s.parse::<Locality>())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        sensitivity: row
            .get::<_, String>(5)?
            .parse::<Sensitivity>()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        documents,
        outcome: row
            .get::<_, String>(7)?
            .parse::<InferenceOutcome>()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        reason: row.get(8)?,
        prompt_tokens: row.get(9)?,
        completion_tokens: row.get(10)?,
        cost: row.get(11)?,
        currency: row.get(12)?,
        started_at: time_from_row(row, 13)?,
        duration_ms: row.get::<_, Option<i64>>(14)?.map(|d| d as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use marius_model::InferenceTask;

    #[test]
    fn records_round_trip_and_order_newest_first() {
        let db = Database::open_in_memory().unwrap();
        let denied = InferenceRecord::denied(
            InferenceTask::Tagging,
            Sensitivity::NoInference,
            vec![],
            "document is marked no_inference",
        );
        record(db.conn(), &denied).unwrap();

        let mut completed = InferenceRecord::denied(
            InferenceTask::Summarization,
            Sensitivity::Personal,
            vec![],
            "sent to trusted local model",
        );
        completed.id = InferenceId::new();
        completed.provider = Some("ollama".into());
        completed.model = Some("llama3.2".into());
        completed.locality = Some(Locality::Local);
        completed.outcome = InferenceOutcome::Completed;
        completed.started_at = denied.started_at + time::Duration::seconds(5);
        completed.duration_ms = Some(1200);
        record(db.conn(), &completed).unwrap();

        let rows = recent(db.conn(), 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], completed, "newest first");
        assert_eq!(rows[1], denied);
        assert_eq!(get(db.conn(), denied.id).unwrap().unwrap(), denied);

        // Local completion is not a transmission; a denial never is.
        assert_eq!(transmissions(db.conn()).unwrap(), 0);
    }

    #[test]
    fn transmissions_count_only_content_that_left() {
        let db = Database::open_in_memory().unwrap();
        let mut sent = InferenceRecord::denied(
            InferenceTask::QuestionAnswering,
            Sensitivity::Unrestricted,
            vec![],
            "remote provider answered",
        );
        sent.provider = Some("openrouter".into());
        sent.locality = Some(Locality::Remote);
        sent.outcome = InferenceOutcome::Completed;
        record(db.conn(), &sent).unwrap();

        // A provider error after transmission still counts: the content left.
        let mut failed = sent.clone();
        failed.id = InferenceId::new();
        failed.outcome = InferenceOutcome::ProviderError;
        record(db.conn(), &failed).unwrap();

        assert_eq!(transmissions(db.conn()).unwrap(), 2);
        assert!(sent.left_the_device());
    }
}
