# Traceability

Every requirement ID in `knowledge_management_requirements.md`, with where it is
implemented and where it is tested.

**Status key**

| | Meaning |
|---|---|
| **done** | Implemented and covered by tests. |
| **partial** | Substantially implemented; the gap is named. |
| **ready** | The contract, data model and storage exist; the behaviour does not. |
| **open** | Not started. |

Totals of 194 requirement IDs: **103 done, 19 partial, 33 ready, 39 open**. Every ID in the specification appears below exactly once, and these totals match the rows — `crates/core/tests/traceability.rs` checks both against the specification on every `cargo test`, so this table cannot drift silently.

---

## 2. Architectural decisions

| ID | Status | Where | Notes |
|---|---|---|---|
| ARCH-001 | done | all `crates/` | Shared Rust core; `model` has no I/O. |
| ARCH-002 | open | `crates/ui-gpui` | Excluded from the workspace; no Electron anywhere. |
| ARCH-003 | open | — | iOS feasibility not yet investigated. |
| ARCH-004 | done | `LICENSE`, `NOTICE` | Apache 2.0. |
| ARCH-005 | done | `indexing::files`, `markdown` | Files authoritative; indexer is read-only. |
| ARCH-006 | done | `storage::migrations` | SQLite for metadata, relations, indexes, sync state. |
| ARCH-007 | open | `apps/sync-server` | PostgreSQL behind a service; phase 5. |
| ARCH-008 | open | — | CloudKit adapter; phase 5. `SyncBackend::CloudKit` modelled. |
| ARCH-009 | ready | `model::SyncBackend`, `changelog` | One backend per vault; journal records it. |
| ARCH-010 | done | `core::search`, `storage::relationships` | Multi-vault search and cross-vault edges. |
| ARCH-011 | done | `model::ids` | IDs independent of filename, path, vault. |
| ARCH-012 | done | `model::provenance::Authorship` | AI may change metadata, never source content. |
| ARCH-013 | partial | `model::Authorship::Derived` | Modelled, filterable, indexed; generation not built. |
| ARCH-014 | ready | `model::plugin` | Manifest, capabilities, diagnostics modelled. |
| ARCH-015 | done | `DocumentKind::Email` | Email is knowledge, not a mail client. |
| ARCH-016 | ready | `model::policy` | Types and combination rules done; engine not built. |

## 3.1 Vaults and document management

| ID | Status | Where | Test |
|---|---|---|---|
| KB-001 | done | `core::Marius::{open_vault,close_vault,rename_vault}` | `kb_001_closing_a_vault_leaves_the_users_files_alone` |
| KB-002 | done | `core::Marius::open_vault`, `storage::vaults::by_root` | `kb_002_reopening_a_directory_does_not_duplicate_it` |
| KB-003 | done | `core::Marius::{move_document,move_folder}`, `tree.rs`, drag-and-drop | Create (⌘N with folders), tree explorer, drag-to-move for notes and whole folders with three-direction link rewriting, rename, trash, restore. |
| KB-004 | done | `indexing::files`, `model::portability` | `crlf_line_endings_are_preserved`, `a_colon_is_caught_because_email_subjects_contain_them` |
| KB-005 | done | `indexing::reconcile` | `an_external_edit_is_detected` and 8 others |
| KB-006 | done | `model::ids`, `reconcile::plan` | `a_file_moved_outside_the_app_keeps_its_identity_...` |
| KB-007 | done | `indexer::build_document` | `titles_fall_back_from_frontmatter_to_heading_to_filename` |
| KB-008 | done | `storage::vaults` | `the_same_path_in_two_vaults_is_fine` |
| KB-009 | done | `core::search::vault_scope` | `ac_009_cross_vault_links_and_global_search` |
| KB-010 | done | `documents::set_aliases`, `resolve_link` | `links_resolve_through_aliases` |
| KB-011 | done | `storage::revisions` | `history_is_newest_first_and_bodies_are_recoverable` |
| KB-012 | done | `documents::set_archived`, `search::keep` | `kb_012_archived_documents_stay_searchable_unless_filtered` |
| KB-013 | partial | `Document::external`, `index_attachment` | Flag honoured and attachments indexed; no UI to add an external reference. |
| KB-014 | partial | `FRONTMATTER_ID_KEY`, `frontmatter::upsert_key` | Writeback mechanism exists; export/migration flow not built. |

## 3.2 Markdown authoring and rendering

| ID | Status | Where | Test |
|---|---|---|---|
| MD-001 | done | `buffer.rs`, `editor.rs` | Typed, saved, undone via real keystrokes; file-level assertions. |
| MD-002 | done | `markdown::{scan,text}` | `tables_checklists_and_quotes_are_flattened` |
| MD-003 | partial | `editor.rs`, `preview.rs` | Source editing and rendered reading (⌘E) done, wiki links clickable; live side-by-side preview pending. |
| MD-004 | done | `buffer` undo, autosave-on-switch, `files::write_atomic` | Word-grouped undo/redo; autosave; atomic saves + revisions. |
| MD-005 | done | `scan` images, `index_attachment` | `attachments_are_indexed_by_name_without_being_read_as_text` |
| MD-006 | done | `markdown::scan`, `documents::resolve_relative` | `wiki_link_aliases_headings_and_block_refs_are_decomposed` + 7 |
| MD-007 | done | `markdown::rewrite`, `core::relocate`, `resolve_pending_links` | `ac_003_renaming_a_linked_document_keeps_identity_and_fixes_links` |
| MD-008 | done | `markdown::frontmatter` | `unknown_keys_are_kept_and_reported`, recovery tests |
| MD-009 | done | `highlight.rs` | Same scanner as the indexer; code/fences/math excluded correctly. |
| MD-010 | partial | `scan` math masking, `text` | Parsed and indexed, not rendered. |
| MD-011 | partial | `markdown::outline` | Outline done; templates, daily notes, panes need the UI. |
| MD-012 | partial | `preview.rs` | Quotes, tables, task lists render; Obsidian callout styling pending. |
| MD-013 | open | — | Needs the UI. |
| MD-014 | open | — | P2. |

## 3.3 Import and capture

| ID | Status | Where | Test |
|---|---|---|---|
| IM-001 | done | `indexing::scan`, `indexer` | `an_existing_directory_scans_without_conversion` |
| IM-002 | done | `DocumentKind::PlainText` | `document_kinds_are_inferred_from_extensions` |
| IM-003 | done | `documents::by_hash` | `duplicate_content_is_detectable_across_vaults` |
| IM-004 | done | `Provenance::Imported`, `frontmatter::recover` | `a_pasted_email_header_block_is_recovered_rather_than_discarded` |
| IM-005 | done | `core::create_document` → `index_path` | `indexing_a_single_path_works_for_a_watcher` |
| IM-006 | done | `DocumentKind::Email`, open-in-place | `ac_011_a_pasted_email_becomes_ordinary_knowledge` |
| IM-007 | done | `scan::DEFAULT_IGNORED_DIRS`, aliases, wiki links | `ac_002_an_obsidian_style_vault_opens_without_conversion` |
| IM-008 | open | `crates/import` | PDF/HTML/Word extraction not built. |
| IM-009 | open | `crates/import` | `.eml` parsing not built; kind is modelled. |
| IM-010 | open | — | Watched folders need `notify` wiring. |
| IM-011 | open | `crates/import` | `.mbox` not built. |
| IM-012 | open | — | P2. |

## 3.4 Tags and metadata

| ID | Status | Where | Test |
|---|---|---|---|
| TAG-001 | done | `core::{add_tag,remove_tag}` | `ac_004_manual_tag_changes_are_reflected_immediately` |
| TAG-002 | done | `tags::{of_document,documents_with,counts}` | `tag_counts_include_unused_tags` |
| TAG-003 | done | `tags::documents_matching`, `search::query` | `boolean_tag_queries_combine_and_or_and_not` |
| TAG-004 | done | `scan`, `frontmatter::tags`, `replace_file_tags` | `inline_and_frontmatter_tags_are_indexed_with_their_source` |
| TAG-005 | ready | `DocumentTag::ai_generated` | Storage and provenance done; classifier not built. |
| TAG-006 | done | `provenance::is_machine_writable`, `tags::assign` | `manual_tags_are_never_replaced_by_the_classifier` |
| TAG-007 | done | `Provenance`, `tags::approve` | `approving_promotes_an_ai_tag_and_then_protects_it` |
| TAG-008 | done | `TagName::new`, `tags::ensure`, aliases | `an_alias_routes_a_proposal_to_the_existing_tag` |
| TAG-009 | done | `TagSource::Index` default for AI tags | `ac_005_ai_tags_carry_provenance_and_never_touch_the_file` |
| TAG-010 | done | `TagName` hierarchy, `tags::{rename,merge,expand}` | `hierarchical_tags_match_their_descendants_when_asked` |
| TAG-011 | done | `DocumentTag.{confidence,model,review}` | `edge_metadata_survives_a_round_trip` |
| TAG-012 | done | `ReviewState::{Locked,Rejected}`, `rejected_tag` | `a_rejected_tag_is_not_reproposed_even_by_a_different_model` |
| TAG-013 | done | `tags::replace_machine_tags` | `reclassification_preserves_user_tags_and_drops_stale_machine_tags` |
| TAG-014 | done | `model::MetadataValue` | `a_date_survives_a_round_trip_without_decaying_into_text` |
| TAG-015 | partial | `documents::Filter`, `saved_search` table | Filtering done; table views and saved queries need the UI. |
| TAG-016 | open | — | P2. |

## 3.5 AI providers, policy and generated knowledge

| ID | Status | Where | Notes |
|---|---|---|---|
| AI-001 | ready | `config::ModelChoice` | Provider, model, base URL, credential reference. |
| AI-002 | open | `crates/llm` | Adapters not built. |
| AI-003 | ready | `config::ModelSettings`, `InferenceTask` | Per-task model selection modelled. |
| AI-004 | done | everything above | `ac_017_everything_essential_works_with_no_network` |
| AI-005 | ready | `model::policy` | Types done; engine not built. |
| AI-006 | ready | `Sensitivity`, `Locality`, `VaultPolicy::permits_provider` | Selection rules expressed and tested. |
| AI-007 | done | `Sensitivity::default`, `VaultPolicy::effective` | `a_sensitivity_declared_in_frontmatter_...only_tightens` |
| AI-008 | done | `Sensitivity::across` | `strictest_level_wins_across_a_prompt` |
| AI-009 | done | `Authorship`, `Provenance` | Enforced in storage, not just intended. |
| AI-010 | partial | `VaultPolicy::auto_classify`, `changelog` | Flag and audit trail exist; classifier not built. |
| AI-011 | ready | `GeneratedNoteProvenance`, `Citation` | Table migrated; generation not built. |
| AI-012 | done | `Authorship::Derived`, `is:generated` | `ai_012_generated_notes_are_filterable` |
| AI-013 | ready | `GeneratedNoteProvenance::allows_in_place_regeneration` | Rule implemented and tested; no generator to apply it. |
| AI-014 | ready | `Authorship::Promoted` | Modelled and protected. |
| AI-015 | ready | `InferenceRecord` | Table migrated with cost and token fields. |
| AI-016 | partial | `VaultPolicy`, `config` | Per-vault rules and prompt IDs modelled. |
| AI-017 | done | `VaultPath` traversal refusal, `Capability` | `paths_refuse_to_escape_the_vault` |
| AI-018 | open | — | P2. |

## 3.6 Search and retrieval

| ID | Status | Where | Test |
|---|---|---|---|
| SRCH-001 | done | `storage::search_index`, `markdown::text` | `indexed_text_is_findable_by_word` |
| SRCH-002 | done | `search::query`, `documents::Filter` | `path_vault_and_type_filters_parse` |
| SRCH-003 | done | FTS5 + `quote_phrase` + `snippet_for` | `srch_003_and_005_query_features_and_result_identity` |
| SRCH-004 | done | `core::search::vault_scope` | `ac_009_cross_vault_links_and_global_search` |
| SRCH-005 | done | `SearchHit::{vault_name,reference}` | `srch_003_and_005_query_features_and_result_identity` |
| SRCH-006 | partial | `prefix='2 3 4'`, `saved_search` table | Prefix search done; fuzzy and saved searches not built. |
| SRCH-007 | ready | `chunks`, `EmbeddingModel` | Storage and staleness done; no embedding client. |
| SRCH-008 | done | `markdown::chunk`, `storage::chunks` | `chunks_are_produced_only_when_asked_for` |
| SRCH-009 | ready | `matching_seqs`, `cosine_similarity` | Pieces present; fusion not wired. |
| SRCH-010 | ready | `Citation`, `Chunk::citation_label` | Chunk spans quote the file exactly. |
| SRCH-011 | open | — | Needs the Q&A pipeline. |
| SRCH-012 | ready | `ClaimSupport`, `Claim::is_well_formed` | Rule implemented and tested. |
| SRCH-013 | done | `search_index::index`, `chunks::stale_chunks` | `the_search_index_is_updated_when_a_file_changes` |
| SRCH-014 | open | — | P2. |

## 3.7 Knowledge graph

| ID | Status | Where | Test |
|---|---|---|---|
| GRAPH-001 | done | `core::graph`, `ui-gpui/graph_view.rs` | Force-directed canvas: zoom, pan, drag, hover, click-to-open (⌘G). `graph.rs` tests. |
| GRAPH-002 | done | `relationships::{outgoing,incoming}` | `wiki_links_become_edges_and_backlinks` |
| GRAPH-003 | done | `shared_tag_neighbours` | `shared_tags_are_computed_and_ranked_by_overlap` |
| GRAPH-004 | done | `relationships::in_vaults` | `edges_can_be_filtered_by_kind_for_the_graph_view` |
| GRAPH-005 | done | `core::graph` focus+depth, `neighbourhood` | `local_graph_is_a_bounded_neighbourhood`; local/global toggle in the view. |
| GRAPH-006 | partial | `tags::documents_with` | Data available; bipartite view needs the UI. |
| GRAPH-007 | ready | `RelationshipKind::Semantic` | Stored, confidence-carrying, reversible. |
| GRAPH-008 | done | `GlobalDocumentId`, no FK on endpoints | `graph_queries_see_cross_vault_edges_from_either_side` |
| GRAPH-009 | done | `Relationship` | `edge_metadata_survives_a_round_trip` |
| GRAPH-010 | done | `relationships::{upsert,confirm,delete,reject}` | `confirming_an_edge_protects_it_from_re_inference` |
| GRAPH-011 | done | `rejected_relationship` | `a_rejected_edge_is_not_reproposed_in_either_direction` |
| GRAPH-012 | done | `replace_machine_edges` | `re_inference_replaces_stale_edges_but_keeps_confirmed_ones` |
| GRAPH-013 | done | `core::graph::EdgeStyle`, `graph_view.rs` | Explicit solid, inferred accent, co-tag faint; `global_graph_has_every_document_and_both_edge_sources`. |
| GRAPH-014 | done | `GraphOptions::max_nodes`, breadth caps | `the_node_ceiling_keeps_the_best_connected_and_reports_truncation`; co-tag pairs skip broad tags. |
| GRAPH-015 | partial | `Vault::available`, scoped queries | Honoured in search; graph previews need the UI. |
| GRAPH-016 | open | — | P2. |

## 3.8 Synchronisation and remote storage

| ID | Status | Where | Test |
|---|---|---|---|
| SYNC-001 | done | everything | `ac_017_everything_essential_works_with_no_network` |
| SYNC-002 | done | `Database`, device-local `vault.root` | `a_device_identity_is_created_once` |
| SYNC-003 | open | — | CloudKit adapter; phase 5. |
| SYNC-004 | open | `apps/sync-server` | Phase 5. |
| SYNC-005 | ready | `changelog`, `EntityKind` | Every entity kind journalled. |
| SYNC-006 | ready | `changelog::outstanding` | Queue exists; no transport. |
| SYNC-007 | ready | `SyncBackend`, journal `backend` column | One backend per vault. |
| SYNC-008 | ready | journal records its backend | `the_journal_records_the_backend_a_vault_syncs_to` |
| SYNC-009 | ready | `SyncOperation::conflicts_with` | `concurrent_edits_to_one_document_are_a_conflict` |
| SYNC-010 | done | `LogicalTimestamp`, tombstones, revisions | `the_clock_survives_a_reopen` + 5 |
| SYNC-011 | ready | `SyncState`, `counts_by_state`, `last_error` | `pruning_keeps_everything_still_unresolved` |
| SYNC-012 | done | no FK on edge endpoints | `cross_vault_edges_survive_their_far_vault_becoming_unavailable` |
| SYNC-013 | open | — | Needs the transport. |
| SYNC-014 | done | design constraint | SQLite is never the sync unit; the journal is. |
| SYNC-015 | open | — | P2. |

## 3.9 Plugin system and Obsidian compatibility

| ID | Status | Where | Notes |
|---|---|---|---|
| PLUG-001 | ready | `PluginInstallation` | Installs disabled with no capabilities. |
| PLUG-002 | open | `crates/plugin-compat` | Not yet created; engine choice open. |
| PLUG-003 | ready | `PluginManifest` | Reads the documented manifest format. |
| PLUG-004 | open | — | Needs the runtime. |
| PLUG-005 | ready | `Capability::{ReadVault,WriteVault}` | Mediated through the document service. |
| PLUG-006 | ready | `CompatibilityDiagnostic` | `the_compatibility_report_names_unsupported_apis` |
| PLUG-007 | done | `Capability::requires_explicit_consent` | `dangerous_capabilities_need_explicit_consent` |
| PLUG-008 | open | — | Needs the runtime sandbox. |
| PLUG-009 | ready | `desktop_only` manifest flag | Desktop prioritised. |
| PLUG-010 | open | — | Marius's own plugin interface. |
| PLUG-011 | open | — | P2. |
| PLUG-012 | open | — | P2; no Electron, by construction. |
| PLUG-013 | open | — | P2. |

## 3.10 User experience and platform support

| ID | Status | Where | Notes |
|---|---|---|---|
| UI-001 | partial | `apps/desktop`, `crates/ui-gpui` | Runs and interactively verified on macOS; Windows/Linux builds unverified. |
| UI-002 | done | `workspace.rs`, `tree.rs` | Folder-tree explorer with fold/unfold, search (⌘K), editor, tag browser, properties strip. |
| UI-003 | done | `editor::key_bindings`, `workspace` | ⌘K/⌘N/⌘S/⌘⇧L, Esc loop, arrows in list, full editor keys. |
| UI-004 | done | `theme.rs` | Both appearances, WCAG-contrast asserted by test, live toggle. |
| UI-005 | partial | `core::neighbourhood`, `marius graph` | Backlinks and related notes available; views need the UI. |
| UI-006 | open | — | Needs the UI. |
| UI-007 | open | — | Phase 7. |
| UI-008 | done | crate graph | `model` has no I/O; `core` has no UI assumption. |
| UI-009 | open | — | Needs the UI. |
| UI-010 | done | `cli::render::marker`, `provenance_note` | `marius tag list` shows `= ~ + !`. |
| UI-011 | open | — | P2. |

## 5. Security, privacy and trust

| ID | Status | Where | Test |
|---|---|---|---|
| SEC-001 | done | `config::ModelChoice::credential` | `a_saved_file_never_contains_a_credential` |
| SEC-002 | ready | `model::policy` | Rules tested; no transmission path exists yet. |
| SEC-003 | done | `VaultPath` refusal, `files::read_text` | `paths_refuse_to_escape_the_vault` |
| SEC-004 | done | everything | `ac_017_everything_essential_works_with_no_network` |
| SEC-005 | done | `VaultPolicy`, `document_policy` | `ac_016_a_local_only_policy_is_stored_...` |
| SEC-006 | ready | `inference_record`, `changelog` | Journal live; inference log awaits inference. |
| SEC-007 | open | — | Needs the transport. |
| SEC-008 | done | `backup_to`, `integrity_check`, `rebuild_derived` | `a_backup_can_be_reopened`, `nfr_007_...` |
| SEC-009 | ready | `Capability`, policy routing | Grants cannot widen a vault's policy by design. |
| SEC-010 | open | — | P2. |

## 6. Nonfunctional targets

| ID | Status | Where | Test |
|---|---|---|---|
| NFR-001 | done | `files::write_atomic` | `nfr_001_saving_is_atomic_and_leaves_no_debris` |
| NFR-002 | partial | WAL, batched transactions | Structurally supported; no UI to measure against. |
| NFR-003 | done | directory pruning, single-query reconcile | **Measured: 50 ms** to reopen a 10,000-document vault (target ~2 s). |
| NFR-004 | done | FTS5, weighted BM25, batched tag lookup | **Measured: <10 ms** across five query shapes on 10,000 documents (target ~200 ms). |
| NFR-005 | partial | schema and indexes | GPUI list measured at **100,000 rows, 87 MB** (`docs/SPIKE-GPUI.md`); the store itself is untested at that size and progressive graph rendering is not built. |
| NFR-006 | done | batching, `IndexReport::problems` | `an_unreadable_file_does_not_stop_the_pass` |
| NFR-007 | done | `rebuild_derived`, `Thoroughness::Rebuild` | `nfr_007_a_rebuild_restores_search_without_losing_metadata` |
| NFR-008 | partial | 503 tests incl. conflict detection | Compatibility regression tests await the plugin runtime. |

## 8. Acceptance scenarios

| ID | Status | Test |
|---|---|---|
| AC-001 | done | `ac_001_an_existing_directory_opens_in_place` |
| AC-002 | done | `ac_002_an_obsidian_style_vault_opens_without_conversion` |
| AC-003 | done | `ac_003_renaming_a_linked_document_keeps_identity_and_fixes_links`, `ac_003_moving_a_document_...` |
| AC-004 | done | `ac_004_manual_tag_changes_are_reflected_immediately` |
| AC-005 | partial | `ac_005_ai_tags_carry_provenance_and_never_touch_the_file` — the guarantee is tested; auto-tagging itself is not built |
| AC-006 | done | `ac_006_a_rejection_persists_across_reindexing` |
| AC-007 | open | Needs embeddings. |
| AC-008 | open | Needs grounded Q&A. |
| AC-009 | done | `ac_009_cross_vault_links_and_global_search` |
| AC-010 | ready | `a_user_edited_note_is_never_regenerated_in_place` (rule tested; no generator) |
| AC-011 | done | `ac_011_a_pasted_email_becomes_ordinary_knowledge` |
| AC-012 | ready | `concurrent_edits_to_one_document_are_a_conflict` (detection tested; no transport) |
| AC-013 | open | Needs CloudKit. |
| AC-014 | open | Needs both backends. |
| AC-015 | open | Needs the plugin runtime. |
| AC-016 | done | `ac_016_a_local_only_policy_is_stored_and_cannot_be_relaxed_automatically` |
| AC-017 | done | `ac_017_everything_essential_works_with_no_network` |

## Data integrity rules (§4.1)

| Rule | Where | Test |
|---|---|---|
| 1. Renaming preserves ID and links | `documents::relocate`, `reconcile` | `renaming_preserves_identity_and_every_reference_to_it` |
| 2. Cross-vault edges use stable IDs | `GlobalDocumentId` | `cross_vault_edges_are_recognised_and_traversable` |
| 3. Indexes rebuildable | `DERIVED_TABLES`, `rebuild_derived` | `nfr_007_a_rebuild_restores_search_without_losing_metadata` |
| 4. Manual/confirmed never overwritten | `is_machine_writable` | `reindexing_never_discards_a_manual_or_ai_tag` |
| 5. Generated notes keep sources | `GeneratedNoteProvenance` | `generated_notes_keep_their_sources` |
| 6. User edits never overwritten | `allows_in_place_regeneration`, `prune_bodies` | `pruning_never_discards_a_body_a_person_wrote` |
| 7. Rejections never reappear | `rejected_tag`, `rejected_relationship` | `a_rejection_survives_a_full_reclassification_pass` |
| 8. Access controls apply to previews | `vault_scope`, `Vault::available` | `ac_009_cross_vault_links_and_global_search` |
