//! Export and migration (KB-014): the vault's files plus a metadata
//! sidecar carrying what the files alone cannot — stable identities,
//! tag provenance and review, non-derived relationships, rejections.
//!
//! The files ARE the export; the sidecar is what makes a re-opened copy
//! the *same* vault rather than a lookalike: `import_metadata` matches
//! by path and adopts the exported identities, so links by id, sync
//! history and review decisions keep meaning what they meant.

use std::path::Path;

use marius_model::{
    Document, DocumentTag, GlobalDocumentId, Provenance, RejectedRelationship, Relationship,
    TagName, Vault,
};
use marius_storage::{documents, relationships, replica, tags};
use serde::{Deserialize, Serialize};

use crate::app::{CoreError, Marius, Result};

/// The sidecar's shape, versioned so a future reader can refuse rather
/// than misread.
#[derive(Debug, Serialize, Deserialize)]
pub struct VaultExport {
    pub version: u32,
    pub vault_id: String,
    pub vault_name: String,
    pub documents: Vec<ExportedDocument>,
    pub relationships: Vec<Relationship>,
    pub edge_rejections: Vec<RejectedRelationship>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportedDocument {
    pub document: Document,
    /// User-authored and model assignments; derived rows re-derive.
    pub tags: Vec<(TagName, DocumentTag)>,
    pub rejected_tags: Vec<(TagName, Option<String>)>,
}

#[derive(Debug, Default)]
pub struct MigrationReport {
    pub matched: usize,
    pub identities_adopted: usize,
    pub tags_applied: usize,
    pub rejections_applied: usize,
    pub relationships_applied: usize,
    /// Exported paths with no local file to attach to.
    pub missing: Vec<String>,
}

impl Marius {
    /// Write the vault's files to `dest` plus `marius-export.json`.
    pub fn export_vault(&self, vault: &Vault, dest: &Path) -> Result<std::path::PathBuf> {
        std::fs::create_dir_all(dest).map_err(|e| CoreError::Inference(e.to_string()))?;
        let filter = documents::Filter {
            vaults: vec![vault.id],
            ..Default::default()
        };
        let mut exported = Vec::new();
        for document in documents::list(self.db.conn(), &filter)? {
            // The file itself.
            let from = document.path.to_fs(&vault.root);
            let to = dest.join("files").join(document.path.as_str());
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| CoreError::Inference(e.to_string()))?;
            }
            if from.exists() {
                std::fs::copy(&from, &to).map_err(|e| CoreError::Inference(e.to_string()))?;
            }
            // Its portable metadata.
            let mut kept = Vec::new();
            for (tag, assignment) in self.document_tags(document.id)? {
                if assignment.provenance != Provenance::Derived {
                    kept.push((tag.canonical_name.clone(), assignment));
                }
            }
            let mut rejected = Vec::new();
            for rejection in tags::rejections(self.db.conn(), document.id)? {
                // Resolve the tag id back to its portable name.
                if let Some((tag, _)) = replica::get_tag(self.db.conn(), rejection.tag)? {
                    rejected.push((tag.canonical_name, rejection.reason));
                }
            }
            exported.push(ExportedDocument {
                document,
                tags: kept,
                rejected_tags: rejected,
            });
        }
        let relationships: Vec<Relationship> =
            relationships::in_vaults(self.db.conn(), &[vault.id], &[], None)?
                .into_iter()
                .filter(|r| r.provenance != Provenance::Derived)
                .collect();
        let mut edge_rejections = Vec::new();
        for entry in &exported {
            edge_rejections.extend(relationships::rejections(
                self.db.conn(),
                entry.document.id,
            )?);
        }
        edge_rejections.dedup_by(|a, b| a.endpoints == b.endpoints && a.kind == b.kind);

        let sidecar = VaultExport {
            version: 1,
            vault_id: vault.id.to_string(),
            vault_name: vault.name.clone(),
            documents: exported,
            relationships,
            edge_rejections,
        };
        let path = dest.join("marius-export.json");
        let json = serde_json::to_string_pretty(&sidecar)
            .map_err(|e| CoreError::Inference(e.to_string()))?;
        std::fs::write(&path, json).map_err(|e| CoreError::Inference(e.to_string()))?;
        Ok(path)
    }

    /// Attach an export's identities and metadata to this vault's
    /// already-indexed files, matching by path (KB-014's migration).
    pub fn import_metadata(&mut self, vault: &Vault, sidecar: &Path) -> Result<MigrationReport> {
        let json = std::fs::read_to_string(sidecar)
            .map_err(|e| CoreError::Inference(format!("cannot read {}: {e}", sidecar.display())))?;
        let export: VaultExport = serde_json::from_str(&json)
            .map_err(|e| CoreError::Inference(format!("not a Marius export: {e}")))?;
        if export.version != 1 {
            return Err(CoreError::Inference(format!(
                "export version {} is newer than this build understands",
                export.version
            )));
        }

        let mut report = MigrationReport::default();
        let tx = self.db.transaction()?;
        for entry in &export.documents {
            let Some(local) = documents::by_path(&tx, vault.id, &entry.document.path)? else {
                report.missing.push(entry.document.path.to_string());
                continue;
            };
            report.matched += 1;
            if local.id != entry.document.id {
                replica::adopt_document_id(&tx, local.id, entry.document.id)?;
                report.identities_adopted += 1;
            }
            let id = entry.document.id;
            documents::set_sensitivity(&tx, id, entry.document.sensitivity)?;
            if entry.document.authorship != marius_model::Authorship::Source {
                documents::set_authorship(&tx, id, entry.document.authorship)?;
            }
            for (name, assignment) in &entry.tags {
                let tag = tags::ensure(&tx, Some(vault.id), name)?;
                let mut assignment = assignment.clone();
                assignment.document = id;
                assignment.tag = tag.id;
                tags::assign(&tx, &assignment)?;
                report.tags_applied += 1;
            }
            for (name, reason) in &entry.rejected_tags {
                let tag = tags::ensure(&tx, Some(vault.id), name)?;
                replica::reject_tag(&tx, id, tag.id, reason.as_deref())?;
                report.rejections_applied += 1;
            }
        }
        for relationship in &export.relationships {
            // Endpoints carry the exported vault id; this vault owns them now.
            let mut edge = relationship.clone();
            edge.endpoints.source = GlobalDocumentId::new(vault.id, edge.endpoints.source.document);
            edge.endpoints.target = GlobalDocumentId::new(vault.id, edge.endpoints.target.document);
            if relationships::upsert(&tx, &edge)?.changed() {
                report.relationships_applied += 1;
            }
        }
        for rejection in &export.edge_rejections {
            let mut rejection = rejection.clone();
            rejection.endpoints.source =
                GlobalDocumentId::new(vault.id, rejection.endpoints.source.document);
            rejection.endpoints.target =
                GlobalDocumentId::new(vault.id, rejection.endpoints.target.document);
            replica::reject_relationship(&tx, &rejection)?;
        }
        marius_storage::commit(tx)?;
        Ok(report)
    }
}
