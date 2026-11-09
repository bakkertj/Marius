//! Vaults, documents and their metadata.
//!
//! Requirements: KB-001 .. KB-014, MD-008 (frontmatter preserved including
//! unknown fields), TAG-014 (typed metadata fields), ARCH-005 (Markdown files
//! stay authoritative).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{ContentHash, DocumentId, GlobalDocumentId, VaultId};
use crate::policy::Sensitivity;
use crate::provenance::Authorship;

/// A path relative to a vault root, stored with `/` separators on every
/// platform so that a vault directory copied between macOS and Windows keeps
/// working and the database stays comparable across devices (SYNC-005).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VaultPath(String);

impl VaultPath {
    /// Build a vault path from components, normalising separators and
    /// collapsing `.` and `..` so no path can escape the vault root.
    pub fn new(raw: impl AsRef<str>) -> Result<Self, PathError> {
        let raw = raw.as_ref().replace('\\', "/");
        let mut parts: Vec<&str> = Vec::new();
        for part in raw.split('/') {
            // Trim each component: leading/trailing whitespace in a filename is
            // almost always accidental and breaks path comparison between
            // devices, while spaces *inside* a name are perfectly ordinary.
            match part.trim() {
                "" | "." => {}
                ".." => {
                    // Refuse rather than silently clamp: a traversal attempt in
                    // an imported link or a plugin call is a bug or an attack,
                    // and quietly rewriting it would hide both (SEC-003).
                    if parts.pop().is_none() {
                        return Err(PathError::EscapesVault(raw.clone()));
                    }
                }
                other => parts.push(other),
            }
        }
        if parts.is_empty() {
            return Err(PathError::Empty);
        }
        Ok(Self(parts.join("/")))
    }

    /// Interpret a filesystem path found under `root` as a vault path.
    pub fn from_fs(root: &Path, absolute: &Path) -> Result<Self, PathError> {
        let relative = absolute
            .strip_prefix(root)
            .map_err(|_| PathError::OutsideRoot {
                root: root.to_path_buf(),
                path: absolute.to_path_buf(),
            })?;
        let text = relative
            .to_str()
            .ok_or_else(|| PathError::NotUtf8(relative.to_path_buf()))?;
        Self::new(text)
    }

    /// Resolve back to an absolute filesystem path under `root`.
    pub fn to_fs(&self, root: &Path) -> PathBuf {
        let mut out = root.to_path_buf();
        for part in self.components() {
            out.push(part);
        }
        out
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The final component, including any extension.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The final component without its extension — the fallback document title
    /// when a note has no heading and no frontmatter title (KB-007).
    pub fn file_stem(&self) -> &str {
        let name = self.file_name();
        match name.rsplit_once('.') {
            // A leading dot is part of the name, not an extension separator.
            Some((stem, _)) if !stem.is_empty() => stem,
            _ => name,
        }
    }

    /// Lower-cased extension, if any.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        name.rsplit_once('.').and_then(|(stem, ext)| {
            (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
        })
    }

    /// The containing folder, or `None` for a document at the vault root.
    pub fn parent(&self) -> Option<Self> {
        self.0.rsplit_once('/').map(|(dir, _)| Self(dir.to_owned()))
    }

    /// True when this path lies inside `folder` at any depth.
    pub fn is_within(&self, folder: &Self) -> bool {
        self.0.starts_with(&folder.0) && self.0.as_bytes().get(folder.0.len()) == Some(&b'/')
    }

    /// The same file under a different parent folder, for moves (KB-003).
    pub fn with_parent(&self, parent: Option<&Self>) -> Self {
        match parent {
            Some(dir) => Self(format!("{}/{}", dir.0, self.file_name())),
            None => Self(self.file_name().to_owned()),
        }
    }

    /// The same folder with a different final component, for renames (KB-003).
    pub fn with_file_name(&self, name: &str) -> Result<Self, PathError> {
        match self.parent() {
            Some(dir) => Self::new(format!("{}/{}", dir.0, name)),
            None => Self::new(name),
        }
    }
}

impl std::fmt::Display for VaultPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("vault path is empty")]
    Empty,
    #[error("vault path {0:?} escapes the vault root")]
    EscapesVault(String),
    #[error("path {path} is not inside vault root {root}")]
    OutsideRoot { root: PathBuf, path: PathBuf },
    #[error("path {0} is not valid UTF-8")]
    NotUtf8(PathBuf),
}

/// What kind of content a document holds.
///
/// Requirements: KB-007 (document type), IM-008/IM-009 (imported formats),
/// SRCH-002 (search by document type).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// An ordinary Markdown note — the authoritative, user-editable form.
    #[default]
    Markdown,
    /// Plain text captured or imported as-is (IM-002).
    PlainText,
    /// Captured email. Searchable knowledge, not a mailbox (ARCH-015, IM-006).
    Email,
    /// A binary or non-Markdown file tracked for its extracted text and
    /// attachments only; never rewritten (IM-008, KB-013).
    Attachment,
    /// A note the application generated (AI-011). Still a Markdown file on disk.
    GeneratedNote,
}

impl DocumentKind {
    /// Guess a kind from a file extension. Unknown extensions become
    /// attachments rather than Markdown, so the editor never offers to rewrite
    /// a file it cannot faithfully round-trip (ARCH-005).
    pub fn from_extension(ext: Option<&str>) -> Self {
        match ext {
            Some("md" | "markdown" | "mdown" | "mkd") => Self::Markdown,
            Some("txt" | "text") => Self::PlainText,
            Some("eml" | "mbox") => Self::Email,
            _ => Self::Attachment,
        }
    }

    /// True when the body is Markdown the editor may safely modify.
    pub const fn is_markdown(self) -> bool {
        matches!(self, Self::Markdown | Self::GeneratedNote)
    }

    /// True when the document carries indexable text at all.
    pub const fn has_text(self) -> bool {
        !matches!(self, Self::Attachment)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::PlainText => "plain_text",
            Self::Email => "email",
            Self::Attachment => "attachment",
            Self::GeneratedNote => "generated_note",
        }
    }

    pub const ALL: [DocumentKind; 5] = [
        Self::Markdown,
        Self::PlainText,
        Self::Email,
        Self::Attachment,
        Self::GeneratedNote,
    ];
}

impl std::fmt::Display for DocumentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DocumentKind {
    type Err = UnknownDocumentKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "markdown" => Ok(Self::Markdown),
            "plain_text" => Ok(Self::PlainText),
            "email" => Ok(Self::Email),
            "attachment" => Ok(Self::Attachment),
            "generated_note" => Ok(Self::GeneratedNote),
            other => Err(UnknownDocumentKind(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown document kind {0:?}")]
pub struct UnknownDocumentKind(pub String);

/// A typed user metadata value (TAG-014).
///
/// Frontmatter is schemaless, so values are kept in the shape the user wrote
/// them and only *interpreted* as a type when sorting or filtering (TAG-015).
/// Tagged on the wire rather than `untagged`, so that a date and a string that
/// merely looks like one stay distinguishable through a round trip into SQLite
/// and back — frontmatter is the user's data and must survive verbatim
/// (MD-008).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MetadataValue {
    Text(String),
    Number(f64),
    Boolean(bool),
    #[serde(with = "time::serde::rfc3339")]
    Date(OffsetDateTime),
    List(Vec<MetadataValue>),
    /// A nested mapping. Frontmatter is arbitrary YAML, and a nested block is
    /// data the user put there; flattening it to a string would make it
    /// unqueryable (MD-008, TAG-014).
    Map(BTreeMap<String, MetadataValue>),
    /// A key the user wrote with no value, e.g. `due:` on its own line.
    /// Preserved rather than dropped or coerced to an empty string.
    Null,
}

impl MetadataValue {
    /// Name of the value's type, for the properties panel and table views.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Number(_) => "number",
            Self::Boolean(_) => "boolean",
            Self::Date(_) => "date",
            Self::List(_) => "list",
            Self::Map(_) => "map",
            Self::Null => "null",
        }
    }

    /// Flatten to the text the full-text index should see.
    pub fn to_index_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Number(n) => n.to_string(),
            Self::Boolean(b) => b.to_string(),
            Self::Date(d) => d.date().to_string(),
            Self::List(items) => items
                .iter()
                .map(Self::to_index_text)
                .collect::<Vec<_>>()
                .join(" "),
            Self::Map(entries) => entries
                .iter()
                .map(|(k, v)| format!("{k} {}", v.to_index_text()))
                .collect::<Vec<_>>()
                .join(" "),
            Self::Null => String::new(),
        }
    }

    /// Flatten a scalar or list into the individual strings it contains.
    ///
    /// `tags: [a, b]` and `tags: a` are both legal frontmatter, so every reader
    /// of a list-ish field goes through this rather than matching on the shape.
    pub fn to_string_list(&self) -> Vec<String> {
        match self {
            Self::List(items) => items.iter().flat_map(Self::to_string_list).collect(),
            Self::Map(entries) => entries.values().flat_map(Self::to_string_list).collect(),
            Self::Null => Vec::new(),
            other => vec![other.to_index_text()],
        }
    }
}

/// A document's indexed record.
///
/// This is the database's view. The Markdown file on disk remains authoritative
/// for `title`, body text and frontmatter; everything here is either a cache of
/// the file or metadata that exists only in the application (integrity rule 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    pub vault: VaultId,
    pub path: VaultPath,
    /// Display title: frontmatter `title`, else the first H1, else the file stem.
    pub title: String,
    pub kind: DocumentKind,
    pub authorship: Authorship,
    pub content_hash: ContentHash,
    pub size_bytes: u64,
    /// Creation time as recorded by the application. Preserved across moves,
    /// unlike the filesystem's own birth time on some platforms (KB-007).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub modified_at: OffsetDateTime,
    /// When the index last read the file. Lets the scanner skip unchanged files
    /// cheaply and detect out-of-band edits (KB-005).
    #[serde(with = "time::serde::rfc3339")]
    pub indexed_at: OffsetDateTime,
    /// Set when the user archived the document. Archived documents stay in
    /// search results unless explicitly filtered out (KB-012).
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub archived_at: Option<OffsetDateTime>,
    /// Document-level restriction, tightening the vault policy (SEC-005).
    pub sensitivity: Option<Sensitivity>,
    /// Unknown frontmatter keys are kept verbatim so a round-trip through
    /// Marius never drops another tool's data (MD-008).
    pub frontmatter: BTreeMap<String, MetadataValue>,
    /// True when the file lives outside the vault root and is indexed in place
    /// (KB-013). Such files are never moved or rewritten.
    pub external: bool,
}

impl Document {
    /// This document's cross-vault reference (GRAPH-008).
    pub fn global_id(&self) -> GlobalDocumentId {
        GlobalDocumentId::new(self.vault, self.id)
    }

    /// True when the application must not rewrite the file's body.
    ///
    /// Combines the authorship rule (ARCH-012) with the practical rule that
    /// non-Markdown and externally referenced files are never rewritten.
    pub fn is_write_protected(&self) -> bool {
        self.authorship.is_source_protected() || self.external || !self.kind.is_markdown()
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// An alternate name a document can be linked by (KB-010, MD-006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentAlias {
    pub document: DocumentId,
    pub vault: VaultId,
    pub alias: String,
}

/// Which backend owns replication for a vault (ARCH-009, SYNC-007).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SyncBackend {
    /// Local only. No account, no network (SYNC-001).
    #[default]
    None,
    /// Apple CloudKit across the user's macOS and iOS devices (SYNC-003).
    CloudKit { container: String },
    /// The user's own Axum + PostgreSQL service (SYNC-004).
    SelfHosted { base_url: String },
}

impl SyncBackend {
    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "local-only",
            Self::CloudKit { .. } => "cloudkit",
            Self::SelfHosted { .. } => "self-hosted",
        }
    }
}

/// A vault: one directory of Markdown files plus the metadata Marius keeps
/// about it (KB-001, KB-008).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vault {
    pub id: VaultId,
    pub name: String,
    /// Absolute path to the vault root on this device. Device-local: the same
    /// vault has a different root on each machine, so this is never replicated.
    pub root: PathBuf,
    pub sync_backend: SyncBackend,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Set while the vault's files are not reachable — an unmounted drive, a
    /// detached device. Cross-vault edges into it must survive (SYNC-012).
    pub available: bool,
    /// When true, Marius writes its document IDs into frontmatter so that
    /// identity survives outside the database. Off by default, because turning
    /// it on modifies every source file (open question 3, ARCH-005).
    pub write_ids_to_frontmatter: bool,
}

impl Vault {
    /// A new local-only vault rooted at `root`.
    pub fn new_local(name: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            id: VaultId::new(),
            name: name.into(),
            root: root.into(),
            sync_backend: SyncBackend::None,
            created_at: OffsetDateTime::now_utc(),
            available: true,
            write_ids_to_frontmatter: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> VaultPath {
        VaultPath::new(s).unwrap()
    }

    #[test]
    fn paths_normalise_separators_and_redundant_segments() {
        assert_eq!(
            path("notes\\daily\\today.md").as_str(),
            "notes/daily/today.md"
        );
        assert_eq!(path("./notes//a.md").as_str(), "notes/a.md");
        assert_eq!(path("notes/sub/../a.md").as_str(), "notes/a.md");
    }

    #[test]
    fn paths_refuse_to_escape_the_vault() {
        // SEC-003: traversal in imported or plugin-supplied input is rejected
        // loudly rather than clamped.
        assert_eq!(
            VaultPath::new("../secrets.md"),
            Err(PathError::EscapesVault("../secrets.md".into()))
        );
        assert_eq!(VaultPath::new("   "), Err(PathError::Empty));
    }

    #[test]
    fn path_components_are_decomposed() {
        let p = path("research/lighthouses/alexandria.md");
        assert_eq!(p.file_name(), "alexandria.md");
        assert_eq!(p.file_stem(), "alexandria");
        assert_eq!(p.extension().as_deref(), Some("md"));
        assert_eq!(p.parent().unwrap().as_str(), "research/lighthouses");
        assert!(p.is_within(&path("research")));
        assert!(p.is_within(&path("research/lighthouses")));
        assert!(!p.is_within(&path("research/light")));
    }

    #[test]
    fn dotfiles_are_not_treated_as_extensions() {
        let p = path(".obsidian");
        assert_eq!(p.file_stem(), ".obsidian");
        assert_eq!(p.extension(), None);
    }

    #[test]
    fn paths_round_trip_through_the_filesystem() {
        let root = Path::new("/vaults/marius");
        let p = path("a/b/c.md");
        let fs = p.to_fs(root);
        assert_eq!(fs, PathBuf::from("/vaults/marius/a/b/c.md"));
        assert_eq!(VaultPath::from_fs(root, &fs).unwrap(), p);
    }

    #[test]
    fn paths_outside_the_root_are_rejected() {
        let err = VaultPath::from_fs(Path::new("/vaults/marius"), Path::new("/etc/passwd"));
        assert!(matches!(err, Err(PathError::OutsideRoot { .. })));
    }

    #[test]
    fn renames_and_moves_rewrite_only_the_intended_part() {
        let p = path("research/old.md");
        assert_eq!(
            p.with_file_name("new.md").unwrap().as_str(),
            "research/new.md"
        );
        assert_eq!(
            p.with_parent(Some(&path("archive"))).as_str(),
            "archive/old.md"
        );
        assert_eq!(p.with_parent(None).as_str(), "old.md");
    }

    #[test]
    fn unknown_extensions_are_attachments_not_markdown() {
        // ARCH-005: never offer to rewrite a file we cannot round-trip.
        assert_eq!(
            DocumentKind::from_extension(Some("md")),
            DocumentKind::Markdown
        );
        assert_eq!(
            DocumentKind::from_extension(Some("eml")),
            DocumentKind::Email
        );
        assert_eq!(
            DocumentKind::from_extension(Some("pdf")),
            DocumentKind::Attachment
        );
        assert_eq!(DocumentKind::from_extension(None), DocumentKind::Attachment);
        assert!(!DocumentKind::Attachment.is_markdown());
        assert!(!DocumentKind::Attachment.has_text());
    }

    #[test]
    fn source_documents_are_write_protected_and_derived_notes_are_not() {
        let mut doc = sample_document();
        assert!(doc.is_write_protected());

        doc.authorship = Authorship::Derived;
        doc.kind = DocumentKind::GeneratedNote;
        assert!(!doc.is_write_protected());

        // An externally referenced file is never rewritten, whatever it is (KB-013).
        doc.external = true;
        assert!(doc.is_write_protected());
    }

    #[test]
    fn metadata_values_flatten_for_indexing() {
        let v = MetadataValue::List(vec![
            MetadataValue::Text("lighthouse".into()),
            MetadataValue::Number(1869.0),
            MetadataValue::Boolean(true),
            MetadataValue::Null,
        ]);
        assert_eq!(v.to_index_text(), "lighthouse 1869 true ");
        assert_eq!(v.type_name(), "list");
    }

    #[test]
    fn list_fields_accept_both_a_scalar_and_a_list() {
        // `tags: a` and `tags: [a, b]` are both legal frontmatter.
        assert_eq!(
            MetadataValue::Text("a".into()).to_string_list(),
            vec!["a".to_string()]
        );
        assert_eq!(
            MetadataValue::List(vec![
                MetadataValue::Text("a".into()),
                MetadataValue::List(vec![MetadataValue::Text("b".into())]),
            ])
            .to_string_list(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(MetadataValue::Null.to_string_list().is_empty());
    }

    #[test]
    fn a_date_survives_a_round_trip_without_decaying_into_text() {
        // MD-008: unknown frontmatter must come back out exactly as it went in.
        let values = vec![
            MetadataValue::Date(OffsetDateTime::UNIX_EPOCH),
            MetadataValue::Text("1970-01-01T00:00:00Z".into()),
            MetadataValue::Number(3.5),
            MetadataValue::Boolean(false),
            MetadataValue::Null,
            MetadataValue::List(vec![MetadataValue::Text("x".into())]),
            MetadataValue::Map(BTreeMap::from([(
                "nested".to_string(),
                MetadataValue::Number(1.0),
            )])),
        ];
        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let back: MetadataValue = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back, "{json}");
            assert_eq!(value.type_name(), back.type_name());
        }
    }

    fn sample_document() -> Document {
        let now = OffsetDateTime::now_utc();
        Document {
            id: DocumentId::new(),
            vault: VaultId::new(),
            path: path("notes/alexandria.md"),
            title: "Alexandria".into(),
            kind: DocumentKind::Markdown,
            authorship: Authorship::Source,
            content_hash: ContentHash::of(b"body"),
            size_bytes: 4,
            created_at: now,
            modified_at: now,
            indexed_at: now,
            archived_at: None,
            sensitivity: None,
            frontmatter: BTreeMap::new(),
            external: false,
        }
    }
}
