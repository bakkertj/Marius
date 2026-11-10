//! `marius` — the command-line interface.
//!
//! The desktop application (UI-001) is the product; this exists because a
//! command line is the fastest way to exercise the whole stack honestly, is
//! what a script or a cron job needs, and keeps the core service layer free of any
//! assumption that a GPUI window is attached (UI-008).
//!
//! Every command runs entirely locally and needs no account (SYNC-001,
//! AC-017).

mod reference;
mod render;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use marius_core::{Marius, SearchOptions};

use crate::reference::DocumentRef;

#[derive(Parser)]
#[command(
    name = "marius",
    version,
    about = "A local-first personal knowledge manager",
    long_about = "Marius keeps your notes as ordinary Markdown files and builds \
                  an index, a tag system and a knowledge graph around them. \
                  Named for Marius Michel Pasha, who spent his life putting \
                  lighthouses where ships kept being lost."
)]
struct Cli {
    /// Where Marius keeps its own state. Defaults to the platform convention.
    #[arg(long, global = true, env = "MARIUS_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Print results as JSON, for scripting.
    #[arg(long, global = true)]
    json: bool,

    /// Increase log detail. Repeat for more.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage vaults.
    #[command(subcommand)]
    Vault(VaultCommand),

    /// Bring a vault's index up to date.
    Index {
        /// Vault name. Omit to index every vault.
        vault: Option<String>,
        /// Re-read every file regardless of its timestamps.
        #[arg(long)]
        full: bool,
    },

    /// Drop and rebuild every derived index, keeping all metadata.
    Rebuild,

    /// Watch a vault and index external changes as they happen (IM-010).
    /// Runs until interrupted.
    Watch {
        /// Vault name. Omit when exactly one vault is open.
        vault: Option<String>,
        /// Debounce window in milliseconds.
        #[arg(long, default_value_t = 400)]
        debounce: u64,
    },

    /// Search across every vault.
    Search {
        /// Query. See `marius help-query` for the syntax.
        query: Vec<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// `keyword` (default), `semantic`, or `hybrid` (SRCH-009).
        /// Semantic and hybrid need `marius embed` to have run and search
        /// one vault (name it unless exactly one is open).
        #[arg(long, default_value = "keyword")]
        mode: String,
        /// Vault for semantic/hybrid modes.
        #[arg(long)]
        vault: Option<String>,
    },

    /// Chunk and embed a vault's notes for semantic search (SRCH-007).
    Embed {
        /// Vault name. Omit when exactly one vault is open.
        vault: Option<String>,
    },

    /// Ask a question, answered only from your notes, with citations
    /// (SRCH-010, AC-008).
    Ask {
        question: Vec<String>,
        /// Vault to ask. Omit when exactly one vault is open.
        #[arg(long)]
        vault: Option<String>,
    },

    /// Explain the query language.
    HelpQuery,

    /// The inference audit log: every model request, including refusals
    /// (AI-015, SEC-006).
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Show or set which model handles which task (AI-003).
    #[command(subcommand)]
    Model(ModelCommand),

    /// Print a document.
    Show {
        /// `vault:path`, or just `path` when one vault is open.
        document: String,
        /// Show metadata instead of the body.
        #[arg(long)]
        meta: bool,
    },

    /// Create a document.
    New {
        /// `vault:path`
        document: String,
        /// Initial body. Reads standard input when omitted.
        #[arg(long)]
        body: Option<String>,
    },

    /// Rename a document, updating the links that point at it.
    Rename {
        document: String,
        /// New name, with or without an extension.
        new_name: String,
    },

    /// Move a document to another folder, updating inbound links.
    Move {
        document: String,
        /// Destination folder, or `/` for the vault root.
        folder: String,
    },

    /// Move a document to the vault trash.
    Rm { document: String },

    /// Manage a document's tags.
    #[command(subcommand)]
    Tag(TagCommand),

    /// List tags and how often they are used.
    Tags {
        vault: Option<String>,
        /// Hide tags nothing uses.
        #[arg(long)]
        used: bool,
    },

    /// Show what a document connects to.
    Graph { document: String },

    /// Check the database and report what is indexed.
    Doctor,

    /// Compact the search index and prune old journal history.
    Maintain,

    /// Copy the database to a file.
    Backup { path: PathBuf },
}

#[derive(Subcommand)]
enum VaultCommand {
    /// Open an existing directory as a vault. Nothing in it is modified.
    Add {
        name: String,
        path: PathBuf,
        /// Index it immediately.
        #[arg(long, default_value_t = true)]
        index: bool,
    },
    /// Create a new empty vault directory.
    Create { name: String, path: PathBuf },
    /// List vaults.
    List,
    /// Rename a vault. Its directory is untouched.
    Rename { name: String, new_name: String },
    /// Forget a vault. Its files are left exactly where they are.
    Remove { name: String },
    /// Mark a vault unreachable, keeping all of its metadata.
    Detach { name: String },
    /// Mark a detached vault reachable again.
    Attach { name: String },
    /// Show or set a vault's inference policy.
    Policy {
        name: String,
        /// One of: unrestricted, personal, confidential, no_inference.
        #[arg(long)]
        sensitivity: Option<String>,
        /// Allow automatic classification without review.
        #[arg(long)]
        auto_classify: Option<bool>,
    },
}

#[derive(Subcommand)]
enum TagCommand {
    /// Apply a tag by hand.
    Add { document: String, tag: String },
    /// Remove a tag without rejecting it.
    Rm { document: String, tag: String },
    /// Reject a tag, so automatic classification will not propose it again.
    Reject {
        document: String,
        tag: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Accept a proposed tag.
    Approve { document: String, tag: String },
    /// List a document's tags with their provenance.
    List { document: String },
    /// Ask the configured model to propose tags (TAG-005). The privacy
    /// policy engine rules first, and the request is audited either way.
    Suggest {
        document: String,
        /// Store accepted proposals as unreviewed machine assignments.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(clap::Subcommand)]
enum ModelCommand {
    /// Current model configuration.
    Show,
    /// Configure the model for a task: tagging, embedding, summarization
    /// or question_answering.
    Set {
        task: String,
        /// `ollama`, `openrouter`, or any OpenAI-compatible name.
        provider: String,
        model: String,
        /// Endpoint, e.g. http://127.0.0.1:11434 — required for anything
        /// but local Ollama.
        #[arg(long)]
        base_url: Option<String>,
        /// Environment variable holding the API key. The key itself is
        /// never stored (SEC-001).
        #[arg(long)]
        credential: Option<String>,
        /// local, self_hosted or remote. Inferred from the provider name
        /// when omitted.
        #[arg(long)]
        locality: Option<String>,
        /// Trust this non-local provider with Personal material (AI-006).
        #[arg(long)]
        trusted: bool,
    },
    /// Remove the model for a task.
    Clear { task: String },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match run(&cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // The whole chain, because "cannot write note.md" without the
            // underlying permission error is not actionable.
            eprintln!("marius: {error}");
            for cause in error.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("MARIUS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
}

fn run(cli: &Cli) -> Result<()> {
    let mut app = match &cli.data_dir {
        Some(dir) => Marius::open(dir).with_context(|| format!("opening {}", dir.display()))?,
        None => Marius::open_default().context("opening the default data directory")?,
    };

    match &cli.command {
        Command::Vault(command) => vault_command(&mut app, command, cli.json),
        Command::Index { vault, full } => {
            index_command(&mut app, vault.as_deref(), *full, cli.json)
        }
        Command::Rebuild => {
            let reports = app.rebuild_indexes()?;
            render::index_reports(&reports, cli.json);
            Ok(())
        }
        Command::Watch { vault, debounce } => watch_command(&mut app, vault.as_deref(), *debounce),
        Command::Search {
            query,
            limit,
            offset,
            mode,
            vault,
        } => {
            let query = query.join(" ");
            if mode != "keyword" {
                let vault = one_vault(&app, vault.as_deref())?;
                let results = match mode.as_str() {
                    "semantic" => app.semantic_search(&query, vault.id, *limit)?,
                    "hybrid" => app.hybrid_search(&query, vault.id, *limit)?,
                    other => anyhow::bail!(
                        "unknown mode {other:?}; expected keyword, semantic or hybrid"
                    ),
                };
                render::semantic_results(&query, &results, cli.json);
                return Ok(());
            }
            let results = app.search(
                &query,
                SearchOptions {
                    limit: *limit,
                    offset: *offset,
                    ..SearchOptions::default()
                },
            )?;
            render::search_results(&query, &results, cli.json);
            Ok(())
        }
        Command::Audit { limit } => {
            let rows = app.inference_log(*limit)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!("no inference has been requested");
            }
            for r in rows {
                let provider = r.provider.as_deref().unwrap_or("—");
                let model = r.model.as_deref().unwrap_or("—");
                let sent = if r.left_the_device() {
                    " [left device]"
                } else {
                    ""
                };
                println!(
                    "{}  {:<22} {:<18} {provider}/{model}{sent}\n    {}",
                    r.started_at
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                    r.task.to_string(),
                    r.outcome.to_string(),
                    r.reason
                );
            }
            Ok(())
        }
        Command::Model(command) => model_command(&mut app, command, cli.json),
        Command::Embed { vault } => {
            let vault = one_vault(&app, vault.as_deref())?;
            let report = app.embed_vault(&vault)?;
            if let Some(refusal) = &report.refusal {
                println!("nothing embedded: {refusal}");
                return Ok(());
            }
            println!(
                "{}: {} passage(s) embedded across {} document(s); {} already current{}",
                vault.name,
                report.chunks_embedded,
                report.documents,
                report.up_to_date,
                if report.skipped_policy > 0 {
                    format!("; {} skipped by policy", report.skipped_policy)
                } else {
                    String::new()
                }
            );
            Ok(())
        }
        Command::Ask { question, vault } => {
            let vault = one_vault(&app, vault.as_deref())?;
            let question = question.join(" ");
            let answer = app.ask(&question, vault.id)?;
            if let Some(refusal) = &answer.refusal {
                println!("not asked: {refusal}");
                return Ok(());
            }
            println!("{}", answer.text.trim());
            if !answer.citations.is_empty() {
                println!();
                for (index, citation) in answer.citations.iter().enumerate() {
                    println!("  [{}] {}", index + 1, citation.label);
                }
            } else if answer.unanswered {
                println!("\n(the notes do not answer this)");
            }
            Ok(())
        }
        Command::HelpQuery => {
            print!("{}", render::QUERY_HELP);
            Ok(())
        }
        Command::Show { document, meta } => {
            let (_, found) = DocumentRef::resolve(&app, document)?;
            if *meta {
                render::document_meta(&app, &found, cli.json)?;
            } else {
                print!("{}", app.read(&found)?);
            }
            Ok(())
        }
        Command::New { document, body } => {
            let (vault, path) = DocumentRef::parse_new(&app, document)?;
            let body = match body {
                Some(body) => format!("{body}\n"),
                None => read_stdin()?,
            };
            let created = app.create_document(&vault, &path, &body)?;
            render::created(&created, &vault.name, cli.json);
            Ok(())
        }
        Command::Rename { document, new_name } => {
            let (_, found) = DocumentRef::resolve(&app, document)?;
            let report = app.rename_document(found.id, new_name)?;
            render::rename_report(&report, cli.json);
            Ok(())
        }
        Command::Move { document, folder } => {
            let destination = if folder == "/" || folder.is_empty() {
                None
            } else {
                Some(
                    marius_model::VaultPath::new(folder)
                        .with_context(|| format!("invalid folder {folder:?}"))?,
                )
            };
            match DocumentRef::resolve(&app, document) {
                Ok((_, found)) => {
                    let report = app.move_document(found.id, destination.as_ref())?;
                    render::rename_report(&report, cli.json);
                    Ok(())
                }
                Err(resolve_error) => {
                    // Not a document: perhaps a folder. `marius move research
                    // archive` moves the whole subtree, links rewritten.
                    let (vault, rest) = crate::reference::split(&app, document)?;
                    let Ok(folder_path) = marius_model::VaultPath::new(rest) else {
                        return Err(resolve_error);
                    };
                    match app.move_folder(&vault, &folder_path, destination.as_ref()) {
                        Ok(report) => {
                            render::folder_move_report(&report, cli.json);
                            Ok(())
                        }
                        // The original "no such document" is usually the more
                        // useful message when neither interpretation works.
                        Err(marius_core::CoreError::InvalidName(_, reason))
                            if reason.contains("no documents") =>
                        {
                            Err(resolve_error)
                        }
                        Err(other) => Err(other.into()),
                    }
                }
            }
        }
        Command::Rm { document } => {
            let (_, found) = DocumentRef::resolve(&app, document)?;
            let trashed = app.delete_document(found.id)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "trashed": trashed.display().to_string() })
                );
            } else {
                println!("moved to {}", trashed.display());
                println!("restore it by moving it back, or run `marius index` after doing so");
            }
            Ok(())
        }
        Command::Tag(command) => tag_command(&mut app, command, cli.json),
        Command::Tags { vault, used } => {
            let scope = match vault {
                Some(name) => Some(app.vault(name)?.id),
                None => None,
            };
            let counts = app.tag_counts(scope)?;
            render::tag_counts(&counts, *used, cli.json);
            Ok(())
        }
        Command::Graph { document } => {
            let (vault, found) = DocumentRef::resolve(&app, document)?;
            let neighbourhood = app.neighbourhood(found.id)?;
            render::neighbourhood(&app, &vault, &neighbourhood, cli.json)?;
            Ok(())
        }
        Command::Doctor => {
            render::doctor(&app, cli.json)?;
            Ok(())
        }
        Command::Maintain => {
            let report = app.maintain()?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "journal_before": report.journal_before,
                        "journal_pruned": report.journal_pruned,
                        "journal_after": report.journal_after,
                        "bytes_before": report.bytes_before,
                        "bytes_after": report.bytes_after,
                    })
                );
            } else {
                println!(
                    "journal: {} -> {} rows ({} pruned)",
                    report.journal_before, report.journal_after, report.journal_pruned
                );
                println!(
                    "database: {:.1} MB -> {:.1} MB",
                    report.bytes_before as f64 / 1_048_576.0,
                    report.bytes_after as f64 / 1_048_576.0
                );
            }
            Ok(())
        }
        Command::Backup { path } => {
            app.backup(path)?;
            println!("backed up to {}", path.display());
            Ok(())
        }
    }
}

fn vault_command(app: &mut Marius, command: &VaultCommand, json: bool) -> Result<()> {
    match command {
        VaultCommand::Add { name, path, index } => {
            let vault = app.open_vault(name, path)?;
            if *index {
                let report = app.index(&vault)?;
                render::index_reports(std::slice::from_ref(&report), json);
            } else {
                render::vaults(std::slice::from_ref(&vault), json);
            }
            Ok(())
        }
        VaultCommand::Create { name, path } => {
            let vault = app.create_vault(name, path)?;
            render::vaults(std::slice::from_ref(&vault), json);
            Ok(())
        }
        VaultCommand::List => {
            let vaults = app.vaults()?;
            render::vaults(&vaults, json);
            Ok(())
        }
        VaultCommand::Rename { name, new_name } => {
            let vault = app.vault(name)?;
            let renamed = app.rename_vault(vault.id, new_name)?;
            render::vaults(std::slice::from_ref(&renamed), json);
            Ok(())
        }
        VaultCommand::Remove { name } => {
            let vault = app.vault(name)?;
            app.close_vault(vault.id)?;
            if !json {
                println!("forgot vault {:?}", vault.name);
                println!("its files are untouched at {}", vault.root.display());
            }
            Ok(())
        }
        VaultCommand::Detach { name } => {
            let vault = app.vault(name)?;
            app.set_vault_available(vault.id, false)?;
            if !json {
                println!("{} detached; its links and metadata are kept", vault.name);
            }
            Ok(())
        }
        VaultCommand::Attach { name } => {
            let vault = app.vault(name)?;
            app.set_vault_available(vault.id, true)?;
            if !json {
                println!("{} attached", vault.name);
            }
            Ok(())
        }
        VaultCommand::Policy {
            name,
            sensitivity,
            auto_classify,
        } => {
            let vault = app.vault(name)?;
            let mut policy = app.vault_policy(vault.id)?;
            let mut changed = false;
            if let Some(level) = sensitivity {
                policy.default_sensitivity = level
                    .parse()
                    .with_context(|| format!("unknown sensitivity {level:?}"))?;
                changed = true;
            }
            if let Some(auto) = auto_classify {
                policy.auto_classify = *auto;
                changed = true;
            }
            if changed {
                app.set_vault_policy(policy.clone())?;
            }
            render::policy(&vault, &policy, json);
            Ok(())
        }
    }
}

fn tag_command(app: &mut Marius, command: &TagCommand, json: bool) -> Result<()> {
    match command {
        TagCommand::Add { document, tag } => {
            let (_, found) = DocumentRef::resolve(app, document)?;
            let applied = app.add_tag(found.id, tag)?;
            if !json {
                println!("tagged {} with {}", found.path, applied.canonical_name);
            }
            Ok(())
        }
        TagCommand::Rm { document, tag } => {
            let (_, found) = DocumentRef::resolve(app, document)?;
            let removed = app.remove_tag(found.id, tag)?;
            if !json {
                if removed {
                    println!("removed {tag} from {}", found.path);
                    println!("(not rejected: automatic classification may propose it again)");
                } else {
                    println!("{} does not carry {tag}", found.path);
                }
            }
            Ok(())
        }
        TagCommand::Reject {
            document,
            tag,
            reason,
        } => {
            let (_, found) = DocumentRef::resolve(app, document)?;
            app.reject_tag(found.id, tag, reason.as_deref())?;
            if !json {
                println!("rejected {tag} for {}", found.path);
                println!("(this will not be proposed again, and survives reindexing)");
            }
            Ok(())
        }
        TagCommand::Approve { document, tag } => {
            let (_, found) = DocumentRef::resolve(app, document)?;
            if app.approve_tag(found.id, tag)? && !json {
                println!("approved {tag} on {}", found.path);
            }
            Ok(())
        }
        TagCommand::Suggest { document, apply } => {
            let (_, found) = DocumentRef::resolve(app, document)?;
            let result = app.suggest_tags(found.id, *apply)?;
            if json {
                let names: Vec<&str> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "reason": result.reason,
                        "suggestions": names,
                        "applied": result.applied.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
                        "skipped": result.skipped.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
                        "outcome": result.record.outcome.as_str(),
                    })
                );
                return Ok(());
            }
            if result.suggestions.is_empty() {
                println!("no suggestions: {}", result.reason);
                return Ok(());
            }
            for s in &result.suggestions {
                let mark = if result.applied.contains(&s.name) {
                    "+"
                } else if result.skipped.contains(&s.name) {
                    "×"
                } else {
                    "·"
                };
                println!("  {mark} {}", s.name);
            }
            if *apply {
                println!(
                    "{} applied, {} skipped (rejected or pinned); review with `marius tags approve/reject`",
                    result.applied.len(),
                    result.skipped.len()
                );
            } else {
                println!("(dry run; --apply stores them as unreviewed proposals)");
            }
            Ok(())
        }
        TagCommand::List { document } => {
            let (_, found) = DocumentRef::resolve(app, document)?;
            let assignments = app.document_tags(found.id)?;
            render::document_tags(&assignments, json);
            Ok(())
        }
    }
}

fn one_vault(app: &Marius, name: Option<&str>) -> Result<marius_model::Vault> {
    match name {
        Some(name) => Ok(app.vault(name)?),
        None => {
            let mut vaults = app.vaults()?;
            match vaults.len() {
                0 => anyhow::bail!("no vaults registered"),
                1 => Ok(vaults.remove(0)),
                _ => anyhow::bail!("several vaults are open; say which with --vault"),
            }
        }
    }
}

fn model_command(app: &mut Marius, command: &ModelCommand, json: bool) -> Result<()> {
    use marius_core::config::ModelChoice;
    fn slot<'a>(
        config: &'a mut marius_core::Config,
        task: &str,
    ) -> Result<&'a mut Option<ModelChoice>> {
        let models = &mut config.models;
        Ok(match task {
            "tagging" => &mut models.tagging,
            "embedding" => &mut models.embedding,
            "summarization" => &mut models.summarization,
            "question_answering" | "qa" => &mut models.question_answering,
            other => anyhow::bail!(
                "unknown task {other:?}; expected tagging, embedding, summarization or question_answering"
            ),
        })
    }
    match command {
        ModelCommand::Show => {
            let models = &app.config().models;
            if json {
                println!("{}", serde_json::to_string_pretty(models)?);
                return Ok(());
            }
            for (task, choice) in [
                ("tagging", &models.tagging),
                ("embedding", &models.embedding),
                ("summarization", &models.summarization),
                ("question_answering", &models.question_answering),
            ] {
                match choice {
                    Some(c) => {
                        let profile = c.profile();
                        println!(
                            "{task:<20} {}/{} ({}{})",
                            c.provider,
                            c.model,
                            profile.locality,
                            if profile.trusted_for_personal
                                && profile.locality != marius_model::Locality::Local
                            {
                                ", trusted with personal"
                            } else {
                                ""
                            }
                        );
                    }
                    None => println!("{task:<20} —"),
                }
            }
            Ok(())
        }
        ModelCommand::Set {
            task,
            provider,
            model,
            base_url,
            credential,
            locality,
            trusted,
        } => {
            let locality = locality
                .as_deref()
                .map(str::parse::<marius_model::Locality>)
                .transpose()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut config = app.config().clone();
            *slot(&mut config, task)? = Some(ModelChoice {
                provider: provider.clone(),
                model: model.clone(),
                credential: credential.clone(),
                base_url: base_url.clone(),
                locality,
                trusted_for_personal: *trusted,
            });
            app.set_config(config)?;
            println!("{task} → {provider}/{model}");
            Ok(())
        }
        ModelCommand::Clear { task } => {
            let mut config = app.config().clone();
            *slot(&mut config, task)? = None;
            app.set_config(config)?;
            println!("{task} cleared");
            Ok(())
        }
    }
}

fn watch_command(app: &mut Marius, vault: Option<&str>, debounce_ms: u64) -> Result<()> {
    let vault = match vault {
        Some(name) => app.vault(name)?,
        None => {
            let mut vaults = app.vaults()?;
            match vaults.len() {
                0 => anyhow::bail!("no vaults registered"),
                1 => vaults.remove(0),
                _ => anyhow::bail!("several vaults are open; say which: `marius watch <name>`"),
            }
        }
    };
    if !vault.available {
        anyhow::bail!(
            "vault '{}' is not available at {}",
            vault.name,
            vault.root.display()
        );
    }

    // Start from a consistent index, then follow events.
    let report = app.index(&vault)?;
    println!(
        "{}: indexed ({} added, {} modified); watching {} — Ctrl-C stops",
        vault.name,
        report.added,
        report.modified,
        vault.root.display()
    );

    let watcher = marius_indexing::watch::VaultWatcher::new(
        &vault.root,
        std::time::Duration::from_millis(debounce_ms.max(50)),
    )?;
    loop {
        let Some(changes) = watcher.changes_timeout(std::time::Duration::from_secs(1)) else {
            continue;
        };
        match app.apply_external_changes(&vault, &changes)? {
            marius_core::ExternalReport::Indexed(n) => {
                for path in &changes.paths {
                    println!("  {path}");
                }
                println!("indexed {n} change(s)");
            }
            marius_core::ExternalReport::Reindexed(report) => {
                println!(
                    "reconciled: {} added, {} modified, {} moved, {} removed",
                    report.added, report.modified, report.moved, report.removed
                );
            }
        }
    }
}

fn index_command(app: &mut Marius, vault: Option<&str>, full: bool, json: bool) -> Result<()> {
    let vaults = match vault {
        Some(name) => vec![app.vault(name)?],
        None => app.vaults()?,
    };
    if vaults.is_empty() {
        anyhow::bail!("no vaults registered; add one with `marius vault add <name> <path>`");
    }
    let mut reports = Vec::new();
    for vault in &vaults {
        if !vault.available {
            continue;
        }
        reports.push(if full {
            app.reindex(vault)?
        } else {
            app.index(vault)?
        });
    }
    render::index_reports(&reports, json);
    Ok(())
}

fn read_stdin() -> Result<String> {
    use std::io::Read;
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .context("reading the document body from standard input")?;
    Ok(buffer)
}
