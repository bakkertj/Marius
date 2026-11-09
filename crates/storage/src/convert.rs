//! Conversions between domain types and SQLite values.
//!
//! The orphan rule prevents implementing `ToSql`/`FromSql` for types owned by
//! `marius-model`, so conversion is explicit helper functions. That turns out
//! to be an advantage: the storage representation of an identity or a timestamp
//! is a compatibility contract with every other device (SYNC-010), and having
//! it written out in one readable place is worth more than the syntactic
//! convenience of a trait impl.
//!
//! Representation choices:
//!
//! * **Identities** are 16-byte `BLOB`s. Half the size of the hyphenated text
//!   form and directly comparable, which matters for the index on every join
//!   column.
//! * **Timestamps** are RFC 3339 strings normalised to UTC. Lexicographic
//!   order then equals chronological order, so `ORDER BY modified_at` and
//!   `WHERE created_at > ?` work without a conversion function, and the value
//!   stays readable when someone opens the database with `sqlite3`.
//! * **Enums** are their stable `as_str()` tokens, never ordinals. An ordinal
//!   would silently change meaning the day a variant is inserted.

use marius_model::{ContentHash, DeviceId, DocumentId, VaultPath};
use rusqlite::types::{FromSqlError, Value, ValueRef};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::StorageError;

/// Convert any typed identity to its storage value.
///
/// Generic over the 16-byte accessor every identity type shares.
macro_rules! id_sql {
    ($($ty:ty),* $(,)?) => {
        $(
            impl crate::convert::IdSql for $ty {
                fn to_sql_blob(&self) -> Vec<u8> {
                    self.as_bytes().to_vec()
                }
                fn from_sql_blob(bytes: &[u8]) -> Result<Self, StorageError> {
                    let array: [u8; 16] = bytes.try_into().map_err(|_| {
                        StorageError::Corrupt(format!(
                            "{} identifier is {} bytes, expected 16",
                            <$ty>::KIND,
                            bytes.len()
                        ))
                    })?;
                    Ok(Self::from_bytes(array))
                }
            }
        )*
    };
}

/// A typed identity that can round-trip through a SQLite `BLOB`.
pub trait IdSql: Sized {
    fn to_sql_blob(&self) -> Vec<u8>;
    fn from_sql_blob(bytes: &[u8]) -> Result<Self, StorageError>;
}

id_sql!(
    marius_model::VaultId,
    marius_model::DocumentId,
    marius_model::TagId,
    marius_model::RelationshipId,
    marius_model::RevisionId,
    marius_model::ChunkId,
    marius_model::OperationId,
    marius_model::DeviceId,
    marius_model::InferenceId,
    marius_model::PluginInstallationId,
);

/// Read a typed identity from a row column.
pub fn id_from_row<T: IdSql>(row: &rusqlite::Row<'_>, index: usize) -> Result<T, rusqlite::Error> {
    let bytes: Vec<u8> = row.get(index)?;
    T::from_sql_blob(&bytes).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Blob, Box::new(e))
    })
}

/// Format a timestamp for storage, normalised to UTC so that string order and
/// chronological order agree.
pub fn time_to_sql(value: OffsetDateTime) -> String {
    value
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Parse a stored timestamp.
pub fn time_from_sql(text: &str) -> Result<OffsetDateTime, StorageError> {
    OffsetDateTime::parse(text, &Rfc3339)
        .map_err(|e| StorageError::Corrupt(format!("invalid timestamp {text:?}: {e}")))
}

/// Read a timestamp from a row column.
pub fn time_from_row(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<OffsetDateTime, rusqlite::Error> {
    let text: String = row.get(index)?;
    time_from_sql(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Read an optional timestamp.
pub fn opt_time_from_row(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<OffsetDateTime>, rusqlite::Error> {
    match row.get::<_, Option<String>>(index)? {
        Some(text) => time_from_row_text(&text, index).map(Some),
        None => Ok(None),
    }
}

fn time_from_row_text(text: &str, index: usize) -> Result<OffsetDateTime, rusqlite::Error> {
    time_from_sql(text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Parse an enum token from a row, reporting the column on failure.
pub fn parsed_from_row<T>(row: &rusqlite::Row<'_>, index: usize) -> Result<T, rusqlite::Error>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let text: String = row.get(index)?;
    text.parse().map_err(|e: T::Err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Content hashes are 32-byte `BLOB`s.
pub fn hash_to_sql(hash: &ContentHash) -> Vec<u8> {
    hash.as_bytes().to_vec()
}

/// Read a content hash from a row column.
pub fn hash_from_row(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<ContentHash, rusqlite::Error> {
    let bytes: Vec<u8> = row.get(index)?;
    let array: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(StorageError::Corrupt(format!(
                "content hash is {} bytes, expected 32",
                bytes.len()
            ))),
        )
    })?;
    Ok(ContentHash::from_bytes(array))
}

/// Read a vault-relative path from a row column, validating it.
pub fn path_from_row(row: &rusqlite::Row<'_>, index: usize) -> Result<VaultPath, rusqlite::Error> {
    let text: String = row.get(index)?;
    VaultPath::new(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Serialise a value as JSON for a `TEXT` column.
///
/// Used for the handful of fields that are genuinely open-ended — frontmatter,
/// citation lists, provider allow-lists. Anything that is queried gets its own
/// column instead.
pub fn json_to_sql<T: serde::Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|e| StorageError::Corrupt(format!("cannot encode: {e}")))
}

/// Read a JSON column.
pub fn json_from_row<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<T, rusqlite::Error> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Read a JSON column that may be `NULL`.
pub fn opt_json_from_row<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<T>, rusqlite::Error> {
    match row.get::<_, Option<String>>(index)? {
        Some(text) => serde_json::from_str(&text).map(Some).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        }),
        None => Ok(None),
    }
}

/// SQLite has no boolean type; 0 and 1 are the convention.
pub fn bool_to_sql(value: bool) -> i64 {
    i64::from(value)
}

/// Encode a `f32` confidence as a SQLite `REAL`.
pub fn opt_f32_to_sql(value: Option<f32>) -> Option<f64> {
    value.map(f64::from)
}

/// Decode a nullable `REAL` as a confidence.
pub fn opt_confidence_from_row(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<marius_model::Confidence>, rusqlite::Error> {
    Ok(row
        .get::<_, Option<f64>>(index)?
        .map(|v| marius_model::Confidence::new(v as f32)))
}

/// The all-zero vault identity, used as the "no vault" sentinel in columns that
/// must take part in a `UNIQUE` constraint.
///
/// SQLite treats `NULL`s as distinct for uniqueness, so a nullable `vault_id`
/// would let two global tags named `project` both exist. The nil UUID is
/// already the documented sentinel in the model, so it is reused here.
pub fn vault_or_nil(vault: Option<marius_model::VaultId>) -> Vec<u8> {
    vault
        .unwrap_or_else(marius_model::VaultId::nil)
        .to_sql_blob()
}

/// Inverse of [`vault_or_nil`].
pub fn vault_from_nil(vault: marius_model::VaultId) -> Option<marius_model::VaultId> {
    (!vault.is_nil()).then_some(vault)
}

/// Convert a `ValueRef` to an owned `Value`, for dynamic query building.
pub fn owned(value: ValueRef<'_>) -> Result<Value, FromSqlError> {
    Ok(value.into())
}

/// A device identity and a document identity, the pair most joins need.
pub type DeviceAndDocument = (DeviceId, DocumentId);

#[cfg(test)]
mod tests {
    use super::*;
    use marius_model::{DocumentId, VaultId};

    #[test]
    fn identities_round_trip_through_blobs() {
        let id = DocumentId::new();
        let blob = id.to_sql_blob();
        assert_eq!(blob.len(), 16);
        assert_eq!(DocumentId::from_sql_blob(&blob).unwrap(), id);
    }

    #[test]
    fn a_wrong_sized_identity_blob_is_reported_as_corruption() {
        let err = DocumentId::from_sql_blob(&[0u8; 8]).unwrap_err();
        assert!(matches!(err, StorageError::Corrupt(_)), "{err:?}");
        assert!(err.to_string().contains("document"));
    }

    #[test]
    fn timestamps_are_stored_in_utc_so_text_order_is_time_order() {
        let later = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let earlier = OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap();
        assert!(time_to_sql(earlier) < time_to_sql(later));

        // A non-UTC input must still serialise as UTC.
        let offset = later.to_offset(time::UtcOffset::from_hms(5, 30, 0).unwrap());
        assert_eq!(time_to_sql(offset), time_to_sql(later));
        assert!(time_to_sql(offset).ends_with('Z'));
    }

    #[test]
    fn timestamps_round_trip() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(time_from_sql(&time_to_sql(now)).unwrap(), now);
    }

    #[test]
    fn a_malformed_timestamp_is_reported_not_defaulted() {
        assert!(time_from_sql("yesterday").is_err());
    }

    #[test]
    fn the_nil_vault_sentinel_round_trips_to_none() {
        assert_eq!(vault_from_nil(VaultId::nil()), None);
        let real = VaultId::new();
        assert_eq!(vault_from_nil(real), Some(real));
        assert_eq!(vault_or_nil(None), VaultId::nil().to_sql_blob());
        assert_eq!(vault_or_nil(Some(real)), real.to_sql_blob());
    }
}
