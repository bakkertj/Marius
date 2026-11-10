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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use marius_indexing::indexer::{self, IndexOptions, IndexReport};
use marius_indexing::reconcile::Thoroughness;
use marius_indexing::{files, scan};
use marius_markdown::{rewrite, ChunkConfig, ParsedDocument};
use marius_model::{
    ContentHash, Document, DocumentId, DocumentKind, DocumentTag, Relationship, RelationshipKind,
    RevisionCause, Tag, TagName, Vault, VaultId, VaultPath, VaultPolicy,
};
use marius_model::{GlobalDocumentId, InferenceOutcome, InferenceRecord, InferenceTask};
use marius_search::Query;
use marius_storage::{chunks, inference};
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
    /// A failure explained in its own words — inference, sync, import
    /// and plugin errors all speak for themselves.
    #[error("{0}")]
    Inference(String),
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
    pub(crate) db: Database,
    pub(crate) config: Config,
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

    /// Apply a settled batch of external filesystem changes (IM-010).
    ///
    /// Per-path indexing is only safe for paths that still exist *and*
    /// are already indexed at that path — an unfamiliar new file may be
    /// the destination of a rename (watchers often report only that side),
    /// and a vanished file may be its origin. Both must go through full
    /// reconciliation so content-hash move detection keeps the document's
    /// identity (KB-006, AC-003). A platform-reported overflow also forces
    /// the full pass.
    pub fn apply_external_changes(
        &mut self,
        vault: &Vault,
        changes: &marius_indexing::watch::ExternalChanges,
    ) -> Result<ExternalReport> {
        let per_path_safe = !changes.rescan
            && changes.paths.iter().all(|path| {
                path.to_fs(&vault.root).exists()
                    && matches!(
                        documents::by_path(self.db.conn(), vault.id, path),
                        Ok(Some(_))
                    )
            });
        if !per_path_safe {
            return Ok(ExternalReport::Reindexed(self.index(vault)?));
        }
        let options = self.index_options(vault)?;
        for path in &changes.paths {
            indexer::index_path(&mut self.db, vault, path, &options)?;
        }
        Ok(ExternalReport::Indexed(changes.paths.len()))
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

    pub(crate) fn index_options(&self, vault: &Vault) -> Result<IndexOptions> {
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

        // Integrity rule 6: the moment a person touches a generated note's
        // body, regeneration loses the right to replace it (AI-013).
        if marius_storage::generated::get(self.db.conn(), id)?.is_some() {
            marius_storage::generated::mark_user_edited(self.db.conn(), id)?;
        }
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

    /// Set or clear a document's sensitivity override (SEC-005). The
    /// effective level can only be *tightened* by this — the vault floor
    /// still applies underneath (AI-007).
    pub fn set_document_sensitivity(
        &mut self,
        id: DocumentId,
        sensitivity: Option<marius_model::Sensitivity>,
    ) -> Result<()> {
        self.document(id)?; // NoSuchDocument beats a silent no-op
        Ok(documents::set_sensitivity(self.db.conn(), id, sensitivity)?)
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

    // ------------------------------------------------------ inference

    /// Propose tags for one document with the configured model (TAG-005).
    ///
    /// The privacy policy engine rules first, and *every* request becomes an
    /// audit row — approved, denied, or nothing-eligible alike (AI-015,
    /// SEC-006). With `apply`, accepted suggestions are stored as
    /// `ai_generated`/unreviewed assignments; rejected and pinned tags are
    /// skipped by the storage gate, not by hope (TAG-006, TAG-012).
    pub fn suggest_tags(&mut self, id: DocumentId, apply: bool) -> Result<SuggestedTags> {
        let document = self.document(id)?;
        let text = self.read(&document)?;
        let policy = self.vault_policy(document.vault)?;
        let sensitivity = policy.effective(document.sensitivity);
        let global = GlobalDocumentId::new(document.vault, document.id);

        let choice = self.config.models.tagging.clone();
        let profiles: Vec<marius_llm_policy::ProviderProfile> =
            choice.iter().map(|c| c.profile()).collect();
        let decision =
            marius_llm_policy::decide(&policy, InferenceTask::Tagging, sensitivity, &profiles);

        let (profile, choice) = match decision {
            marius_llm_policy::Decision::Approved { provider, .. } => (
                provider,
                choice.expect("approved implies a configured choice"),
            ),
            marius_llm_policy::Decision::Denied { reason } => {
                let record = InferenceRecord::denied(
                    InferenceTask::Tagging,
                    sensitivity,
                    vec![global],
                    reason.clone(),
                );
                inference::record(self.db.conn(), &record)?;
                return Ok(SuggestedTags::refused(record, reason));
            }
            marius_llm_policy::Decision::NoEligibleProvider { reason } => {
                let mut record = InferenceRecord::denied(
                    InferenceTask::Tagging,
                    sensitivity,
                    vec![global],
                    reason.clone(),
                );
                record.outcome = InferenceOutcome::NoEligibleProvider;
                inference::record(self.db.conn(), &record)?;
                return Ok(SuggestedTags::refused(record, reason));
            }
        };

        // Vocabulary: the vault's tags, most used first (TAG-008).
        let mut counted = tags::counts(self.db.conn(), Some(document.vault))?;
        counted.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        let vocabulary: Vec<marius_model::TagName> = counted
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .map(|(tag, _)| tag.canonical_name)
            .collect();

        let request =
            marius_classification::tagging_request(&document.title, &text, &vocabulary, 8);

        let client = match build_client(&choice, &profile) {
            Ok(client) => client,
            Err(reason) => {
                let mut record = InferenceRecord::denied(
                    InferenceTask::Tagging,
                    sensitivity,
                    vec![global],
                    reason.clone(),
                );
                record.outcome = InferenceOutcome::NoEligibleProvider;
                inference::record(self.db.conn(), &record)?;
                return Ok(SuggestedTags::refused(record, reason));
            }
        };

        let started = OffsetDateTime::now_utc();
        let clock = std::time::Instant::now();
        let mut record = InferenceRecord::denied(
            InferenceTask::Tagging,
            sensitivity,
            vec![global],
            String::new(),
        );
        record.provider = Some(profile.name.clone());
        record.model = Some(choice.model.clone());
        record.locality = Some(profile.locality);
        record.started_at = started;

        match client.chat_with(&choice.model, &request) {
            Ok(response) => {
                record.outcome = InferenceOutcome::Completed;
                record.reason = format!("tagging via {}", profile.name);
                record.prompt_tokens = response.prompt_tokens;
                record.completion_tokens = response.completion_tokens;
                record.duration_ms = Some(clock.elapsed().as_millis() as u64);
                inference::record(self.db.conn(), &record)?;

                let mut suggestions = marius_classification::parse_suggestions(&response.text, 8);
                if response.truncated {
                    // The provider hit its token budget mid-reply: the last
                    // candidate is a cut-off fragment ("hardware/re"), not
                    // a tag. The tolerant parser would happily keep it.
                    suggestions.pop();
                }
                let mut applied = Vec::new();
                let mut skipped = Vec::new();
                if apply {
                    for suggestion in &suggestions {
                        let tag =
                            tags::ensure(self.db.conn(), Some(document.vault), &suggestion.name)?;
                        let assignment = marius_model::DocumentTag::ai_generated(
                            document.id,
                            tag.id,
                            response.model.clone(),
                            suggestion.confidence,
                        );
                        match tags::assign(self.db.conn(), &assignment)? {
                            tags::Assigned::Inserted | tags::Assigned::Updated => {
                                applied.push(suggestion.name.clone());
                            }
                            tags::Assigned::SkippedRejected | tags::Assigned::SkippedProtected => {
                                skipped.push(suggestion.name.clone());
                            }
                        }
                    }
                }
                Ok(SuggestedTags {
                    record,
                    reason: format!("{} suggestion(s)", suggestions.len()),
                    suggestions,
                    applied,
                    skipped,
                    raw_reply: Some(response.text),
                })
            }
            Err(error) => {
                record.outcome = InferenceOutcome::ProviderError;
                record.reason = error.to_string();
                record.duration_ms = Some(clock.elapsed().as_millis() as u64);
                inference::record(self.db.conn(), &record)?;
                Err(CoreError::Inference(error.to_string()))
            }
        }
    }

    /// Chunk and embed every permitted document in a vault (SRCH-007,
    /// SRCH-013). Idempotent: a vector computed from the document's current
    /// content hash in the current embedding space is left alone, so a
    /// second run after one edit embeds one document's passages.
    ///
    /// Policy runs *per document* — one confidential note in a permissive
    /// vault is skipped, not sent (AI-008's spirit at the corpus level) —
    /// and the whole run becomes audit rows: one for what was embedded,
    /// one per refusal reason for what was not.
    pub fn embed_vault(&mut self, vault: &Vault) -> Result<EmbedReport> {
        let policy = self.vault_policy(vault.id)?;
        let Some(choice) = self.config.models.embedding.clone() else {
            let reason = "no provider is configured for embedding".to_owned();
            let mut record = InferenceRecord::denied(
                InferenceTask::Embedding,
                policy.default_sensitivity,
                vec![],
                reason.clone(),
            );
            record.outcome = InferenceOutcome::NoEligibleProvider;
            inference::record(self.db.conn(), &record)?;
            return Ok(EmbedReport {
                refusal: Some(reason),
                ..EmbedReport::default()
            });
        };
        let profile = choice.profile();

        let client = match build_client(&choice, &profile) {
            Ok(client) => client,
            Err(reason) => {
                let mut record = InferenceRecord::denied(
                    InferenceTask::Embedding,
                    policy.default_sensitivity,
                    vec![],
                    reason.clone(),
                );
                record.outcome = InferenceOutcome::NoEligibleProvider;
                inference::record(self.db.conn(), &record)?;
                return Ok(EmbedReport {
                    refusal: Some(reason),
                    ..EmbedReport::default()
                });
            }
        };

        let filter = documents::Filter {
            vaults: vec![vault.id],
            kinds: vec![DocumentKind::Markdown],
            ..Default::default()
        };
        let mut report = EmbedReport::default();
        let mut embedded_docs: Vec<GlobalDocumentId> = Vec::new();
        let mut refusals: BTreeMap<String, Vec<GlobalDocumentId>> = BTreeMap::new();
        let mut space: Option<marius_model::EmbeddingModel> = None;
        let started = OffsetDateTime::now_utc();
        let clock = std::time::Instant::now();

        for document in documents::list(self.db.conn(), &filter)? {
            let sensitivity = policy.effective(document.sensitivity);
            let decision = marius_llm_policy::decide(
                &policy,
                InferenceTask::Embedding,
                sensitivity,
                std::slice::from_ref(&profile),
            );
            let global = GlobalDocumentId::new(document.vault, document.id);
            if decision.approved().is_none() {
                report.skipped_policy += 1;
                refusals
                    .entry(decision.reason().to_owned())
                    .or_default()
                    .push(global);
                continue;
            }

            let text = self.read(&document)?;
            self.ensure_chunks(&document, &text)?;

            // The embedding space is named by its dimensions, which only
            // the first response reveals (SRCH-013's space key).
            let model = match &space {
                Some(model) => model.clone(),
                None => {
                    let probe = ClientKind::embed_with(&client, &choice.model, &["marius"])
                        .map_err(|e| self.record_embed_failure(&policy, &e, started, &clock))?;
                    let dims = probe.first().map(Vec::len).unwrap_or(0);
                    let model = marius_model::EmbeddingModel::new(
                        format!("{}/{}", profile.name, choice.model),
                        dims,
                    );
                    // Vectors from another model or dimension are not
                    // comparable; a model change starts the space afresh.
                    chunks::drop_other_spaces(self.db.conn(), &model)?;
                    space = Some(model.clone());
                    model
                }
            };

            let stale =
                chunks::stale_chunks(self.db.conn(), document.id, &model, &document.content_hash)?;
            if stale.is_empty() {
                report.up_to_date += 1;
                continue;
            }
            for batch in stale.chunks(16) {
                let texts: Vec<&str> = batch.iter().map(|c| c.text.as_str()).collect();
                let vectors = ClientKind::embed_with(&client, &choice.model, &texts)
                    .map_err(|e| self.record_embed_failure(&policy, &e, started, &clock))?;
                for (chunk, vector) in batch.iter().zip(vectors) {
                    chunks::put_embedding(
                        self.db.conn(),
                        &chunks::embedding_for(chunk, &model, document.content_hash, vector),
                    )?;
                }
                report.chunks_embedded += batch.len();
            }
            report.documents += 1;
            embedded_docs.push(global);
        }

        // The run's audit rows (AI-015): what went, and what was refused.
        if !embedded_docs.is_empty() || refusals.is_empty() {
            let mut record = InferenceRecord::denied(
                InferenceTask::Embedding,
                policy.default_sensitivity,
                embedded_docs.clone(),
                format!(
                    "embedded {} passage(s) across {} document(s)",
                    report.chunks_embedded, report.documents
                ),
            );
            record.provider = Some(profile.name.clone());
            record.model = Some(choice.model.clone());
            record.locality = Some(profile.locality);
            record.outcome = InferenceOutcome::Completed;
            record.started_at = started;
            record.duration_ms = Some(clock.elapsed().as_millis() as u64);
            inference::record(self.db.conn(), &record)?;
        }
        for (reason, documents) in refusals {
            let record = InferenceRecord::denied(
                InferenceTask::Embedding,
                policy.default_sensitivity,
                documents,
                reason,
            );
            inference::record(self.db.conn(), &record)?;
        }
        Ok(report)
    }

    fn record_embed_failure(
        &self,
        policy: &VaultPolicy,
        error: &marius_llm::LlmError,
        started: OffsetDateTime,
        clock: &std::time::Instant,
    ) -> CoreError {
        let mut record = InferenceRecord::denied(
            InferenceTask::Embedding,
            policy.default_sensitivity,
            vec![],
            error.to_string(),
        );
        record.outcome = InferenceOutcome::ProviderError;
        record.started_at = started;
        record.duration_ms = Some(clock.elapsed().as_millis() as u64);
        let _ = inference::record(self.db.conn(), &record);
        CoreError::Inference(error.to_string())
    }

    /// Make sure a document's stored passages describe its current text.
    fn ensure_chunks(&mut self, document: &Document, text: &str) -> Result<()> {
        let current = chunks::of_document(self.db.conn(), document.id)?;
        let fresh = !current.is_empty()
            && current.iter().all(|c| {
                text.get(c.byte_start..c.byte_end)
                    .is_some_and(|span| span == c.text)
            });
        if fresh {
            return Ok(());
        }
        let parsed = marius_markdown::ParsedDocument::parse(text);
        let drafts = marius_markdown::chunk::chunk(
            text,
            &parsed.outline,
            &marius_markdown::chunk::ChunkConfig::default(),
        );
        let fresh: Vec<marius_model::Chunk> = drafts
            .into_iter()
            .map(|d| d.into_chunk(document.id))
            .collect();
        chunks::replace(self.db.conn(), document.id, &fresh)?;
        Ok(())
    }

    /// Semantic retrieval (SRCH-007): the question embedded in the same
    /// space as the vault, ranked by cosine, one hit per document.
    ///
    /// Returns the ranked passages and, when nothing could run, the
    /// refusal in audit-log words (AI-004: an explanation, not an error).
    pub fn semantic_search(
        &mut self,
        query: &str,
        vault: VaultId,
        limit: usize,
    ) -> Result<SemanticResults> {
        let policy = self.vault_policy(vault)?;
        let Some(choice) = self.config.models.embedding.clone() else {
            return Ok(SemanticResults::refused(
                "no provider is configured for embedding".to_owned(),
            ));
        };
        let profile = choice.profile();
        // The query is the user's own words, typed just now: vault floor.
        let decision = marius_llm_policy::decide(
            &policy,
            InferenceTask::Embedding,
            policy.default_sensitivity,
            std::slice::from_ref(&profile),
        );
        if decision.approved().is_none() {
            let mut record = InferenceRecord::denied(
                InferenceTask::Embedding,
                policy.default_sensitivity,
                vec![],
                decision.reason().to_owned(),
            );
            if matches!(
                decision,
                marius_llm_policy::Decision::NoEligibleProvider { .. }
            ) {
                record.outcome = InferenceOutcome::NoEligibleProvider;
            }
            inference::record(self.db.conn(), &record)?;
            return Ok(SemanticResults::refused(decision.reason().to_owned()));
        }
        let client = match build_client(&choice, &profile) {
            Ok(client) => client,
            Err(reason) => return Ok(SemanticResults::refused(reason)),
        };

        let started = OffsetDateTime::now_utc();
        let clock = std::time::Instant::now();
        let vectors = match ClientKind::embed_with(&client, &choice.model, &[query]) {
            Ok(vectors) => vectors,
            Err(error) => return Err(self.record_embed_failure(&policy, &error, started, &clock)),
        };
        let Some(query_vector) = vectors.into_iter().next() else {
            return Ok(SemanticResults::refused(
                "provider returned no vector".into(),
            ));
        };
        let mut record = InferenceRecord::denied(
            InferenceTask::Embedding,
            policy.default_sensitivity,
            vec![],
            "query embedding for semantic search".to_owned(),
        );
        record.provider = Some(profile.name.clone());
        record.model = Some(choice.model.clone());
        record.locality = Some(profile.locality);
        record.outcome = InferenceOutcome::Completed;
        record.started_at = started;
        record.duration_ms = Some(clock.elapsed().as_millis() as u64);
        inference::record(self.db.conn(), &record)?;

        let model = marius_model::EmbeddingModel::new(
            format!("{}/{}", profile.name, choice.model),
            query_vector.len(),
        );
        let stored = chunks::embeddings_in_space(self.db.conn(), &model)?;
        let ranked = marius_embeddings::rank(&query_vector, &stored);
        let best = marius_embeddings::best_per_document(&ranked);

        let vault_record = self.vault_by_id(vault)?;
        let mut hits = Vec::new();
        for entry in best {
            if hits.len() == limit {
                break;
            }
            // Vectors can outlive their vault scope; resolve and filter.
            let Some(document) = documents::get(self.db.conn(), entry.document)? else {
                continue;
            };
            if document.vault != vault {
                continue;
            }
            let Some(chunk) = chunks::get(self.db.conn(), entry.chunk)? else {
                continue;
            };
            hits.push(SemanticHit {
                document,
                vault_name: vault_record.name.clone(),
                chunk,
                score: entry.score,
            });
        }
        Ok(SemanticResults {
            hits,
            refusal: None,
        })
    }

    /// Hybrid retrieval (SRCH-009): keyword and semantic rankings fused by
    /// reciprocal rank, so agreement between the two beats either alone and
    /// no score calibration between BM25 and cosine is needed.
    pub fn hybrid_search(
        &mut self,
        query: &str,
        vault: VaultId,
        limit: usize,
    ) -> Result<SemanticResults> {
        let keyword = {
            let options = SearchOptions {
                limit: limit.max(20),
                ..SearchOptions::default()
            };
            self.search(query, options)?
        };
        let semantic = self.semantic_search(query, vault, limit.max(20))?;
        if semantic.refusal.is_some() && keyword.hits.is_empty() {
            return Ok(semantic);
        }

        let keyword_ranked: Vec<DocumentId> = keyword
            .hits
            .iter()
            .filter(|h| h.document.vault == vault)
            .map(|h| h.document.id)
            .collect();
        let semantic_ranked: Vec<DocumentId> =
            semantic.hits.iter().map(|h| h.document.id).collect();
        let fused =
            marius_embeddings::fuse_reciprocal_rank(&[&keyword_ranked, &semantic_ranked], 60.0);

        let by_id: std::collections::HashMap<DocumentId, &SemanticHit> =
            semantic.hits.iter().map(|h| (h.document.id, h)).collect();
        let vault_record = self.vault_by_id(vault)?;
        let mut hits = Vec::new();
        for (id, _score) in fused.into_iter().take(limit) {
            if let Some(hit) = by_id.get(&id) {
                hits.push((*hit).clone());
                continue;
            }
            // Keyword-only hit: synthesise a passage from the stored text.
            let Some(document) = documents::get(self.db.conn(), id)? else {
                continue;
            };
            let snippet = keyword
                .hits
                .iter()
                .find(|h| h.document.id == id)
                .and_then(|h| h.snippet.clone())
                .unwrap_or_default();
            hits.push(SemanticHit {
                document,
                vault_name: vault_record.name.clone(),
                chunk: marius_model::Chunk {
                    id: marius_model::ChunkId::new(),
                    document: id,
                    ordinal: 0,
                    byte_start: 0,
                    byte_end: 0,
                    heading_path: Vec::new(),
                    text: snippet,
                },
                score: 0.0,
            });
        }
        Ok(SemanticResults {
            hits,
            refusal: semantic.refusal,
        })
    }

    /// Answer a question from the vault's own notes (SRCH-010..012,
    /// AC-008): retrieve semantically, show the model numbered passages,
    /// demand citations, and surface "the notes do not say" instead of
    /// letting the model improvise.
    ///
    /// The request's sensitivity is the strictest level across every
    /// *retrieved* document (AI-008): one confidential source in the
    /// context restricts the whole question.
    pub fn ask(&mut self, question: &str, vault: VaultId) -> Result<GroundedAnswer> {
        const PASSAGES: usize = 6;
        /// Below this cosine similarity a passage is noise, and sending
        /// noise to a model invites a confident answer about nothing
        /// (SRCH-011).
        const FLOOR: f32 = 0.25;

        let retrieved = self.semantic_search(question, vault, PASSAGES)?;
        if let Some(refusal) = retrieved.refusal {
            return Ok(GroundedAnswer::refused(refusal));
        }
        let relevant: Vec<&SemanticHit> =
            retrieved.hits.iter().filter(|h| h.score >= FLOOR).collect();
        if relevant.is_empty() {
            return Ok(GroundedAnswer::unanswered(
                "no sufficiently relevant notes were found; the index may need `marius embed`",
            ));
        }

        let policy = self.vault_policy(vault)?;
        let sensitivity = marius_model::Sensitivity::across(
            relevant
                .iter()
                .map(|h| policy.effective(h.document.sensitivity)),
        );
        let documents: Vec<GlobalDocumentId> = relevant
            .iter()
            .map(|h| GlobalDocumentId::new(h.document.vault, h.document.id))
            .collect();

        let Some(choice) = self.config.models.question_answering.clone() else {
            let reason = "no provider is configured for question_answering".to_owned();
            let mut record = InferenceRecord::denied(
                InferenceTask::QuestionAnswering,
                sensitivity,
                documents,
                reason.clone(),
            );
            record.outcome = InferenceOutcome::NoEligibleProvider;
            inference::record(self.db.conn(), &record)?;
            return Ok(GroundedAnswer::refused(reason));
        };
        let profile = choice.profile();
        let decision = marius_llm_policy::decide(
            &policy,
            InferenceTask::QuestionAnswering,
            sensitivity,
            std::slice::from_ref(&profile),
        );
        if decision.approved().is_none() {
            let mut record = InferenceRecord::denied(
                InferenceTask::QuestionAnswering,
                sensitivity,
                documents,
                decision.reason().to_owned(),
            );
            if matches!(
                decision,
                marius_llm_policy::Decision::NoEligibleProvider { .. }
            ) {
                record.outcome = InferenceOutcome::NoEligibleProvider;
            }
            inference::record(self.db.conn(), &record)?;
            return Ok(GroundedAnswer::refused(decision.reason().to_owned()));
        }
        let client = match build_client(&choice, &profile) {
            Ok(client) => client,
            Err(reason) => {
                let mut record = InferenceRecord::denied(
                    InferenceTask::QuestionAnswering,
                    sensitivity,
                    documents,
                    reason.clone(),
                );
                record.outcome = InferenceOutcome::NoEligibleProvider;
                inference::record(self.db.conn(), &record)?;
                return Ok(GroundedAnswer::refused(reason));
            }
        };

        let passages: Vec<marius_classification::qa::Passage> = relevant
            .iter()
            .map(|h| marius_classification::qa::Passage {
                label: format!("{} › {}", h.document.title, h.chunk.citation_label()),
                text: h.chunk.text.clone(),
            })
            .collect();
        let request = marius_classification::qa::request(question, &passages);

        let started = OffsetDateTime::now_utc();
        let clock = std::time::Instant::now();
        let mut record = InferenceRecord::denied(
            InferenceTask::QuestionAnswering,
            sensitivity,
            documents,
            String::new(),
        );
        record.provider = Some(profile.name.clone());
        record.model = Some(choice.model.clone());
        record.locality = Some(profile.locality);
        record.started_at = started;

        match client.chat_with(&choice.model, &request) {
            Ok(response) => {
                record.outcome = InferenceOutcome::Completed;
                record.reason = format!("grounded answer via {}", profile.name);
                record.prompt_tokens = response.prompt_tokens;
                record.completion_tokens = response.completion_tokens;
                record.duration_ms = Some(clock.elapsed().as_millis() as u64);
                inference::record(self.db.conn(), &record)?;

                let cited =
                    marius_classification::qa::cited_passages(&response.text, relevant.len());
                let citations: Vec<marius_model::Citation> = cited
                    .iter()
                    .map(|&index| {
                        let hit = relevant[index];
                        marius_model::Citation {
                            document: GlobalDocumentId::new(hit.document.vault, hit.document.id),
                            chunk: Some(hit.chunk.id),
                            label: format!(
                                "{} › {}",
                                hit.document.title,
                                hit.chunk.citation_label()
                            ),
                            byte_range: Some((hit.chunk.byte_start, hit.chunk.byte_end)),
                        }
                    })
                    .collect();
                let unanswered = marius_classification::qa::is_unanswered(&cited);
                Ok(GroundedAnswer {
                    text: response.text,
                    citations,
                    unanswered,
                    refusal: None,
                })
            }
            Err(error) => {
                record.outcome = InferenceOutcome::ProviderError;
                record.reason = error.to_string();
                record.duration_ms = Some(clock.elapsed().as_millis() as u64);
                inference::record(self.db.conn(), &record)?;
                Err(CoreError::Inference(error.to_string()))
            }
        }
    }

    /// Write (or regenerate) a synthesis note from source documents
    /// (AI-011..AI-014, AC-010).
    ///
    /// The note lands as a real Markdown file with a Sources section of
    /// wiki links, is marked `Derived`, and carries a provenance row
    /// naming model, prompt and sources. Regeneration replaces the body
    /// in place ONLY while no person has edited it; afterwards a sibling
    /// note is written instead — the user's bytes are never the model's
    /// to take back (integrity rule 6).
    pub fn summarize(
        &mut self,
        ids: &[DocumentId],
        output: Option<&VaultPath>,
    ) -> Result<Summarized> {
        if ids.is_empty() {
            return Err(CoreError::Inference("no source documents given".into()));
        }
        let mut documents = Vec::with_capacity(ids.len());
        for id in ids {
            documents.push(self.document(*id)?);
        }
        let vault_id = documents[0].vault;
        if documents.iter().any(|d| d.vault != vault_id) {
            return Err(CoreError::Inference(
                "sources must belong to one vault".into(),
            ));
        }
        let vault = self.vault_by_id(vault_id)?;
        let policy = self.vault_policy(vault_id)?;
        let sensitivity = marius_model::Sensitivity::across(
            documents.iter().map(|d| policy.effective(d.sensitivity)),
        );
        let globals: Vec<GlobalDocumentId> = documents
            .iter()
            .map(|d| GlobalDocumentId::new(d.vault, d.id))
            .collect();

        let Some(choice) = self.config.models.summarization.clone() else {
            let reason = "no provider is configured for summarization".to_owned();
            let mut record = InferenceRecord::denied(
                InferenceTask::Summarization,
                sensitivity,
                globals,
                reason.clone(),
            );
            record.outcome = InferenceOutcome::NoEligibleProvider;
            inference::record(self.db.conn(), &record)?;
            return Ok(Summarized::Refused(reason));
        };
        let profile = choice.profile();
        let decision = marius_llm_policy::decide(
            &policy,
            InferenceTask::Summarization,
            sensitivity,
            std::slice::from_ref(&profile),
        );
        if decision.approved().is_none() {
            let mut record = InferenceRecord::denied(
                InferenceTask::Summarization,
                sensitivity,
                globals,
                decision.reason().to_owned(),
            );
            if matches!(
                decision,
                marius_llm_policy::Decision::NoEligibleProvider { .. }
            ) {
                record.outcome = InferenceOutcome::NoEligibleProvider;
            }
            inference::record(self.db.conn(), &record)?;
            return Ok(Summarized::Refused(decision.reason().to_owned()));
        }
        let client = match build_client(&choice, &profile) {
            Ok(client) => client,
            Err(reason) => {
                let mut record = InferenceRecord::denied(
                    InferenceTask::Summarization,
                    sensitivity,
                    globals,
                    reason.clone(),
                );
                record.outcome = InferenceOutcome::NoEligibleProvider;
                inference::record(self.db.conn(), &record)?;
                return Ok(Summarized::Refused(reason));
            }
        };

        let mut sources = Vec::with_capacity(documents.len());
        for document in &documents {
            sources.push(marius_classification::summary::Source {
                title: document.title.clone(),
                text: self.read(document)?,
            });
        }
        let request = marius_classification::summary::request(&sources);

        let started = OffsetDateTime::now_utc();
        let clock = std::time::Instant::now();
        let mut record = InferenceRecord::denied(
            InferenceTask::Summarization,
            sensitivity,
            globals,
            String::new(),
        );
        record.provider = Some(profile.name.clone());
        record.model = Some(choice.model.clone());
        record.locality = Some(profile.locality);
        record.started_at = started;

        let response = match client.chat_with(&choice.model, &request) {
            Ok(response) => {
                record.outcome = InferenceOutcome::Completed;
                record.reason = format!("summary of {} note(s)", documents.len());
                record.prompt_tokens = response.prompt_tokens;
                record.completion_tokens = response.completion_tokens;
                record.duration_ms = Some(clock.elapsed().as_millis() as u64);
                inference::record(self.db.conn(), &record)?;
                response
            }
            Err(error) => {
                record.outcome = InferenceOutcome::ProviderError;
                record.reason = error.to_string();
                record.duration_ms = Some(clock.elapsed().as_millis() as u64);
                inference::record(self.db.conn(), &record)?;
                return Err(CoreError::Inference(error.to_string()));
            }
        };

        // Where the note lives, and whether we may overwrite what is there.
        let title = if documents.len() == 1 {
            format!("Summary of {}", documents[0].title)
        } else {
            format!("Synthesis of {} notes", documents.len())
        };
        let path = match output {
            Some(path) => path.clone(),
            None => {
                let name = marius_model::sanitize(&format!("{title}.md"));
                VaultPath::new(format!("generated/{name}"))
                    .map_err(|e| CoreError::Inference(e.to_string()))?
            }
        };

        let source_titles: Vec<String> = documents.iter().map(|d| d.title.clone()).collect();
        let body =
            marius_classification::summary::note_body(&title, &response.text, &source_titles);

        let mut wrote_sibling = false;
        let target = match self.document_at(vault_id, &path)? {
            Some(existing) => {
                let provenance = marius_storage::generated::get(self.db.conn(), existing.id)?;
                let replaceable = existing.authorship == marius_model::Authorship::Derived
                    && provenance
                        .as_ref()
                        .is_some_and(|p| p.allows_in_place_regeneration());
                if replaceable {
                    // In-place regeneration adds a revision; the previous
                    // body stays recoverable (AI-013).
                    self.save_document(existing.id, &body)?;
                    existing
                } else {
                    // A person edited it (or it never was generated):
                    // their bytes stay; the fresh run lands beside them
                    // (AC-010).
                    wrote_sibling = true;
                    let stem = path.as_str().trim_end_matches(".md");
                    let sibling = VaultPath::new(format!("{stem} (regenerated).md"))
                        .map_err(|e| CoreError::Inference(e.to_string()))?;
                    match self.document_at(vault_id, &sibling)? {
                        Some(previous_run) => {
                            self.save_document(previous_run.id, &body)?;
                            previous_run
                        }
                        None => self.create_document(&vault, &sibling, &body)?,
                    }
                }
            }
            None => self.create_document(&vault, &path, &body)?,
        };

        documents::set_authorship(self.db.conn(), target.id, marius_model::Authorship::Derived)?;
        let revision = revisions::latest(self.db.conn(), target.id)?
            .map(|r| r.id)
            .unwrap_or_else(marius_model::RevisionId::new);
        let provenance = marius_model::GeneratedNoteProvenance {
            document: target.id,
            revision,
            inference: record.id,
            task: InferenceTask::Summarization,
            provider: profile.name.clone(),
            model: response.model.clone(),
            prompt_id: Some(marius_classification::summary::PROMPT_ID.to_owned()),
            sources: documents
                .iter()
                .map(|d| marius_model::Citation {
                    document: GlobalDocumentId::new(d.vault, d.id),
                    chunk: None,
                    label: d.title.clone(),
                    byte_range: None,
                })
                .collect(),
            generated_at: started,
            user_edited: false,
        };
        marius_storage::generated::put(self.db.conn(), &provenance)?;
        let document = self.document(target.id)?;
        Ok(Summarized::Written {
            document: Box::new(document),
            provenance: Box::new(provenance),
            wrote_sibling,
        })
    }

    /// The provenance of a generated note, if the document is one (AI-011).
    pub fn note_provenance(
        &self,
        id: DocumentId,
    ) -> Result<Option<marius_model::GeneratedNoteProvenance>> {
        Ok(marius_storage::generated::get(self.db.conn(), id)?)
    }

    /// Promote a generated note to a protected, user-owned document
    /// (AI-014). From here on it is treated exactly like a source note:
    /// regeneration writes siblings, automation keeps its hands off.
    pub fn promote_note(&mut self, id: DocumentId) -> Result<()> {
        let document = self.document(id)?;
        if document.authorship != marius_model::Authorship::Derived {
            return Err(CoreError::Inference(format!(
                "{} is not a generated note",
                document.path
            )));
        }
        documents::set_authorship(self.db.conn(), id, marius_model::Authorship::Promoted)?;
        Ok(())
    }

    /// Propose semantic relationships between documents (GRAPH-007).
    ///
    /// Embeddings nominate the candidate pairs; the model judges each and
    /// explains itself; the edge lands `ai_generated`/unreviewed with the
    /// rationale attached, behind the same storage gate that keeps a
    /// rejected pair rejected (GRAPH-011, GRAPH-012).
    pub fn discover_relationships(
        &mut self,
        vault: &Vault,
        limit: usize,
    ) -> Result<DiscoveryReport> {
        let policy = self.vault_policy(vault.id)?;
        let choice = self
            .config
            .models
            .relationship_discovery
            .clone()
            .or_else(|| self.config.models.tagging.clone());
        let Some(choice) = choice else {
            let reason =
                "no provider is configured for relationship_discovery (or tagging)".to_owned();
            let mut record = InferenceRecord::denied(
                InferenceTask::RelationshipDiscovery,
                policy.default_sensitivity,
                vec![],
                reason.clone(),
            );
            record.outcome = InferenceOutcome::NoEligibleProvider;
            inference::record(self.db.conn(), &record)?;
            return Ok(DiscoveryReport {
                refusal: Some(reason),
                ..DiscoveryReport::default()
            });
        };
        let profile = choice.profile();
        let client = match build_client(&choice, &profile) {
            Ok(client) => client,
            Err(reason) => {
                return Ok(DiscoveryReport {
                    refusal: Some(reason),
                    ..DiscoveryReport::default()
                })
            }
        };

        let stored = chunks::dominant_space_embeddings(self.db.conn())?;
        if stored.is_empty() {
            return Ok(DiscoveryReport {
                refusal: Some("no embeddings; run `marius embed` first".into()),
                ..DiscoveryReport::default()
            });
        }
        let candidates = marius_embeddings::similar_document_pairs(&stored, 0.55, limit * 3);

        let mut report = DiscoveryReport::default();
        let mut judged: Vec<GlobalDocumentId> = Vec::new();
        let started = OffsetDateTime::now_utc();
        let clock = std::time::Instant::now();

        for (a, b, _score) in candidates {
            if report.proposed + report.judged_unrelated >= limit {
                break;
            }
            let (Some(doc_a), Some(doc_b)) = (
                documents::get(self.db.conn(), a)?,
                documents::get(self.db.conn(), b)?,
            ) else {
                continue;
            };
            if doc_a.vault != vault.id || doc_b.vault != vault.id {
                continue;
            }
            let endpoints = marius_model::Endpoints::new(
                GlobalDocumentId::new(doc_a.vault, doc_a.id),
                GlobalDocumentId::new(doc_b.vault, doc_b.id),
            );
            // Already connected, by any stored kind? Nothing to discover.
            if !relationships::touching(self.db.conn(), doc_a.id)?
                .iter()
                .all(|r| {
                    r.endpoints.source.document != doc_b.id
                        && r.endpoints.target.document != doc_b.id
                })
            {
                report.skipped_existing += 1;
                continue;
            }
            // A durable rejection means never re-proposing (GRAPH-011).
            let probe = marius_model::Relationship::new(
                endpoints,
                marius_model::RelationshipKind::Semantic,
            );
            if relationships::rejections(self.db.conn(), doc_a.id)?
                .iter()
                .any(|r| r.suppresses(&probe))
            {
                report.skipped_rejected += 1;
                continue;
            }

            let sensitivity = marius_model::Sensitivity::across([
                policy.effective(doc_a.sensitivity),
                policy.effective(doc_b.sensitivity),
            ]);
            let decision = marius_llm_policy::decide(
                &policy,
                InferenceTask::RelationshipDiscovery,
                sensitivity,
                std::slice::from_ref(&profile),
            );
            if decision.approved().is_none() {
                report.skipped_policy += 1;
                continue;
            }

            let request = marius_classification::discovery::request(
                &doc_a.title,
                &self.read(&doc_a)?,
                &doc_b.title,
                &self.read(&doc_b)?,
            );
            let response = match client.chat_with(&choice.model, &request) {
                Ok(response) => response,
                Err(error) => {
                    let mut record = InferenceRecord::denied(
                        InferenceTask::RelationshipDiscovery,
                        sensitivity,
                        judged,
                        error.to_string(),
                    );
                    record.outcome = InferenceOutcome::ProviderError;
                    record.started_at = started;
                    record.duration_ms = Some(clock.elapsed().as_millis() as u64);
                    inference::record(self.db.conn(), &record)?;
                    return Err(CoreError::Inference(error.to_string()));
                }
            };
            judged.push(probe.endpoints.source);
            judged.push(probe.endpoints.target);
            let verdict = marius_classification::discovery::parse_verdict(&response.text);
            if !verdict.related {
                report.judged_unrelated += 1;
                continue;
            }
            let mut edge = probe;
            edge.confidence = Some(verdict.confidence);
            edge.model = Some(response.model.clone());
            edge.rationale = verdict.rationale;
            match relationships::upsert(self.db.conn(), &edge)? {
                relationships::Upserted::SkippedRejected
                | relationships::Upserted::SkippedProtected => {
                    report.skipped_rejected += 1;
                }
                _ => report.proposed += 1,
            }
        }

        let mut record = InferenceRecord::denied(
            InferenceTask::RelationshipDiscovery,
            policy.default_sensitivity,
            judged,
            format!(
                "{} pair(s) judged, {} edge(s) proposed",
                report.proposed + report.judged_unrelated,
                report.proposed
            ),
        );
        record.provider = Some(profile.name.clone());
        record.model = Some(choice.model.clone());
        record.locality = Some(profile.locality);
        record.outcome = InferenceOutcome::Completed;
        record.started_at = started;
        record.duration_ms = Some(clock.elapsed().as_millis() as u64);
        inference::record(self.db.conn(), &record)?;
        Ok(report)
    }

    /// Draw an edge yourself (GRAPH-010): kind `Asserted`, provenance
    /// manual, no review needed.
    pub fn assert_relationship(
        &mut self,
        source: DocumentId,
        target: DocumentId,
    ) -> Result<marius_model::Relationship> {
        let a = self.document(source)?;
        let b = self.document(target)?;
        let edge = marius_model::Relationship::new(
            marius_model::Endpoints::new(
                GlobalDocumentId::new(a.vault, a.id),
                GlobalDocumentId::new(b.vault, b.id),
            ),
            marius_model::RelationshipKind::Asserted,
        );
        relationships::upsert(self.db.conn(), &edge)?;
        Ok(edge)
    }

    /// Accept a machine-proposed edge between two documents (GRAPH-010),
    /// which also protects it from re-inference (GRAPH-012).
    pub fn confirm_relationship(&mut self, source: DocumentId, target: DocumentId) -> Result<bool> {
        match self.edge_between(source, target)? {
            Some(edge) => Ok(relationships::confirm(self.db.conn(), edge.id)?),
            None => Ok(false),
        }
    }

    /// Reject the edge between two documents; the rejection is durable
    /// against rediscovery and sync (GRAPH-011, integrity rule 7).
    pub fn reject_relationship(
        &mut self,
        source: DocumentId,
        target: DocumentId,
        reason: Option<&str>,
    ) -> Result<bool> {
        match self.edge_between(source, target)? {
            Some(edge) => Ok(relationships::reject(self.db.conn(), edge.id, reason)?),
            None => Ok(false),
        }
    }

    fn edge_between(
        &self,
        source: DocumentId,
        target: DocumentId,
    ) -> Result<Option<marius_model::Relationship>> {
        // Prefer the machine-proposed kinds a user would be reviewing.
        let a = self.document(source)?;
        let b = self.document(target)?;
        let endpoints = marius_model::Endpoints::new(
            GlobalDocumentId::new(a.vault, a.id),
            GlobalDocumentId::new(b.vault, b.id),
        );
        for kind in [
            marius_model::RelationshipKind::Semantic,
            marius_model::RelationshipKind::Asserted,
        ] {
            if let Some(edge) = relationships::by_endpoints(self.db.conn(), endpoints, kind)? {
                return Ok(Some(edge));
            }
        }
        Ok(None)
    }

    /// Documents in `vault` with no machine-proposed tags yet — the
    /// backlog `marius classify` works through (AI-010).
    pub fn unclassified(&self, vault: VaultId) -> Result<Vec<Document>> {
        let filter = documents::Filter {
            vaults: vec![vault],
            kinds: vec![DocumentKind::Markdown],
            exclude_generated: true,
            ..Default::default()
        };
        let mut out = Vec::new();
        for document in documents::list(self.db.conn(), &filter)? {
            let has_machine_tag = self.document_tags(document.id)?.iter().any(|(_, dt)| {
                matches!(
                    dt.provenance,
                    marius_model::Provenance::AiGenerated | marius_model::Provenance::AiConfirmed
                )
            });
            if !has_machine_tag {
                out.push(document);
            }
        }
        Ok(out)
    }

    /// The inference audit log, newest first (AI-015, SEC-006).
    pub fn inference_log(&self, limit: usize) -> Result<Vec<InferenceRecord>> {
        Ok(inference::recent(self.db.conn(), limit)?)
    }

    // -------------------------------------------- templates and dailies

    /// Open — creating if needed — today's daily note (MD-011).
    ///
    /// `{templates_folder}/daily.md` seeds the body when it exists;
    /// placeholders `{{date}}`, `{{time}}` and `{{title}}` expand.
    /// Idempotent: the second call today returns the same note.
    pub fn daily_note(&mut self, vault: &Vault) -> Result<Document> {
        let now = OffsetDateTime::now_utc();
        let name = crate::templates::daily_name(now);
        let path = VaultPath::new(format!("{}/{name}", self.config.daily_folder))
            .map_err(|e| CoreError::Inference(e.to_string()))?;
        if let Some(existing) = self.document_at(vault.id, &path)? {
            return Ok(existing);
        }
        let title = name.trim_end_matches(".md").to_owned();
        let template_path = vault
            .root
            .join(&self.config.templates_folder)
            .join("daily.md");
        let body = match std::fs::read_to_string(&template_path) {
            Ok(template) => crate::templates::expand(&template, &title, now),
            Err(_) => format!("# {title}\n\n"),
        };
        self.create_document(vault, &path, &body)
    }

    /// Templates available in this vault: the stems of the Markdown
    /// files in the templates folder.
    pub fn templates(&self, vault: &Vault) -> Vec<String> {
        let dir = vault.root.join(&self.config.templates_folder);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".md").map(str::to_owned)
            })
            .collect();
        names.sort();
        names
    }

    /// Create a note from a template (MD-011): placeholders expanded,
    /// the file placed at `path`.
    pub fn new_from_template(
        &mut self,
        vault: &Vault,
        template: &str,
        path: &VaultPath,
    ) -> Result<Document> {
        let template_path = vault
            .root
            .join(&self.config.templates_folder)
            .join(format!("{template}.md"));
        let body = std::fs::read_to_string(&template_path).map_err(|e| {
            CoreError::Inference(format!(
                "no template {template:?} in {}/: {e}",
                self.config.templates_folder
            ))
        })?;
        let title = path.file_stem().to_owned();
        let body = crate::templates::expand(&body, &title, OffsetDateTime::now_utc());
        self.create_document(vault, path, &body)
    }

    // ------------------------------------------------------- importing

    /// Import an external file into the vault (IM-001/002/008/009/011).
    ///
    /// Markdown and plain text are copied as they are; HTML is converted;
    /// `.eml` becomes one note (attachments placed beside it); `.mbox`
    /// becomes one note per message. Everything indexes immediately
    /// (IM-005), names are made portable, and an occupied name gets a
    /// numbered variant rather than an overwrite (IM-003's
    /// non-destructive instinct).
    pub fn import_file(
        &mut self,
        vault: &Vault,
        source: &Path,
        folder: Option<&VaultPath>,
    ) -> Result<ImportReport> {
        let bytes = std::fs::read(source)
            .map_err(|e| CoreError::Inference(format!("cannot read {}: {e}", source.display())))?;
        let stem = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "import".to_owned());
        let extension = source
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        let mut notes: Vec<marius_import::ImportedNote> = Vec::new();
        let mut skipped = 0usize;
        match extension.as_str() {
            "md" | "markdown" | "txt" => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let name = source
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{stem}.md"));
                notes.push(marius_import::ImportedNote {
                    file_name: name,
                    body: text,
                    attachments: Vec::new(),
                });
            }
            "html" | "htm" => {
                let html = String::from_utf8_lossy(&bytes);
                notes.push(
                    marius_import::html::import(&html, &stem, Some(&source.display().to_string()))
                        .map_err(|e| CoreError::Inference(e.to_string()))?,
                );
            }
            "eml" => {
                notes.push(
                    marius_import::email::import_eml(&bytes)
                        .map_err(|e| CoreError::Inference(e.to_string()))?,
                );
            }
            "msg" => {
                notes.push(
                    marius_import::msg::import_msg(&bytes)
                        .map_err(|e| CoreError::Inference(e.to_string()))?,
                );
            }
            "mbox" => {
                let (parsed, failed) = marius_import::email::import_mbox(&bytes);
                skipped = failed;
                notes.extend(parsed);
            }
            "pdf" => {
                notes.push(
                    marius_import::documents::import_pdf(&bytes, &stem)
                        .map_err(|e| CoreError::Inference(e.to_string()))?,
                );
            }
            "docx" => {
                notes.push(
                    marius_import::documents::import_docx(&bytes, &stem)
                        .map_err(|e| CoreError::Inference(e.to_string()))?,
                );
            }
            "epub" => {
                notes.push(
                    marius_import::documents::import_epub(&bytes, &stem)
                        .map_err(|e| CoreError::Inference(e.to_string()))?,
                );
            }
            other => {
                return Err(CoreError::Inference(format!(
                    "no importer for .{other} (markdown, txt, html, pdf, docx, epub, eml, msg, mbox)"
                )));
            }
        }

        let mut report = ImportReport {
            skipped,
            ..ImportReport::default()
        };
        for note in notes {
            let document = self.place_imported(vault, folder, &note)?;
            report.imported.push(document);
        }
        Ok(report)
    }

    /// Clip a web page into the vault (IM-012): fetched, converted,
    /// titled by the page, with the URL in frontmatter as `source:`.
    pub fn import_url(
        &mut self,
        vault: &Vault,
        url: &str,
        folder: Option<&VaultPath>,
    ) -> Result<Document> {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(CoreError::Inference(format!("not a web address: {url}")));
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("marius-clipper")
            .build()
            .expect("constructing an HTTP client cannot fail with these options");
        let response = client
            .get(url)
            .send()
            .map_err(|e| CoreError::Inference(format!("cannot fetch {url}: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(CoreError::Inference(format!(
                "{url} answered HTTP {status}"
            )));
        }
        let html = response
            .text()
            .map_err(|e| CoreError::Inference(e.to_string()))?;
        let fallback = url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("clipping");
        let note = marius_import::html::import(&html, fallback, Some(url))
            .map_err(|e| CoreError::Inference(e.to_string()))?;
        self.place_imported(vault, folder, &note)
    }

    fn place_imported(
        &mut self,
        vault: &Vault,
        folder: Option<&VaultPath>,
        note: &marius_import::ImportedNote,
    ) -> Result<Document> {
        let base = marius_model::sanitize(&note.file_name);
        let base = if base.to_lowercase().ends_with(".md") {
            base
        } else {
            format!("{base}.md")
        };
        let joined = |name: &str| -> Result<VaultPath> {
            let relative = match folder {
                Some(folder) => format!("{folder}/{name}"),
                None => name.to_owned(),
            };
            VaultPath::new(relative).map_err(|e| CoreError::Inference(e.to_string()))
        };

        // Find a free name: "x.md", then "x (imported 2).md", …
        let mut path = joined(&base)?;
        let mut counter = 1usize;
        while self.document_at(vault.id, &path)?.is_some() || path.to_fs(&vault.root).exists() {
            counter += 1;
            let stem = base.trim_end_matches(".md");
            path = joined(&format!("{stem} (imported {counter}).md"))?;
        }

        // Attachments land beside the note, under "<stem> attachments/",
        // and the note ends with a clickable list of them — an email's
        // four PDFs should be one click away, not a folder hunt (IM-011).
        let mut body = note.body.clone();
        let mut targets = Vec::new();
        if !note.attachments.is_empty() {
            let stem = path.as_str().trim_end_matches(".md").to_owned();
            let folder_name = stem.rsplit('/').next().unwrap_or(&stem).to_owned();
            body.push_str("\n\n## Attachments\n\n");
            for attachment in &note.attachments {
                let name = marius_model::sanitize(&attachment.file_name);
                let target = VaultPath::new(format!("{stem} attachments/{name}"))
                    .map_err(|e| CoreError::Inference(e.to_string()))?;
                // Angle brackets keep the spaces in the folder name legal.
                body.push_str(&format!("- [{name}](<{folder_name} attachments/{name}>)\n"));
                targets.push((target, attachment.bytes.clone()));
            }
        }

        let document = self.create_document(vault, &path, &body)?;

        if !targets.is_empty() {
            let options = self.index_options(vault)?;
            for (target, bytes) in &targets {
                files::write_atomic_bytes(&target.to_fs(&vault.root), bytes)?;
                indexer::index_path(&mut self.db, vault, target, &options)?;
            }
        }
        Ok(document)
    }

    // ----------------------------------------------- search conveniences

    /// Fuzzy title/path matching (SRCH-006): the quick switcher's
    /// forgiving half, for when exact words escape you.
    pub fn fuzzy_titles(&self, query: &str, limit: usize) -> Result<Vec<Document>> {
        let filter = documents::Filter::default();
        let all = documents::list(self.db.conn(), &filter)?;
        let labels: Vec<String> = all
            .iter()
            .map(|d| format!("{} {}", d.title, d.path))
            .collect();
        let pairs: Vec<(usize, &String)> = marius_search::fuzzy::rank(query, &labels, |l| l);
        Ok(pairs
            .into_iter()
            .take(limit)
            .map(|(index, _)| all[index].clone())
            .collect())
    }

    /// Store a named query (SRCH-006, TAG-015).
    pub fn save_search(&mut self, name: &str, query: &str) -> Result<()> {
        // Refuse to save a query that will not parse: a saved search
        // should never greet its owner with a syntax error later.
        marius_search::parse(query)?;
        Ok(marius_storage::saved_searches::save(
            self.db.conn(),
            name,
            query,
        )?)
    }

    pub fn saved_searches(&self) -> Result<Vec<(String, String)>> {
        Ok(marius_storage::saved_searches::list(self.db.conn())?)
    }

    pub fn saved_search(&self, name: &str) -> Result<Option<String>> {
        Ok(marius_storage::saved_searches::get(self.db.conn(), name)?)
    }

    pub fn remove_saved_search(&mut self, name: &str) -> Result<bool> {
        Ok(marius_storage::saved_searches::remove(
            self.db.conn(),
            name,
        )?)
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

    pub(crate) fn vault_by_id(&self, id: VaultId) -> Result<Vault> {
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

/// The outcome of one tagging request (TAG-005), refusals included.
#[derive(Debug)]
pub struct SuggestedTags {
    /// The audit row this request produced — there is always one (AI-015).
    pub record: InferenceRecord,
    /// Human-sized summary: the engine's refusal reason, or a count.
    pub reason: String,
    pub suggestions: Vec<marius_classification::TagSuggestion>,
    /// Stored (with `apply`): new or refreshed machine assignments.
    pub applied: Vec<marius_model::TagName>,
    /// Refused by the storage gate: rejected or pinned (TAG-006, TAG-012).
    pub skipped: Vec<marius_model::TagName>,
    /// The model's verbatim reply — what it proposed before parsing and
    /// the prose-fragment guard. The only way to tell "the model never
    /// said it" from "Marius filtered it out".
    pub raw_reply: Option<String>,
}

impl SuggestedTags {
    fn refused(record: InferenceRecord, reason: String) -> Self {
        Self {
            record,
            reason,
            suggestions: Vec::new(),
            applied: Vec::new(),
            skipped: Vec::new(),
            raw_reply: None,
        }
    }
}

/// The outcome of one summarisation request (AI-011, AC-010).
#[derive(Debug)]
pub enum Summarized {
    /// The policy engine or configuration stopped it; the reason is in
    /// audit-log words and an audit row exists.
    Refused(String),
    Written {
        document: Box<Document>,
        provenance: Box<marius_model::GeneratedNoteProvenance>,
        /// True when the target note was user-edited (or user-owned) and
        /// the fresh run landed beside it instead (integrity rule 6).
        wrote_sibling: bool,
    },
}

/// What one discovery run did (GRAPH-007).
#[derive(Debug, Default)]
pub struct DiscoveryReport {
    /// New `Semantic` edges stored, unreviewed, with rationale.
    pub proposed: usize,
    /// The model looked and said no.
    pub judged_unrelated: usize,
    /// Pairs already connected by a stored edge.
    pub skipped_existing: usize,
    /// Pairs the user rejected before; never re-proposed (GRAPH-011).
    pub skipped_rejected: usize,
    /// Pairs the policy engine refused to send.
    pub skipped_policy: usize,
    /// Why nothing ran at all, when nothing did.
    pub refusal: Option<String>,
}

/// An answer grounded in the vault's own passages (SRCH-010, AC-008).
#[derive(Debug)]
pub struct GroundedAnswer {
    /// The model's prose, `[n]` citation markers left in place.
    pub text: String,
    /// The passages actually cited, in first-use order, clickable by byte
    /// range (SRCH-010).
    pub citations: Vec<marius_model::Citation>,
    /// True when the answer cites nothing — the notes do not say
    /// (SRCH-011), and the caller should present it that way.
    pub unanswered: bool,
    /// Set when policy or configuration stopped the question entirely.
    pub refusal: Option<String>,
}

impl GroundedAnswer {
    fn refused(reason: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            citations: Vec::new(),
            unanswered: true,
            refusal: Some(reason.into()),
        }
    }

    fn unanswered(note: impl Into<String>) -> Self {
        Self {
            text: note.into(),
            citations: Vec::new(),
            unanswered: true,
            refusal: None,
        }
    }
}

/// What one `embed_vault` run did (SRCH-007, SRCH-013).
#[derive(Debug, Default)]
pub struct EmbedReport {
    /// Documents whose passages were (re)embedded this run.
    pub documents: usize,
    pub chunks_embedded: usize,
    /// Documents already current in this embedding space.
    pub up_to_date: usize,
    /// Documents the policy engine refused to send (per-document gate).
    pub skipped_policy: usize,
    /// Why nothing ran at all, when nothing did.
    pub refusal: Option<String>,
}

/// One semantically ranked result: a document, through its best passage.
#[derive(Debug, Clone)]
pub struct SemanticHit {
    pub document: Document,
    pub vault_name: String,
    /// The passage that matched — the unit of citation (SRCH-008).
    pub chunk: marius_model::Chunk,
    /// Cosine similarity; 0.0 for keyword-only entries in a hybrid list.
    pub score: f32,
}

/// Semantic or hybrid results, or the reason there are none.
#[derive(Debug, Default)]
pub struct SemanticResults {
    pub hits: Vec<SemanticHit>,
    /// Set when the policy engine or configuration refused to run.
    pub refusal: Option<String>,
}

impl SemanticResults {
    fn refused(reason: String) -> Self {
        Self {
            hits: Vec::new(),
            refusal: Some(reason),
        }
    }
}

/// What one import produced (IM-005: everything already indexed).
#[derive(Debug, Default)]
pub struct ImportReport {
    pub imported: Vec<Document>,
    /// Messages in an archive that could not be parsed. Counted, named in
    /// the CLI output, never silently dropped.
    pub skipped: usize,
}

/// What `apply_external_changes` did.
#[derive(Debug)]
pub enum ExternalReport {
    /// Each changed path was indexed individually.
    Indexed(usize),
    /// Something vanished or overflowed; the vault was reconciled in full.
    Reindexed(IndexReport),
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

/// A constructed provider client. One enum rather than two trait objects,
/// because both concrete providers implement both capabilities and the
/// callers want whichever one the task needs.
enum ClientKind {
    Ollama(marius_llm::ollama::Ollama),
    OpenAi(marius_llm::openai::OpenAiCompatible),
}

impl ClientKind {
    fn chat_with(
        &self,
        model: &str,
        request: &marius_llm::ChatRequest,
    ) -> std::result::Result<marius_llm::ChatResponse, marius_llm::LlmError> {
        use marius_llm::ChatProvider;
        match self {
            Self::Ollama(c) => c.chat(model, request),
            Self::OpenAi(c) => c.chat(model, request),
        }
    }

    fn embed_with(
        &self,
        model: &str,
        texts: &[&str],
    ) -> std::result::Result<Vec<Vec<f32>>, marius_llm::LlmError> {
        use marius_llm::EmbeddingProvider;
        match self {
            Self::Ollama(c) => c.embed(model, texts),
            Self::OpenAi(c) => c.embed(model, texts),
        }
    }
}

/// Build the client for an approved provider, or say why it cannot be
/// built — a missing credential or base URL is "nothing eligible", not a
/// crash (AI-004). Credentials are fetched from the environment by the
/// name the configuration holds; the key itself is never stored (SEC-001).
fn build_client(
    choice: &crate::config::ModelChoice,
    profile: &marius_llm_policy::ProviderProfile,
) -> std::result::Result<ClientKind, String> {
    if profile.name == "ollama" {
        return Ok(ClientKind::Ollama(marius_llm::ollama::Ollama::new(
            choice
                .base_url
                .clone()
                .unwrap_or_else(|| marius_llm::ollama::DEFAULT_BASE_URL.to_owned()),
        )));
    }
    let key = match &choice.credential {
        Some(name) => match std::env::var(name) {
            Ok(key) => Some(key),
            Err(_) => {
                return Err(format!("credential {name:?} is not set in the environment"));
            }
        },
        None => None,
    };
    let base = choice.base_url.clone().ok_or_else(|| {
        format!(
            "provider {:?} needs a base_url in the configuration",
            profile.name
        )
    })?;
    Ok(ClientKind::OpenAi(
        marius_llm::openai::OpenAiCompatible::new(
            profile.name.clone(),
            base,
            profile.locality,
            key,
        ),
    ))
}
