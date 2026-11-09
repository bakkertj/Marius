//! The application service layer.
//!
//! Requirements: KB-001 .. KB-012, MD-004, MD-007, TAG-001 .. TAG-003,
//! SRCH-004, GRAPH-002, GRAPH-005, SEC-008, AC-001, AC-003, AC-004, AC-006,
//! AC-009, AC-011, AC-017.
//!
//! This is where a *user action* becomes a set of coordinated changes to files
//! and to the index. The distinction matters for ARCH-012: everything below
//! that writes into a source document does so because a person asked for it,
//! and says so in the revision it records. Nothing here is reachable from the
//! classifier or from an inference result.

use std::path::{Path, PathBuf};

use marius_indexing::indexer::{self, IndexOptions, IndexReport};
use marius_indexing::reconcile::Thoroughness;
use marius_indexing::{files, scan};
use marius_markdown::{rewrite, ChunkConfig, ParsedDocument};
use marius_model::{
    ContentHash, Document, DocumentId, DocumentKind, DocumentTag, Relationship, RelationshipKind,
    RevisionCause, Tag, TagName, Vault, VaultId, VaultPath, VaultPolicy,
};
use marius_search::Query;
use marius_storage::{
    documents, relationships, revisions, search_index, tags, Database, StorageError,
};

use time::OffsetDateTime;

use crate::config::{Config, ConfigError};
use crate::search::{self, SearchOptions, SearchResults};

/// Anything a service call can fail with.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    File(#[from] files::FileError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Index(#[from] indexer::IndexError),
    #[error(transparent)]
    Query(#[from] marius_search::QueryError),
    #[error("no vault named {0:?}")]
    NoSuchVault(String),
    #[error("no document {0}")]
    NoSuchDocument(DocumentId),
    #[error("{path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("{0} is a {1} and cannot be edited as Markdown")]
    NotEditable(VaultPath, DocumentKind),
    #[error("{0} is write-protected")]
    WriteProtected(VaultPath),
    #[error("invalid name {0:?}: {1}")]
    InvalidName(String, String),
    /// A name Marius will not author because it breaks on another platform.
    ///
    /// Indexing such a file is always allowed — a note the user already has is
    /// never hidden from them — but creating one is not (KB-004).
    #[error("{path} will not open on Windows: {reason}{suggestion}")]
    NotPortable {
        path: VaultPath,
        reason: String,
        suggestion: String,
    },
}

pub(crate) type Result<T> = std::result::Result<T, CoreError>;

/// Refuse to author a path that would not open on every platform.
///
/// Called on create, rename and move — never on indexing. The asymmetry is
/// deliberate: Marius must not *make* a file that a user's other machine
/// cannot open, but it must never refuse to show them a file they already
/// have (KB-004).
fn check_portable(path: &VaultPath) -> Result<()> {
    let issues = marius_model::windows_issues(path);
    let Some(first) = issues.first() else {
        return Ok(());
    };
    let suggestion = first
        .suggestion()
        .map(|s| format!("; try {s:?}"))
        .unwrap_or_default();
    Err(CoreError::NotPortable {
        path: path.clone(),
        reason: first.explain(),
        suggestion,
    })
}

/// An open Marius installation.
pub struct Marius {
    db: Database,
    config: Config,
    data_dir: PathBuf,
}

impl Marius {
    /// Open the installation in `data_dir`, creating it if necessary.
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.into();
        std::fs::create_dir_all(&data_dir).map_err(|source| ConfigError::Write {
            path: data_dir.clone(),
            source,
        })?;
        let config = Config::load(&data_dir)?;
        let db = Database::open(data_dir.join("marius.db"))?;
        Ok(Self {
            db,
            config,
            data_dir,
        })
    }

    /// Open the installation at the platform's default location.
    pub fn open_default() -> Result<Self> {
        Self::open(crate::config::default_data_dir())
    }

    /// Open an installation with no files at all, for tests.
    pub fn open_ephemeral() -> Result<Self> {
        Ok(Self {
            db: Database::open_in_memory()?,
            config: Config::default(),
            data_dir: PathBuf::new(),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Replace and persist the configuration.
    pub fn set_config(&mut self, config: Config) -> Result<()> {
        if !self.data_dir.as_os_str().is_empty() {
            config.save(&self.data_dir)?;
        }
        self.config = config;
        Ok(())
    }

    /// Direct database access, for callers doing their own queries.
    pub fn database(&self) -> &Database {
        &self.db
    }

    // ----------------------------------------------------------- vaults

    /// Every registered vault (KB-009).
    pub fn vaults(&self) -> Result<Vec<Vault>> {
        Ok(marius_storage::vaults::list(self.db.conn())?
            .into_iter()
            .map(|(vault, _)| vault)
            .collect())
    }

    /// Look a vault up by name, case-insensitively.
    pub fn vault(&self, name: &str) -> Result<Vault> {
        self.vaults()?
            .into_iter()
            .find(|v| v.name.eq_ignore_ascii_case(name) || v.id.to_string() == name)
            .ok_or_else(|| CoreError::NoSuchVault(name.to_owned()))
    }

    /// A vault's policy.
    pub fn vault_policy(&self, id: VaultId) -> Result<VaultPolicy> {
        Ok(marius_storage::vaults::get(self.db.conn(), id)?
            .map(|(_, policy)| policy)
            .unwrap_or_else(|| VaultPolicy::conservative(id)))
    }

    /// Register an existing directory as a vault (KB-001, KB-002, AC-001).
    ///
    /// Nothing in the directory is modified, moved or converted. Opening the
    /// same directory twice returns the vault that already exists, so a
    /// document's identity is never duplicated.
    pub fn open_vault(&mut self, name: &str, root: impl AsRef<Path>) -> Result<Vault> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(CoreError::NotADirectory {
                path: root.to_path_buf(),
            });
        }
        // Canonicalise so that `./notes` and an absolute path are one vault.
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        if let Some((existing, _)) = marius_storage::vaults::by_root(self.db.conn(), &root)? {
            return Ok(existing);
        }
        let vault = Vault::new_local(name, &root);
        let policy = VaultPolicy::conservative(vault.id);
        marius_storage::vaults::upsert(self.db.conn(), &vault, &policy)?;
        Ok(vault)
    }

    /// Create a new, empty vault directory and register it (KB-001).
    pub fn create_vault(&mut self, name: &str, root: impl AsRef<Path>) -> Result<Vault> {
        let root = root.as_ref();
        std::fs::create_dir_all(root).map_err(|source| ConfigError::Write {
            path: root.to_path_buf(),
            source,
        })?;
        self.open_vault(name, root)
    }

    /// Rename a vault. Its directory is untouched.
    pub fn rename_vault(&mut self, id: VaultId, name: &str) -> Result<Vault> {
        let (mut vault, policy) = marius_storage::vaults::get(self.db.conn(), id)?
            .ok_or_else(|| CoreError::NoSuchVault(id.to_string()))?;
        vault.name = name.to_owned();
        marius_storage::vaults::upsert(self.db.conn(), &vault, &policy)?;
        Ok(vault)
    }

    /// Set a vault's inference policy (SEC-005, AI-016).
    pub fn set_vault_policy(&mut self, policy: VaultPolicy) -> Result<()> {
        let (vault, _) = marius_storage::vaults::get(self.db.conn(), policy.vault)?
            .ok_or_else(|| CoreError::NoSuchVault(policy.vault.to_string()))?;
        marius_storage::vaults::upsert(self.db.conn(), &vault, &policy)?;
        Ok(())
    }

    /// Mark a vault reachable or not, keeping all of its metadata (SYNC-012).
    pub fn set_vault_available(&mut self, id: VaultId, available: bool) -> Result<()> {
        Ok(marius_storage::vaults::set_available(
            self.db.conn(),
            id,
            available,
        )?)
    }

    /// Remove a vault's registration.
    ///
    /// KB-001 is explicit: **the underlying files are not deleted**. Edges from
    /// other vaults into this one keep their metadata (SYNC-012, AC-009).
    pub fn close_vault(&mut self, id: VaultId) -> Result<()> {
        Ok(marius_storage::vaults::remove(self.db.conn(), id)?)
    }

    // ---------------------------------------------------------- indexing

    /// Bring a vault's index up to date.
    pub fn index(&mut self, vault: &Vault) -> Result<IndexReport> {
        let options = self.index_options(vault)?;
        let report = indexer::index_vault(&mut self.db, vault, &options)?;
        // An index pass is where the journal grows fastest, so it is also
        // where the retention window is worth applying.
        self.prune_journal()?;
        Ok(report)
    }

    /// Discard journal history that is acknowledged and older than the
    /// configured retention window.
    ///
    /// Never touches pending, failed or conflicted operations: those are
    /// either still queued or still awaiting a decision (SYNC-009, SYNC-011).
    pub fn prune_journal(&mut self) -> Result<usize> {
        let days = self.config.journal_retention_days;
        if days == 0 {
            return Ok(0);
        }
        let before = OffsetDateTime::now_utc() - time::Duration::days(i64::from(days));
        Ok(marius_storage::changelog::prune_acknowledged(
            self.db.conn(),
            before,
        )?)
    }

    /// Re-read every file regardless of its metadata (the answer to anything
    /// the quick reconcile could have missed).
    pub fn reindex(&mut self, vault: &Vault) -> Result<IndexReport> {
        let mut options = self.index_options(vault)?;
        options.thoroughness = Thoroughness::Full;
        Ok(indexer::index_vault(&mut self.db, vault, &options)?)
    }

    /// Discard and rebuild every derived index (NFR-007, SEC-008).
    ///
    /// Authoritative metadata — identities, manual and AI tags, provenance,
    /// rejections, the journal — is untouched; only the search index, passages
    /// and vectors are recomputed from the files (integrity rule 3).
    pub fn rebuild_indexes(&mut self) -> Result<Vec<IndexReport>> {
        self.db.rebuild_derived()?;
        let mut reports = Vec::new();
        for vault in self.vaults()? {
            if !vault.available {
                continue;
            }
            // Every document must pass through the pipeline again: change
            // detection would correctly find nothing to do, and the emptied
            // search index would stay empty.
            let mut options = self.index_options(&vault)?;
            options.thoroughness = Thoroughness::Rebuild;
            reports.push(indexer::index_vault(&mut self.db, &vault, &options)?);
        }
        Ok(reports)
    }

    fn index_options(&self, vault: &Vault) -> Result<IndexOptions> {
        let link_vaults = if self.config.cross_vault_links {
            self.vaults()?
                .into_iter()
                .filter(|v| v.id != vault.id && v.available)
                .map(|v| v.id)
                .collect()
        } else {
            Vec::new()
        };
        Ok(IndexOptions {
            scan: scan::ScanOptions::default(),
            chunking: self.config.chunk_on_index.then(ChunkConfig::default),
            record_revisions: self.config.record_revisions,
            revision_body_limit: self.config.revision_body_limit_kib * 1024,
            link_vaults,
            ..IndexOptions::default()
        })
    }

    // --------------------------------------------------------- documents

    /// Fetch a document record.
    pub fn document(&self, id: DocumentId) -> Result<Document> {
        documents::get(self.db.conn(), id)?.ok_or(CoreError::NoSuchDocument(id))
    }

    /// Fetch a document by vault and path.
    pub fn document_at(&self, vault: VaultId, path: &VaultPath) -> Result<Option<Document>> {
        Ok(documents::by_path(self.db.conn(), vault, path)?)
    }

    /// Read a document's file (ARCH-005: the file is authoritative).
    pub fn read(&self, document: &Document) -> Result<String> {
        let absolute = document.path.to_fs(&self.vault_root(document.vault)?);
        Ok(files::read_text(&absolute)?.text)
    }

    /// Create a new document and index it (KB-003).
    pub fn create_document(
        &mut self,
        vault: &Vault,
        path: &VaultPath,
        body: &str,
    ) -> Result<Document> {
        check_portable(path)?;
        let absolute = path.to_fs(&vault.root);
        files::create_new(&absolute, body)?;
        let options = self.index_options(vault)?;
        indexer::index_path(&mut self.db, vault, path, &options)?;
        self.document_at(vault.id, path)?
            .ok_or_else(|| CoreError::NoSuchVault(vault.name.clone()))
    }

    /// Save a user's edit to a document.
    ///
    /// This is the only path by which Marius writes a body a person typed, and
    /// it records a [`RevisionCause::Edit`] so the trail distinguishes it from
    /// an external change or a generation (KB-011, MD-004).
    pub fn save_document(&mut self, id: DocumentId, text: &str) -> Result<ContentHash> {
        let document = self.document(id)?;
        if document.external {
            // KB-013: a file indexed in place outside the vault is never
            // rewritten.
            return Err(CoreError::WriteProtected(document.path.clone()));
        }
        if !document.kind.is_markdown() && document.kind != DocumentKind::PlainText {
            return Err(CoreError::NotEditable(document.path.clone(), document.kind));
        }
        let vault = self.vault_by_id(document.vault)?;
        let absolute = document.path.to_fs(&vault.root);
        let hash = files::write_atomic(&absolute, text)?;

        let tx = self.db.transaction()?;
        revisions::record(
            &tx,
            id,
            revisions::RevisionDraft::new(
                RevisionCause::Edit,
                hash,
                text.len() as u64,
                document.path.as_str(),
            )
            .with_body(text),
        )?;
        marius_storage::commit(tx)?;

        let options = self.index_options(&vault)?;
        indexer::index_path(&mut self.db, &vault, &document.path, &options)?;
        Ok(hash)
    }

    /// Move a document to the vault trash, from which it can be restored
    /// (KB-003).
    pub fn delete_document(&mut self, id: DocumentId) -> Result<PathBuf> {
        let document = self.document(id)?;
        let vault = self.vault_by_id(document.vault)?;
        let trashed = files::trash(&vault.root, document.path.as_str())?;

        let tx = self.db.transaction()?;
        if let Some(seq) = documents::seq_of(&tx, id)? {
            search_index::remove(&tx, seq)?;
        }
        documents::delete(&tx, id)?;
        marius_storage::commit(tx)?;
        Ok(trashed)
    }

    /// Put a trashed file back where it came from (KB-003).
    pub fn restore_document(&mut self, vault: &Vault, trashed: &Path) -> Result<Document> {
        let origin = files::trashed_origin(trashed).ok_or_else(|| {
            CoreError::InvalidName(
                trashed.display().to_string(),
                "not a trashed Marius document".into(),
            )
        })?;
        let path = VaultPath::new(&origin)
            .map_err(|e| CoreError::InvalidName(origin.clone(), e.to_string()))?;
        files::move_file(trashed, &path.to_fs(&vault.root))?;
        let options = self.index_options(vault)?;
        indexer::index_path(&mut self.db, vault, &path, &options)?;
        self.document_at(vault.id, &path)?
            .ok_or_else(|| CoreError::NoSuchVault(vault.name.clone()))
    }

    /// Archive or unarchive a document (KB-012).
    pub fn set_archived(&mut self, id: DocumentId, archived: bool) -> Result<()> {
        Ok(documents::set_archived(self.db.conn(), id, archived)?)
    }

    // ------------------------------------------------- rename and move

    /// Rename a document, rewriting the links that point at it (MD-007, AC-003).
    ///
    /// `new_name` is a filename, not a path. A `/` in it would silently create
    /// a folder and move the document, which is `move_document`'s job and not
    /// what someone renaming a note means.
    pub fn rename_document(&mut self, id: DocumentId, new_name: &str) -> Result<RenameReport> {
        if new_name.contains(['/', '\\']) {
            return Err(CoreError::InvalidName(
                new_name.to_owned(),
                "a name cannot contain a path separator; use `move` to relocate a document".into(),
            ));
        }
        let document = self.document(id)?;
        let new_path = document
            .path
            .with_file_name(&with_extension(new_name, &document.path))
            .map_err(|e| CoreError::InvalidName(new_name.to_owned(), e.to_string()))?;
        self.relocate(document, new_path)
    }

    /// Move a document to another folder, rewriting inbound links.
    pub fn move_document(
        &mut self,
        id: DocumentId,
        folder: Option<&VaultPath>,
    ) -> Result<RenameReport> {
        let document = self.document(id)?;
        let new_path = document.path.with_parent(folder);
        self.relocate(document, new_path)
    }

    /// Move a whole folder: every document beneath it relocates with its
    /// structure preserved, and every link that any move invalidates is
    /// rewritten (KB-003, MD-007).
    ///
    /// Three directions of links are affected, and all three are planned
    /// *before anything moves*, while every path still resolves:
    ///
    /// * **into** the folder, from documents staying put;
    /// * **out of** the folder, where a moving document used a
    ///   folder-relative destination that will mean something else from its
    ///   new home;
    /// * **within** the folder, where one moving document names another by a
    ///   vault-absolute path.
    ///
    /// Bare `[[wiki names]]` need no rewriting in any direction — the name
    /// does not change — which is most links in practice.
    ///
    /// Validation is all-or-nothing: a destination collision, an unportable
    /// resulting path, or a move into the folder's own subtree refuses the
    /// whole operation before a single file moves.
    pub fn move_folder(
        &mut self,
        vault: &Vault,
        folder: &VaultPath,
        new_parent: Option<&VaultPath>,
    ) -> Result<FolderMoveReport> {
        // A folder cannot move into itself or its own descendant.
        if let Some(parent) = new_parent {
            if parent == folder || parent.is_within(folder) {
                return Err(CoreError::InvalidName(
                    parent.to_string(),
                    format!("cannot move {folder} into its own subtree"),
                ));
            }
        }
        let destination = match new_parent {
            Some(parent) => VaultPath::new(format!("{parent}/{}", folder.file_name()))
                .map_err(|e| CoreError::InvalidName(folder.to_string(), e.to_string()))?,
            None => VaultPath::new(folder.file_name())
                .map_err(|e| CoreError::InvalidName(folder.to_string(), e.to_string()))?,
        };
        let mut report = FolderMoveReport {
            from: folder.clone(),
            to: destination.clone(),
            documents_moved: 0,
            links_rewritten: 0,
            files_rewritten: Vec::new(),
            unwritable: Vec::new(),
        };
        // Everything beneath the folder, attachments included — images move
        // with their notes. Checked before the no-op case: "move a folder
        // that does not exist" is an error, not a success.
        let contents = documents::list(
            self.db.conn(),
            &documents::Filter {
                vaults: vec![vault.id],
                folder: Some(folder.clone()),
                ..Default::default()
            },
        )?;
        if contents.is_empty() {
            return Err(CoreError::InvalidName(
                folder.to_string(),
                "no documents under that folder".into(),
            ));
        }
        if destination == *folder {
            return Ok(report);
        }

        // Plan the destination of every document, refusing on any conflict
        // before anything moves.
        let prefix = format!("{folder}/");
        let mut moves: std::collections::HashMap<DocumentId, (Document, VaultPath)> =
            std::collections::HashMap::new();
        for document in contents {
            let suffix = document
                .path
                .as_str()
                .strip_prefix(&prefix)
                .unwrap_or(document.path.file_name());
            let new_path = VaultPath::new(format!("{destination}/{suffix}"))
                .map_err(|e| CoreError::InvalidName(suffix.to_owned(), e.to_string()))?;
            check_portable(&new_path)?;
            if documents::by_path(self.db.conn(), vault.id, &new_path)?.is_some()
                || new_path.to_fs(&vault.root).exists()
            {
                return Err(CoreError::InvalidName(
                    new_path.to_string(),
                    "destination already exists".into(),
                ));
            }
            moves.insert(document.id, (document, new_path));
        }

        // Who needs rewriting: outside referrers of any moved document, plus
        // every moved document itself (for its own relative links out).
        let mut referrer_ids: std::collections::BTreeSet<DocumentId> =
            moves.keys().copied().collect();
        for id in moves.keys() {
            for edge in relationships::incoming(self.db.conn(), *id)? {
                if matches!(edge.kind, RelationshipKind::Link | RelationshipKind::Embed) {
                    referrer_ids.insert(edge.endpoints.source.document);
                }
            }
        }

        // Plan every rewrite now, while the old locations still resolve.
        let mut plans: Vec<RewritePlan> = Vec::new();
        for referrer_id in referrer_ids {
            let Some(referrer) = documents::get(self.db.conn(), referrer_id)? else {
                continue;
            };
            if referrer.external || !referrer.kind.is_markdown() {
                if moves.contains_key(&referrer.id) {
                    continue; // an attachment moves; it has no links to fix
                }
                report.unwritable.push(referrer.path.clone());
                continue;
            }
            match self.plan_folder_rewrite(vault, &referrer, &moves) {
                Ok(Some(plan)) => plans.push(plan),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(path = %referrer.path, %error, "cannot plan rewrite");
                    report.unwritable.push(referrer.path.clone());
                }
            }
        }

        // Move the files, then the records.
        for (document, new_path) in moves.values() {
            files::move_file(
                &document.path.to_fs(&vault.root),
                &new_path.to_fs(&vault.root),
            )?;
        }
        let tx = self.db.transaction()?;
        for (document, new_path) in moves.values() {
            documents::relocate(&tx, document.id, new_path)?;
            let note = format!("folder moved from {folder}");
            revisions::record(
                &tx,
                document.id,
                revisions::RevisionDraft::new(
                    RevisionCause::Relocation,
                    document.content_hash,
                    document.size_bytes,
                    new_path.as_str(),
                )
                .with_note(&note),
            )?;
        }
        marius_storage::commit(tx)?;
        report.documents_moved = moves.len();

        // Sweep the vacated directory chains.
        for (document, _) in moves.values() {
            if let Some(parent) = document.path.parent() {
                files::prune_empty_dirs(&vault.root, parent.as_str());
            }
        }

        // Apply the rewrites: each lands at the referrer's current location,
        // which for a moved referrer is its new one.
        for plan in plans {
            match self.apply_rewrite(vault, &plan, &destination) {
                Ok(count) => {
                    report.links_rewritten += count;
                    report.files_rewritten.push(plan.write_path.clone());
                }
                Err(error) => {
                    tracing::warn!(path = %plan.write_path, %error, "cannot rewrite links");
                    report.unwritable.push(plan.write_path.clone());
                }
            }
        }

        // Refresh every moved document's index entry at its new path.
        let options = self.index_options(vault)?;
        for (_, new_path) in moves.values() {
            indexer::index_path(&mut self.db, vault, new_path, &options)?;
        }
        Ok(report)
    }

    /// Plan the edits one referrer needs when `moves` are applied.
    fn plan_folder_rewrite(
        &self,
        vault: &Vault,
        referrer: &Document,
        moves: &std::collections::HashMap<DocumentId, (Document, VaultPath)>,
    ) -> Result<Option<RewritePlan>> {
        let text = files::read_text(&referrer.path.to_fs(&vault.root))?.text;
        let parsed = ParsedDocument::parse(&text);
        let referrer_new_path = moves
            .get(&referrer.id)
            .map(|(_, new)| new.clone())
            .unwrap_or_else(|| referrer.path.clone());
        let referrer_moving = moves.contains_key(&referrer.id);
        let mut edits: Vec<rewrite::Edit> = Vec::new();

        // Wiki links are vault-wide: a bare name survives any move, and a
        // path-style target needs updating exactly when its document moved.
        for link in &parsed.scan.wiki_links {
            if link.is_internal() || !link.target.contains('/') {
                continue;
            }
            let Some((found, _)) = documents::resolve_relative(
                self.db.conn(),
                vault.id,
                Some(&referrer.path),
                &link.target,
                &[],
            )?
            else {
                continue;
            };
            let Some((_, target_new)) = moves.get(&found.document) else {
                continue;
            };
            let replacement = strip_markdown_extension(target_new.as_str()).to_owned();
            if replacement == link.target {
                continue;
            }
            if !rewrite::is_writable_wiki_target(&replacement) {
                return Ok(None);
            }
            edits.push(rewrite::Edit::new(link.target_span.clone(), replacement));
        }

        // Markdown destinations: affected when the target moved, or when the
        // referrer moved and the destination only resolved relative to it.
        for link in &parsed.scan.markdown_links {
            if link.is_external() || link.target.trim().is_empty() {
                continue;
            }
            let Some((found, _)) = documents::resolve_relative(
                self.db.conn(),
                vault.id,
                Some(&referrer.path),
                &link.target,
                &[],
            )?
            else {
                continue;
            };
            let target_moved = moves.get(&found.document);
            if target_moved.is_none() {
                if !referrer_moving {
                    continue;
                }
                // The referrer moves and the target stays: only a destination
                // that depended on the referrer's folder needs fixing. One
                // that also resolves vault-wide is location-independent.
                let absolute =
                    documents::resolve_link_global(self.db.conn(), vault.id, &link.target, &[])?;
                let path_stable = absolute.is_some_and(|(hit, how)| {
                    hit.document == found.document && matches!(how, documents::LinkMatch::Path)
                });
                if path_stable {
                    continue;
                }
            }
            let target_path = target_moved
                .map(|(_, new)| new.clone())
                .unwrap_or_else(|| found_path(self.db.conn(), found.document))
                .clone();
            let replacement = markdown_replacement(&text, link, &referrer_new_path, &target_path);
            if replacement == link.target {
                continue;
            }
            edits.push(rewrite::Edit::new(link.target_span.clone(), replacement));
        }

        if edits.is_empty() {
            return Ok(None);
        }
        Ok(Some(RewritePlan {
            document: referrer.id,
            path: referrer.path.clone(),
            write_path: referrer_new_path,
            text,
            edits,
        }))
    }

    /// The shared implementation of rename and move.
    ///
    /// Order is the whole correctness argument here. The edits to other
    /// people's files are computed *first*, while the document is still at its
    /// old location and its links still resolve — resolving them after the move
    /// silently finds nothing and rewrites nothing. Then the file moves, so the
    /// user's data is in its new place even if a later rewrite fails. Then the
    /// index is updated, and finally each referring file is rewritten with its
    /// own revision recorded. A file that cannot be rewritten safely is
    /// reported rather than mangled.
    fn relocate(&mut self, document: Document, new_path: VaultPath) -> Result<RenameReport> {
        let mut report = RenameReport {
            document: document.id,
            from: document.path.clone(),
            to: new_path.clone(),
            links_rewritten: 0,
            files_rewritten: Vec::new(),
            unwritable: Vec::new(),
        };
        if new_path == document.path {
            return Ok(report);
        }
        check_portable(&new_path)?;
        let vault = self.vault_by_id(document.vault)?;

        // 1. Plan the link rewrites while the old location still resolves.
        let referrers: Vec<DocumentId> = relationships::incoming(self.db.conn(), document.id)?
            .into_iter()
            .filter(|edge| matches!(edge.kind, RelationshipKind::Link | RelationshipKind::Embed))
            .map(|edge| edge.endpoints.source.document)
            .collect();

        let mut plans: Vec<RewritePlan> = Vec::new();
        for referrer_id in referrers {
            if referrer_id == document.id {
                continue;
            }
            let Some(referrer) = documents::get(self.db.conn(), referrer_id)? else {
                continue;
            };
            if referrer.external || !referrer.kind.is_markdown() {
                report.unwritable.push(referrer.path.clone());
                continue;
            }
            match self.plan_rewrite(&vault, &referrer, document.id, &new_path) {
                Ok(Some(plan)) => plans.push(plan),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(path = %referrer.path, %error, "cannot plan link rewrite");
                    report.unwritable.push(referrer.path.clone());
                }
            }
        }

        // 2. Move the file.
        let old_absolute = document.path.to_fs(&vault.root);
        let new_absolute = new_path.to_fs(&vault.root);
        files::move_file(&old_absolute, &new_absolute)?;

        // 3. Update the index, keeping the identity (AC-003).
        let tx = self.db.transaction()?;
        documents::relocate(&tx, document.id, &new_path)?;
        let note = format!("renamed from {}", document.path);
        revisions::record(
            &tx,
            document.id,
            revisions::RevisionDraft::new(
                RevisionCause::Relocation,
                document.content_hash,
                document.size_bytes,
                new_path.as_str(),
            )
            .with_note(&note),
        )?;
        marius_storage::commit(tx)?;

        // 4. Apply the planned rewrites.
        for plan in plans {
            match self.apply_rewrite(&vault, &plan, &new_path) {
                Ok(count) => {
                    report.links_rewritten += count;
                    report.files_rewritten.push(plan.path.clone());
                }
                Err(error) => {
                    tracing::warn!(path = %plan.path, %error, "cannot rewrite links");
                    report.unwritable.push(plan.path.clone());
                }
            }
        }

        let options = self.index_options(&vault)?;
        indexer::index_path(&mut self.db, &vault, &new_path, &options)?;
        Ok(report)
    }

    /// Work out which byte ranges in `referrer` name `target`, and what each
    /// should become.
    fn plan_rewrite(
        &self,
        vault: &Vault,
        referrer: &Document,
        target: DocumentId,
        new_path: &VaultPath,
    ) -> Result<Option<RewritePlan>> {
        let absolute = referrer.path.to_fs(&vault.root);
        let text = files::read_text(&absolute)?.text;
        let parsed = ParsedDocument::parse(&text);
        let mut edits: Vec<rewrite::Edit> = Vec::new();

        for link in &parsed.scan.wiki_links {
            if link.is_internal() || !self.points_at(vault, referrer, &link.target, target)? {
                continue;
            }
            // Keep the author's spelling: a bare name stays a bare name and a
            // path stays a path. Rewriting `[[note]]` as `[[folder/note]]`
            // would be a gratuitous change to someone's prose.
            let replacement = if link.target.contains('/') {
                strip_markdown_extension(new_path.as_str()).to_owned()
            } else {
                new_path.file_stem().to_owned()
            };
            if !rewrite::is_writable_wiki_target(&replacement) {
                // No escape syntax other tools would read, so the link is left
                // alone and reported.
                return Ok(None);
            }
            edits.push(rewrite::Edit::new(link.target_span.clone(), replacement));
        }

        for link in &parsed.scan.markdown_links {
            if link.is_external()
                || link.target.trim().is_empty()
                || !self.points_at(vault, referrer, &link.target, target)?
            {
                continue;
            }
            let replacement = markdown_replacement(&text, link, &referrer.path, new_path);
            edits.push(rewrite::Edit::new(link.target_span.clone(), replacement));
        }

        if edits.is_empty() {
            return Ok(None);
        }
        Ok(Some(RewritePlan {
            document: referrer.id,
            path: referrer.path.clone(),
            write_path: referrer.path.clone(),
            text,
            edits,
        }))
    }

    /// Write a planned rewrite to disk and record it.
    fn apply_rewrite(
        &mut self,
        vault: &Vault,
        plan: &RewritePlan,
        new_path: &VaultPath,
    ) -> Result<usize> {
        let updated = rewrite::apply(&plan.text, &plan.edits).map_err(|error| {
            // Overlapping spans would corrupt the file; refuse rather than try.
            CoreError::InvalidName(plan.path.to_string(), error.to_string())
        })?;
        let absolute = plan.write_path.to_fs(&vault.root);
        let hash = files::write_atomic(&absolute, &updated)?;

        let tx = self.db.transaction()?;
        let note = format!("links updated for rename to {new_path}");
        revisions::record(
            &tx,
            plan.document,
            revisions::RevisionDraft::new(
                RevisionCause::Edit,
                hash,
                updated.len() as u64,
                plan.write_path.as_str(),
            )
            .with_body(&updated)
            .with_note(&note),
        )?;
        marius_storage::commit(tx)?;

        let options = self.index_options(vault)?;
        indexer::index_path(&mut self.db, vault, &plan.write_path, &options)?;
        Ok(plan.edits.len())
    }

    /// Whether `written` inside `referrer` resolves to `target`.
    fn points_at(
        &self,
        vault: &Vault,
        referrer: &Document,
        written: &str,
        target: DocumentId,
    ) -> Result<bool> {
        let resolved = documents::resolve_relative(
            self.db.conn(),
            vault.id,
            Some(&referrer.path),
            written,
            &[],
        )?;
        Ok(resolved.is_some_and(|(found, _)| found.document == target))
    }

    // -------------------------------------------------------------- tags

    /// Apply a tag by hand (TAG-001).
    pub fn add_tag(&mut self, id: DocumentId, name: &str) -> Result<Tag> {
        let document = self.document(id)?;
        let parsed = TagName::new(name)
            .map_err(|e| CoreError::InvalidName(name.to_owned(), e.to_string()))?;
        let tag = tags::ensure(self.db.conn(), Some(document.vault), &parsed)?;
        tags::assign(self.db.conn(), &DocumentTag::manual(id, tag.id))?;
        self.refresh_tag_index(&document)?;
        Ok(tag)
    }

    /// Remove a tag without recording a rejection (TAG-001).
    pub fn remove_tag(&mut self, id: DocumentId, name: &str) -> Result<bool> {
        let document = self.document(id)?;
        let parsed = TagName::new(name)
            .map_err(|e| CoreError::InvalidName(name.to_owned(), e.to_string()))?;
        let Some(tag) = tags::by_name(self.db.conn(), Some(document.vault), &parsed)? else {
            return Ok(false);
        };
        let removed = tags::unassign(self.db.conn(), id, tag.id)?;
        self.refresh_tag_index(&document)?;
        Ok(removed)
    }

    /// Reject a tag, so automatic classification will not propose it again
    /// (TAG-012, AC-006).
    pub fn reject_tag(&mut self, id: DocumentId, name: &str, reason: Option<&str>) -> Result<()> {
        let document = self.document(id)?;
        let parsed = TagName::new(name)
            .map_err(|e| CoreError::InvalidName(name.to_owned(), e.to_string()))?;
        let tag = tags::ensure(self.db.conn(), Some(document.vault), &parsed)?;
        tags::reject(self.db.conn(), id, tag.id, reason)?;
        self.refresh_tag_index(&document)?;
        Ok(())
    }

    /// Accept an AI-proposed tag (TAG-007).
    pub fn approve_tag(&mut self, id: DocumentId, name: &str) -> Result<bool> {
        let document = self.document(id)?;
        let parsed = TagName::new(name)
            .map_err(|e| CoreError::InvalidName(name.to_owned(), e.to_string()))?;
        let Some(tag) = tags::by_name(self.db.conn(), Some(document.vault), &parsed)? else {
            return Ok(false);
        };
        Ok(tags::approve(self.db.conn(), id, tag.id)?)
    }

    /// A document's tags with their assignment metadata (TAG-002, TAG-007).
    pub fn document_tags(&self, id: DocumentId) -> Result<Vec<(Tag, DocumentTag)>> {
        Ok(tags::of_document(self.db.conn(), id)?)
    }

    /// Tag usage counts for the tag browser (TAG-002).
    ///
    /// `None` means every vault, which is what a tag browser with no vault
    /// selected should show.
    pub fn tag_counts(&self, vault: Option<VaultId>) -> Result<Vec<(Tag, u64)>> {
        Ok(match vault {
            Some(vault) => tags::counts(self.db.conn(), Some(vault))?,
            None => tags::counts_everywhere(self.db.conn())?,
        })
    }

    /// Keep the search index's tag column in step with an edited assignment,
    /// so a tag change is reflected in search immediately (AC-004).
    fn refresh_tag_index(&self, document: &Document) -> Result<()> {
        let Some(seq) = documents::seq_of(self.db.conn(), document.id)? else {
            return Ok(());
        };
        let tag_text = tags::of_document(self.db.conn(), document.id)?
            .iter()
            .map(|(tag, _)| tag.canonical_name.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let alias_text = documents::aliases(self.db.conn(), document.id)?.join(" ");
        // The body text is reread from the existing index entry's document, so
        // only the tag column effectively changes.
        let body = self.read(document).unwrap_or_default();
        let parsed = ParsedDocument::parse(&body);
        search_index::index(
            self.db.conn(),
            seq,
            &search_index::IndexedText {
                title: document.title.clone(),
                body: parsed.plain_text,
                tags: tag_text,
                path: document.path.to_string(),
                aliases: alias_text,
            },
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ search

    /// Run a query string across every accessible vault (SRCH-004).
    pub fn search(&self, query: &str, options: SearchOptions) -> Result<SearchResults> {
        let parsed: Query = marius_search::parse(query)?;
        let vaults = self.vaults()?;
        Ok(search::execute(self.db.conn(), &parsed, &vaults, options)?)
    }

    // ------------------------------------------------------------- graph

    /// A document's immediate neighbourhood (GRAPH-002, GRAPH-005).
    pub fn neighbourhood(&self, id: DocumentId) -> Result<Neighbourhood> {
        let document = self.document(id)?;
        let outgoing = relationships::outgoing(self.db.conn(), id)?;
        let incoming = relationships::incoming(self.db.conn(), id)?;
        let shared_tags = relationships::shared_tag_neighbours(self.db.conn(), id, &[], 25)?;
        let unresolved = relationships::pending_links(self.db.conn(), id)?;
        Ok(Neighbourhood {
            document,
            outgoing,
            incoming,
            shared_tags,
            unresolved,
        })
    }

    /// The knowledge graph for one vault (GRAPH-001): nodes, styled edges,
    /// and the local/global split, assembled from the stored relationships
    /// and computed co-tag kinship. Drawing is the UI's job.
    pub fn graph(
        &self,
        vault: VaultId,
        options: &crate::graph::GraphOptions,
    ) -> Result<crate::graph::GraphData> {
        crate::graph::assemble(self.db.conn(), vault, options)
    }

    // ----------------------------------------------------- housekeeping

    /// Documents whose filenames will not open on Windows.
    ///
    /// Reported rather than refused, so a vault authored on a Mac can be made
    /// portable before it reaches another machine (KB-004, SYNC-005).
    pub fn portability_report(&self) -> Result<Vec<(Document, Vec<marius_model::Portability>)>> {
        let mut out = Vec::new();
        for vault in self.vaults()? {
            let filter = documents::Filter {
                vaults: vec![vault.id],
                ..Default::default()
            };
            for document in documents::list(self.db.conn(), &filter)? {
                let issues = marius_model::windows_issues(&document.path);
                if !issues.is_empty() {
                    out.push((document, issues));
                }
            }
        }
        Ok(out)
    }

    /// Copy the database to `path` while it stays open (SEC-008).
    pub fn backup(&self, path: impl AsRef<Path>) -> Result<()> {
        Ok(self.db.backup_to(path)?)
    }

    /// Run SQLite's integrity check (SEC-008).
    pub fn integrity_check(&self) -> Result<Vec<String>> {
        Ok(self.db.integrity_check()?)
    }

    /// Compact the search index, prune journal history and refresh statistics.
    pub fn maintain(&mut self) -> Result<MaintenanceReport> {
        let journal_before = marius_storage::changelog::count(self.db.conn())?;
        let pruned = self.prune_journal()?;
        let sizes = self.db.maintain()?;
        Ok(MaintenanceReport {
            journal_before,
            journal_pruned: pruned,
            journal_after: marius_storage::changelog::count(self.db.conn())?,
            bytes_before: sizes.before_bytes,
            bytes_after: sizes.after_bytes,
        })
    }

    fn vault_by_id(&self, id: VaultId) -> Result<Vault> {
        marius_storage::vaults::get(self.db.conn(), id)?
            .map(|(vault, _)| vault)
            .ok_or_else(|| CoreError::NoSuchVault(id.to_string()))
    }

    fn vault_root(&self, id: VaultId) -> Result<PathBuf> {
        Ok(self.vault_by_id(id)?.root)
    }
}

/// What housekeeping reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub journal_before: u64,
    pub journal_pruned: usize,
    pub journal_after: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// What moving a whole folder did.
#[derive(Debug, Clone)]
pub struct FolderMoveReport {
    pub from: VaultPath,
    pub to: VaultPath,
    pub documents_moved: usize,
    pub links_rewritten: usize,
    pub files_rewritten: Vec<VaultPath>,
    pub unwritable: Vec<VaultPath>,
}

/// A document's current path, for building a replacement destination.
fn found_path(conn: &marius_storage::Connection, id: DocumentId) -> VaultPath {
    documents::get(conn, id)
        .ok()
        .flatten()
        .map(|d| d.path)
        .unwrap_or_else(|| VaultPath::new("unknown.md").expect("static path"))
}

/// What a rename or move did.
#[derive(Debug, Clone)]
pub struct RenameReport {
    pub document: DocumentId,
    pub from: VaultPath,
    pub to: VaultPath,
    /// Individual links updated in other people's files (MD-007).
    pub links_rewritten: usize,
    pub files_rewritten: Vec<VaultPath>,
    /// Files that reference the document but could not be rewritten safely.
    /// Reported, never silently mangled.
    pub unwritable: Vec<VaultPath>,
}

/// A rewrite worked out before the document moved, ready to apply after.
struct RewritePlan {
    document: DocumentId,
    path: VaultPath,
    /// Where the rewritten text is written. Identical to `path` for a plain
    /// rename; different when the *referrer itself* is moving as part of a
    /// folder move — the text was read from the old location and lands at
    /// the new one.
    write_path: VaultPath,
    /// The referring file's text as it was when the plan was made; the edit
    /// spans address this exact string.
    text: String,
    edits: Vec<rewrite::Edit>,
}

/// A document and everything connected to it (GRAPH-005).
#[derive(Debug, Clone)]
pub struct Neighbourhood {
    pub document: Document,
    pub outgoing: Vec<Relationship>,
    pub incoming: Vec<Relationship>,
    /// Computed, not stored (GRAPH-003).
    pub shared_tags: Vec<relationships::TagKinship>,
    /// Links from this document that point at nothing.
    pub unresolved: Vec<String>,
}

/// Give a new name the extension of the file being renamed, if it has none.
fn with_extension(new_name: &str, old: &VaultPath) -> String {
    if new_name.contains('.') {
        return new_name.to_owned();
    }
    match old.extension() {
        Some(ext) => format!("{new_name}.{ext}"),
        None => new_name.to_owned(),
    }
}

fn strip_markdown_extension(name: &str) -> &str {
    for ext in [".md", ".markdown", ".mdown", ".mkd"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem;
        }
    }
    name
}

/// Build the replacement text for a Markdown destination.
///
/// Three things have to be preserved: whether the author wrote a path relative
/// to their own folder, whether the destination was wrapped in `<>`, and
/// whether spaces need escaping.
fn markdown_replacement(
    text: &str,
    link: &marius_markdown::MarkdownLink,
    referrer_path: &VaultPath,
    new_path: &VaultPath,
) -> String {
    let was_relative = !link.target.contains('/')
        || referrer_path
            .parent()
            .is_some_and(|folder| !link.target.starts_with(folder.as_str()));
    let destination = match (was_relative, referrer_path.parent()) {
        (true, Some(folder)) if new_path.is_within(&folder) => new_path
            .as_str()
            .strip_prefix(&format!("{}/", folder.as_str()))
            .unwrap_or(new_path.as_str())
            .to_owned(),
        _ => new_path.as_str().to_owned(),
    };

    // `<…>` wrapping already permits spaces; otherwise they must be escaped.
    let wrapped = link.target_span.start > 0
        && text.as_bytes().get(link.target_span.start - 1) == Some(&b'<');
    if wrapped || !destination.contains(' ') {
        destination
    } else {
        destination.replace(' ', "%20")
    }
}
