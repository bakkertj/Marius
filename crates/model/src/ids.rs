//! Typed, stable identities.
//!
//! Requirements: KB-006 (stable globally unique document and vault IDs),
//! KB-011/ARCH-011 (identity independent of filename, path and vault moves),
//! GRAPH-009 (every relationship has a stable ID), data integrity rules 1 and 2.
//!
//! Every identity is a UUIDv7 so that identifiers sort by creation time — this
//! gives cheap chronological pagination and stable ordering in the sync log
//! without a second column — but no part of the system may *depend* on the
//! embedded timestamp: an ID is an opaque token everywhere above this module.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Error returned when a string cannot be parsed as a typed identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} identifier: {input:?}")]
pub struct IdParseError {
    pub kind: &'static str,
    pub input: String,
}

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// The entity kind this identifier namespaces, for diagnostics.
            pub const KIND: &'static str = $kind;

            /// Mint a new, time-ordered identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wrap an existing UUID (used when reading from storage or a peer).
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            /// Byte representation used by the SQLite `BLOB` columns.
            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            /// Reconstruct from the 16-byte storage representation.
            pub fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(bytes))
            }

            /// The all-zero identifier, reserved as a sentinel for "absent".
            pub const fn nil() -> Self {
                Self(Uuid::nil())
            }

            /// True for the reserved sentinel value.
            pub fn is_nil(&self) -> bool {
                self.0.is_nil()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Hyphenated lower-case: the form written into frontmatter and
                // exported files, so it must stay byte-stable forever.
                fmt::Display::fmt(&self.0.as_hyphenated(), f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s.trim()).map(Self).map_err(|_| IdParseError {
                    kind: $kind,
                    input: s.to_owned(),
                })
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }
    };
}

typed_id!(
    /// Identifies a vault. Survives moving the vault's root directory (KB-001, KB-008).
    VaultId, "vault"
);
typed_id!(
    /// Identifies a document for its whole life, across renames, moves between
    /// folders, and moves between vaults (KB-006, AC-003).
    DocumentId, "document"
);
typed_id!(
    /// Identifies a tag independently of its canonical name, so a global rename
    /// (TAG-010) does not break existing assignments.
    TagId, "tag"
);
typed_id!(
    /// Identifies one graph edge (GRAPH-009).
    RelationshipId, "relationship"
);
typed_id!(
    /// Identifies one stored revision of a document (KB-011).
    RevisionId, "revision"
);
typed_id!(
    /// Identifies one embedded chunk of a document (SRCH-008).
    ChunkId, "chunk"
);
typed_id!(
    /// Identifies a single replicated change (SYNC-010).
    OperationId, "operation"
);
typed_id!(
    /// Identifies this installation on this device (SYNC-010).
    DeviceId, "device"
);
typed_id!(
    /// Identifies an inference request, for the audit log (SEC-006, AI-015).
    InferenceId, "inference"
);
typed_id!(
    /// Identifies an installed plugin package (PLUG-001).
    PluginInstallationId, "plugin-installation"
);

/// A document reference that is valid across vaults (GRAPH-008, integrity rule 2).
///
/// Cross-vault edges must keep working while the far vault is detached, so the
/// reference carries the vault ID alongside the document ID and never a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GlobalDocumentId {
    pub vault: VaultId,
    pub document: DocumentId,
}

impl GlobalDocumentId {
    pub const fn new(vault: VaultId, document: DocumentId) -> Self {
        Self { vault, document }
    }
}

impl fmt::Display for GlobalDocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.vault, self.document)
    }
}

impl FromStr for GlobalDocumentId {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || IdParseError {
            kind: "global document",
            input: s.to_owned(),
        };
        let (vault, document) = s.split_once('/').ok_or_else(err)?;
        Ok(Self {
            vault: vault.parse().map_err(|_| err())?,
            document: document.parse().map_err(|_| err())?,
        })
    }
}

/// Content hash of a document body (KB-007).
///
/// BLAKE3 over the exact bytes on disk. Used to detect external modification
/// (KB-005), to recognise a moved file as the same document (AC-003), and to
/// find duplicate imports (IM-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ContentHash {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || IdParseError {
            kind: "content hash",
            input: s.to_owned(),
        };
        if s.len() != 64 {
            return Err(err());
        }
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| err())?;
        }
        Ok(Self(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_text() {
        let id = DocumentId::new();
        assert_eq!(id, id.to_string().parse::<DocumentId>().unwrap());
    }

    #[test]
    fn ids_round_trip_through_storage_bytes() {
        let id = VaultId::new();
        assert_eq!(id, VaultId::from_bytes(*id.as_bytes()));
    }

    #[test]
    fn ids_are_time_ordered() {
        let mut ids: Vec<DocumentId> = (0..64).map(|_| DocumentId::new()).collect();
        let minted = ids.clone();
        ids.sort();
        // UUIDv7 has millisecond granularity plus a monotonic counter, so a tight
        // minting loop must still come back in the order it was created.
        assert_eq!(ids, minted);
    }

    #[test]
    fn parsing_rejects_foreign_text() {
        let err = "not-an-id".parse::<TagId>().unwrap_err();
        assert_eq!(err.kind, "tag");
    }

    #[test]
    fn global_ids_round_trip() {
        let gid = GlobalDocumentId::new(VaultId::new(), DocumentId::new());
        assert_eq!(gid, gid.to_string().parse().unwrap());
    }

    #[test]
    fn content_hash_round_trips_and_distinguishes_content() {
        let a = ContentHash::of(b"# Lighthouse\n");
        let b = ContentHash::of(b"# Lighthouse\n\n");
        assert_ne!(a, b);
        assert_eq!(a, a.to_string().parse().unwrap());
        assert_eq!(a.to_string().len(), 64);
    }

    #[test]
    fn serde_representation_is_the_plain_string() {
        let id = DocumentId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
    }
}
