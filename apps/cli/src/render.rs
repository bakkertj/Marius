//! Printing results, as text for a person and as JSON for a script.
//!
//! Requirements: SRCH-005 (every result shows its vault and identity), UI-010
//! (provenance is visible), NFR-006 (progress and failure reports).
//!
//! Two rules throughout: a provenance marker is never omitted, because a user
//! has to be able to tell at a glance what a model decided for them; and a
//! count that was truncated says so, because a quietly short answer is worse
//! than a long one.

use anyhow::Result;
use marius_core::{Marius, Neighbourhood, RenameReport, SearchResults};
use marius_indexing::indexer::IndexReport;
use marius_model::{
    Document, DocumentTag, Provenance, Relationship, ReviewState, Tag, Vault, VaultPolicy,
};

/// Help for `marius help-query`.
pub const QUERY_HELP: &str = "\
Marius query syntax

  lighthouse harbour         both words (implicit AND)
  \"harbour mouth\"            exact phrase
  -draft    NOT draft        exclude
  history OR engineering     either
  light*                     prefix

Filters

  tag:lighthouse             carries the tag, or any tag beneath it
  tag:project/marius         hierarchical tags
  -tag:draft                 does not carry the tag
  tag:a OR tag:b             either tag
  path:research              inside that folder
  vault:work                 restrict to one vault
  type:markdown              markdown | plain_text | email | attachment | generated_note
  title:pharos               match the title only
  alias:pharos               match an alternate name
  modified:>2024-01-01       changed after a date
  created:<2024-06-30        created before a date
  modified:>week             relative: today, yesterday, week, month, year
  is:archived                only archived documents
  -is:archived               hide archived documents
  is:generated               only AI-generated notes
  -is:generated              hide AI-generated notes

Notes

  Archived and AI-generated documents are included by default; filter them out
  explicitly. A word containing punctuation is matched literally, so C++ and
  read-later work as typed. Quote text that looks like a filter: \"colour:blue\".
";

/// Vault listing.
pub fn vaults(vaults: &[Vault], json: bool) {
    if json {
        let rows: Vec<_> = vaults
            .iter()
            .map(|v| {
                serde_json::json!({
                    "id": v.id.to_string(),
                    "name": v.name,
                    "root": v.root.display().to_string(),
                    "available": v.available,
                    "sync": v.sync_backend.label(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return;
    }
    if vaults.is_empty() {
        println!("no vaults registered");
        println!("add one with: marius vault add <name> <path>");
        return;
    }
    for vault in vaults {
        let state = if vault.available { "" } else { "  [detached]" };
        println!("{}{state}", vault.name);
        println!("  {}", vault.root.display());
        println!("  sync: {}", vault.sync_backend.label());
    }
}

/// A vault's inference policy.
pub fn policy(vault: &Vault, policy: &VaultPolicy, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "vault": vault.name,
                "sensitivity": policy.default_sensitivity.as_str(),
                "allows_remote_inference": policy.default_sensitivity.allows_remote_inference(),
                "auto_classify": policy.auto_classify,
                "allowed_providers": policy.allowed_providers,
                "denied_providers": policy.denied_providers,
            })
        );
        return;
    }
    println!("{}", vault.name);
    println!("  sensitivity:   {}", policy.default_sensitivity);
    println!(
        "  remote models: {}",
        if policy.default_sensitivity.allows_remote_inference() {
            "permitted by policy"
        } else {
            "never — content stays on this device"
        }
    );
    println!(
        "  auto-classify: {}",
        if policy.auto_classify {
            "yes"
        } else {
            "no (proposals need review)"
        }
    );
    if let Some(allowed) = &policy.allowed_providers {
        println!("  providers:     only {}", allowed.join(", "));
    }
    if !policy.denied_providers.is_empty() {
        println!("  denied:        {}", policy.denied_providers.join(", "));
    }
}

/// Indexing outcome.
pub fn index_reports(reports: &[IndexReport], json: bool) {
    if json {
        let rows: Vec<_> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "added": r.added,
                    "modified": r.modified,
                    "moved": r.moved,
                    "removed": r.removed,
                    "unchanged": r.unchanged,
                    "links_completed": r.links_completed,
                    "links_unresolved": r.links_unresolved,
                    "problems": r.problems.iter()
                        .map(|(path, why)| serde_json::json!({"path": path, "problem": why}))
                        .collect::<Vec<_>>(),
                    "elapsed_ms": r.elapsed_ms,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return;
    }
    for report in reports {
        println!("{}", report.summary());
        if report.links_unresolved > 0 {
            println!(
                "  {} link(s) point at documents that do not exist yet",
                report.links_unresolved
            );
        }
        // Problems are listed, not just counted: NFR-006 asks for failure
        // reports, and a count alone is not one.
        for (path, problem) in &report.problems {
            println!("  ! {path}: {problem}");
        }
    }
}

/// Search results.
pub fn search_results(query: &str, results: &SearchResults, json: bool) {
    if json {
        let rows: Vec<_> = results
            .hits
            .iter()
            .map(|hit| {
                serde_json::json!({
                    "vault": hit.vault_name,
                    "path": hit.document.path.to_string(),
                    "id": hit.document.id.to_string(),
                    "title": hit.document.title,
                    "kind": hit.document.kind.as_str(),
                    "archived": hit.document.is_archived(),
                    "score": hit.score,
                    "snippet": hit.snippet,
                    "tags": hit.tags.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "query": query,
                "truncated": results.truncated,
                "examined": results.examined,
                "hits": rows,
            }))
            .unwrap_or_default()
        );
        return;
    }

    if results.hits.is_empty() {
        println!("no matches for {query:?}");
        println!("try fewer words, or `marius help-query` for the syntax");
        return;
    }
    for hit in &results.hits {
        let archived = if hit.document.is_archived() {
            "  [archived]"
        } else {
            ""
        };
        let generated = if hit.document.authorship == marius_model::Authorship::Derived {
            "  [AI-generated]"
        } else {
            ""
        };
        println!("{}{archived}{generated}", hit.document.title);
        println!("  {}", hit.reference());
        if let Some(snippet) = &hit.snippet {
            println!("  {}", snippet.replace('\n', " "));
        }
        if !hit.tags.is_empty() {
            let names: Vec<String> = hit.tags.iter().map(|t| format!("#{t}")).collect();
            println!("  {}", names.join(" "));
        }
        println!();
    }
    println!("{} result(s)", results.hits.len());
    if results.truncated {
        // Honesty about the candidate window, rather than a quietly short answer.
        println!(
            "more matches exist beyond the first {} candidates examined; narrow the query to see them",
            results.examined
        );
    }
}

/// A document's metadata.
pub fn document_meta(app: &Marius, document: &Document, json: bool) -> Result<()> {
    let tags = app.document_tags(document.id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": document.id.to_string(),
                "vault": document.vault.to_string(),
                "path": document.path.to_string(),
                "title": document.title,
                "kind": document.kind.as_str(),
                "authorship": document.authorship.as_str(),
                "content_hash": document.content_hash.to_string(),
                "size_bytes": document.size_bytes,
                "archived": document.is_archived(),
                "sensitivity": document.sensitivity.map(|s| s.as_str()),
                "write_protected": document.is_write_protected(),
                "tags": tags.iter().map(|(tag, a)| serde_json::json!({
                    "name": tag.canonical_name.to_string(),
                    "provenance": a.provenance.as_str(),
                    "review": a.review.as_str(),
                    "model": a.model,
                })).collect::<Vec<_>>(),
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }
    println!("{}", document.title);
    println!("  path:      {}", document.path);
    println!("  id:        {}", document.id);
    println!("  kind:      {}", document.kind);
    println!("  author:    {}", document.authorship);
    println!("  size:      {} bytes", document.size_bytes);
    println!("  hash:      {}", document.content_hash);
    if let Some(level) = document.sensitivity {
        println!("  privacy:   {level}");
    }
    if document.is_archived() {
        println!("  archived:  yes");
    }
    if !tags.is_empty() {
        println!("  tags:");
        for (tag, assignment) in &tags {
            println!("    {} {}", marker(assignment), tag.canonical_name);
        }
    }
    if !document.frontmatter.is_empty() {
        println!("  frontmatter:");
        for (key, value) in &document.frontmatter {
            println!(
                "    {key}: {} ({})",
                value.to_index_text(),
                value.type_name()
            );
        }
    }
    Ok(())
}

/// A document's tags with provenance.
pub fn document_tags(assignments: &[(Tag, DocumentTag)], json: bool) {
    if json {
        let rows: Vec<_> = assignments
            .iter()
            .map(|(tag, a)| {
                serde_json::json!({
                    "name": tag.canonical_name.to_string(),
                    "provenance": a.provenance.as_str(),
                    "review": a.review.as_str(),
                    "source": a.source.as_str(),
                    "confidence": a.confidence.map(|c| c.get()),
                    "model": a.model,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return;
    }
    if assignments.is_empty() {
        println!("no tags");
        return;
    }
    for (tag, assignment) in assignments {
        print!("{} {}", marker(assignment), tag.canonical_name);
        if let Some(confidence) = assignment.confidence {
            print!("  {confidence}");
        }
        if let Some(model) = &assignment.model {
            print!("  {model}");
        }
        println!();
    }
    println!();
    println!("  = manual or imported   ~ proposed by a model   + model proposal you approved");
    println!("  ! pinned against reclassification");
}

/// A one-character provenance marker (UI-010).
///
/// A user must be able to see at a glance which of their tags they chose and
/// which a model chose for them.
fn marker(assignment: &DocumentTag) -> &'static str {
    if assignment.review == ReviewState::Locked {
        return "!";
    }
    match assignment.provenance {
        Provenance::Manual | Provenance::Imported => "=",
        Provenance::Derived => "=",
        Provenance::AiGenerated => "~",
        Provenance::AiConfirmed => "+",
    }
}

/// Tag usage counts.
pub fn tag_counts(counts: &[(Tag, u64)], used_only: bool, json: bool) {
    let rows: Vec<&(Tag, u64)> = counts
        .iter()
        .filter(|(_, count)| !used_only || *count > 0)
        .collect();
    if json {
        let encoded: Vec<_> = rows
            .iter()
            .map(|(tag, count)| {
                serde_json::json!({
                    "name": tag.canonical_name.to_string(),
                    "display": tag.display_name,
                    "documents": count,
                    "aliases": tag.aliases.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&encoded).unwrap_or_default()
        );
        return;
    }
    if rows.is_empty() {
        println!("no tags");
        return;
    }
    let width = rows
        .iter()
        .map(|(tag, _)| tag.canonical_name.as_str().chars().count())
        .max()
        .unwrap_or(0);
    for (tag, count) in rows {
        println!("{:<width$}  {count}", tag.canonical_name.as_str());
    }
}

/// A document's neighbourhood.
pub fn neighbourhood(
    app: &Marius,
    vault: &Vault,
    neighbourhood: &Neighbourhood,
    json: bool,
) -> Result<()> {
    let name = |relationship: &Relationship, outgoing: bool| -> String {
        let far = if outgoing {
            relationship.endpoints.target
        } else {
            relationship.endpoints.source
        };
        match app.document(far.document) {
            Ok(document) => {
                if far.vault == vault.id {
                    document.path.to_string()
                } else {
                    // A cross-vault edge names its vault, and keeps meaning
                    // something even when that vault is detached (SYNC-012).
                    format!("{}:{}", far.vault, document.path)
                }
            }
            Err(_) => format!("(unavailable vault {}) {}", far.vault, far.document),
        }
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "document": neighbourhood.document.path.to_string(),
                "outgoing": neighbourhood.outgoing.iter().map(|e| serde_json::json!({
                    "target": name(e, true),
                    "kind": e.kind.as_str(),
                    "provenance": e.provenance.as_str(),
                    "confidence": e.confidence.map(|c| c.get()),
                })).collect::<Vec<_>>(),
                "incoming": neighbourhood.incoming.iter().map(|e| serde_json::json!({
                    "source": name(e, false),
                    "kind": e.kind.as_str(),
                    "provenance": e.provenance.as_str(),
                })).collect::<Vec<_>>(),
                "shared_tags": neighbourhood.shared_tags.iter().map(|k| serde_json::json!({
                    "document": k.document.document.to_string(),
                    "shared": k.shared_tags,
                })).collect::<Vec<_>>(),
                "unresolved": neighbourhood.unresolved,
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    println!("{}", neighbourhood.document.title);
    println!("  {}", neighbourhood.document.path);
    println!();

    if !neighbourhood.outgoing.is_empty() {
        println!("links to:");
        for edge in &neighbourhood.outgoing {
            println!(
                "  -> {}  [{}{}]",
                name(edge, true),
                edge.kind,
                provenance_note(edge)
            );
        }
    }
    if !neighbourhood.incoming.is_empty() {
        println!("linked from:");
        for edge in &neighbourhood.incoming {
            println!(
                "  <- {}  [{}{}]",
                name(edge, false),
                edge.kind,
                provenance_note(edge)
            );
        }
    }
    if !neighbourhood.shared_tags.is_empty() {
        println!("shares tags with:");
        for kinship in &neighbourhood.shared_tags {
            let label = app
                .document(kinship.document.document)
                .map(|d| d.path.to_string())
                .unwrap_or_else(|_| kinship.document.document.to_string());
            println!("  ~  {label}  ({} tag(s) in common)", kinship.shared_tags);
        }
    }
    if !neighbourhood.unresolved.is_empty() {
        println!("links to nothing yet:");
        for target in &neighbourhood.unresolved {
            println!("  ?  {target}");
        }
    }
    if neighbourhood.outgoing.is_empty()
        && neighbourhood.incoming.is_empty()
        && neighbourhood.shared_tags.is_empty()
        && neighbourhood.unresolved.is_empty()
    {
        println!("nothing connects to this document yet");
    }
    Ok(())
}

/// Say when an edge came from a model, and how sure it was (UI-010).
fn provenance_note(edge: &Relationship) -> String {
    if !edge.provenance.is_ai_originated() {
        return String::new();
    }
    let confidence = edge.confidence.map(|c| format!(" {c}")).unwrap_or_default();
    let reviewed = if edge.review == ReviewState::Approved {
        ", you confirmed"
    } else {
        ", unreviewed"
    };
    format!(", inferred{confidence}{reviewed}")
}

/// A created document.
pub fn created(document: &Document, vault: &str, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": document.id.to_string(),
                "vault": vault,
                "path": document.path.to_string(),
            })
        );
    } else {
        println!("created {vault}:{}", document.path);
    }
}

/// A whole-folder move.
pub fn folder_move_report(report: &marius_core::FolderMoveReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "from": report.from.to_string(),
                "to": report.to.to_string(),
                "documents_moved": report.documents_moved,
                "links_rewritten": report.links_rewritten,
                "files_rewritten": report.files_rewritten.iter()
                    .map(|p| p.to_string()).collect::<Vec<_>>(),
                "unwritable": report.unwritable.iter()
                    .map(|p| p.to_string()).collect::<Vec<_>>(),
            }))
            .unwrap_or_default()
        );
        return;
    }
    println!(
        "{}/ -> {}/  ({} document(s))",
        report.from, report.to, report.documents_moved
    );
    if report.links_rewritten > 0 {
        println!(
            "  updated {} link(s) in {} file(s)",
            report.links_rewritten,
            report.files_rewritten.len()
        );
    }
    for path in &report.unwritable {
        println!("  ! {path} references moved documents but could not be rewritten safely");
    }
}

/// A rename or move.
pub fn rename_report(report: &RenameReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "document": report.document.to_string(),
                "from": report.from.to_string(),
                "to": report.to.to_string(),
                "links_rewritten": report.links_rewritten,
                "files_rewritten": report.files_rewritten.iter()
                    .map(|p| p.to_string()).collect::<Vec<_>>(),
                "unwritable": report.unwritable.iter()
                    .map(|p| p.to_string()).collect::<Vec<_>>(),
            }))
            .unwrap_or_default()
        );
        return;
    }
    println!("{} -> {}", report.from, report.to);
    if report.links_rewritten > 0 {
        println!(
            "  updated {} link(s) in {} file(s)",
            report.links_rewritten,
            report.files_rewritten.len()
        );
        for path in &report.files_rewritten {
            println!("    {path}");
        }
    }
    // Never silent about a file that references the document but was left
    // alone: the user has to know to fix it by hand.
    for path in &report.unwritable {
        println!("  ! {path} references this document but could not be rewritten safely");
    }
}

/// Database health and index statistics.
pub fn doctor(app: &Marius, json: bool) -> Result<()> {
    let problems = app.integrity_check()?;
    let conn = app.database().conn();
    let indexed = marius_storage::search_index::count(conn)?;
    let edges = marius_storage::relationships::count(conn)?;
    let (chunks, vectors) = marius_storage::chunks::counts(conn)?;
    let pending = marius_storage::changelog::counts_by_state(conn)?;
    let vault_list = app.vaults()?;
    let unportable = app.portability_report()?;

    let mut documents = 0u64;
    for vault in &vault_list {
        documents += marius_storage::documents::count(conn, vault.id)?;
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "healthy": problems.is_empty(),
                "problems": problems,
                "device": app.database().device_id().to_string(),
                "vaults": vault_list.len(),
                "documents": documents,
                "indexed": indexed,
                "relationships": edges,
                "chunks": chunks,
                "vectors": vectors,
                "journal": pending.iter()
                    .map(|(state, n)| serde_json::json!({"state": state.as_str(), "count": n}))
                    .collect::<Vec<_>>(),
                "windows_unsafe_names": unportable.iter().map(|(doc, issues)| serde_json::json!({
                    "path": doc.path.to_string(),
                    "problems": issues.iter().map(|i| i.explain()).collect::<Vec<_>>(),
                    "suggestion": issues.first().and_then(|i| i.suggestion()),
                })).collect::<Vec<_>>(),
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    println!(
        "database:      {}",
        if problems.is_empty() {
            "ok"
        } else {
            "PROBLEMS"
        }
    );
    for problem in &problems {
        println!("  ! {problem}");
    }
    println!("device:        {}", app.database().device_id());
    println!("vaults:        {}", vault_list.len());
    println!("documents:     {documents}");
    println!("indexed:       {indexed}");
    if indexed != documents {
        // A mismatch is normal if attachments are present, but worth saying.
        println!("               (run `marius index` if this looks wrong)");
    }
    println!("relationships: {edges}");
    if chunks > 0 || vectors > 0 {
        println!("passages:      {chunks} ({vectors} with vectors)");
    }
    if !pending.is_empty() {
        let total: u64 = pending.iter().map(|(_, n)| n).sum();
        println!("change journal: {total} rows");
        for (state, count) in &pending {
            println!("  {state}: {count}");
        }
        // The journal grows by several rows per document indexed, so say when
        // housekeeping would reclaim something.
        if total > 50_000 {
            println!("  run `marius maintain` to prune acknowledged history");
        }
    }
    if !unportable.is_empty() {
        println!();
        println!("{} file(s) will not open on Windows:", unportable.len());
        for (document, issues) in &unportable {
            println!("  ! {}", document.path);
            for issue in issues {
                println!("      {}", issue.explain());
            }
            if let Some(suggestion) = issues.first().and_then(|i| i.suggestion()) {
                // The current path is quoted because it contains the very
                // characters that make it a problem, and `Re:` would otherwise
                // be read as a vault prefix.
                let stem = suggestion
                    .rsplit_once('.')
                    .map_or(suggestion.as_str(), |(stem, _)| stem);
                println!(
                    "      fix with: marius rename \"{}\" \"{stem}\"",
                    document.path
                );
            }
        }
    }

    println!();
    println!("data directory: {}", app.data_dir().display());
    Ok(())
}
