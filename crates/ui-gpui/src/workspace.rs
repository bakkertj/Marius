//! The main window: sidebar, search, editor, status bar.
//!
//! Requirements: UI-001, UI-002 (document explorer, editor, search, tag
//! browser, properties), UI-003 (keyboard navigation and quick switching),
//! UI-004 (dark and light), UI-010 (provenance indicators), MD-001, MD-004
//! (autosave on switching documents), SRCH-001/003 (live search with
//! snippets).
//!
//! The window talks to `marius-core` through the same service API the CLI
//! uses — nothing here opens the database directly — so everything the UI can
//! do is scriptable, and everything scripted shows up in the UI.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use gpui::{
    actions, div, prelude::*, px, uniform_list, App, Context, Entity, FocusHandle, Focusable,
    KeyBinding, SharedString, Window,
};
use marius_core::{Marius, SearchOptions};
use marius_model::{DocumentId, Provenance, Vault, VaultPath};

use crate::editor::{Editor, EditorEvent};
use crate::graph_view::{GraphEvent, GraphView};
use crate::highlight::LinkResolver;
use crate::preview::{Preview, PreviewContext, PreviewEvent};
use crate::theme::{Appearance, Theme};
use crate::tree::{self, TreeRow};

actions!(
    workspace,
    [
        FocusSearch,
        ToggleAppearance,
        NewNote,
        OpenSelected,
        ReindexVault,
        ToggleReading,
        ToggleGraph
    ]
);

/// Workspace-level bindings; the application registers them with the
/// editor's.
pub fn key_bindings() -> Vec<KeyBinding> {
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let key = |keys: String| -> &'static str { Box::leak(keys.into_boxed_str()) };
    vec![
        // Quick switching and search are the same surface (UI-003): one box,
        // reachable from anywhere.
        KeyBinding::new(key(format!("{primary}-p")), FocusSearch, None),
        KeyBinding::new(key(format!("{primary}-k")), FocusSearch, None),
        KeyBinding::new(key(format!("{primary}-shift-f")), FocusSearch, None),
        KeyBinding::new(key(format!("{primary}-shift-l")), ToggleAppearance, None),
        KeyBinding::new(key(format!("{primary}-n")), NewNote, None),
        KeyBinding::new(key(format!("{primary}-r")), ReindexVault, None),
        // Edit <-> rendered reading, the toggle every Markdown app shares.
        KeyBinding::new(key(format!("{primary}-e")), ToggleReading, None),
        // The knowledge graph (GRAPH-001).
        KeyBinding::new(key(format!("{primary}-g")), ToggleGraph, None),
    ]
}

/// One row in the result list.
#[derive(Clone)]
struct Row {
    id: DocumentId,
    title: SharedString,
    path: SharedString,
    snippet: Option<SharedString>,
}

/// Resolves `[[wiki links]]` against the open vault, caching per document
/// version so highlighting does not query SQLite on every frame.
struct VaultResolver {
    app: Rc<RefCell<Marius>>,
    vault: marius_model::VaultId,
    cache: RefCell<HashMap<String, bool>>,
}

impl LinkResolver for VaultResolver {
    fn resolves(&self, target: &str) -> bool {
        if let Some(&hit) = self.cache.borrow().get(target) {
            return hit;
        }
        let resolved = {
            let app = self.app.borrow();
            marius_storage::documents::resolve_link(app.database().conn(), self.vault, target, &[])
                .ok()
                .flatten()
                .is_some()
        };
        self.cache.borrow_mut().insert(target.to_owned(), resolved);
        resolved
    }
}

/// Wires the reading view to the vault: wiki resolution and attachment
/// paths, both relative to the open document (MD-005, MD-006).
struct VaultPreviewContext {
    app: Rc<RefCell<Marius>>,
    vault: marius_model::VaultId,
    vault_root: std::path::PathBuf,
    /// The open document's path, for resolving relative destinations.
    base: RefCell<Option<VaultPath>>,
}

impl PreviewContext for VaultPreviewContext {
    fn wiki_resolves(&self, target: &str) -> bool {
        let app = self.app.borrow();
        marius_storage::documents::resolve_relative(
            app.database().conn(),
            self.vault,
            self.base.borrow().as_ref(),
            target,
            &[],
        )
        .ok()
        .flatten()
        .is_some()
    }

    fn attachment_path(&self, destination: &str) -> Option<std::path::PathBuf> {
        // Try relative to the open document's folder, then the vault root —
        // the same order link resolution uses.
        if let Some(base) = self.base.borrow().as_ref() {
            if let Some(folder) = base.parent() {
                if let Ok(path) = VaultPath::new(format!("{folder}/{destination}")) {
                    let absolute = path.to_fs(&self.vault_root);
                    if absolute.exists() {
                        return Some(absolute);
                    }
                }
            }
        }
        let path = VaultPath::new(destination).ok()?;
        Some(path.to_fs(&self.vault_root))
    }
}

/// `Preview` wants a `Box<dyn PreviewContext>`, the workspace wants to keep
/// updating the base path: an `Rc` shared by both, with a forwarding shim.
struct ForwardingContext(Rc<VaultPreviewContext>);

impl PreviewContext for ForwardingContext {
    fn wiki_resolves(&self, target: &str) -> bool {
        self.0.wiki_resolves(target)
    }
    fn attachment_path(&self, destination: &str) -> Option<std::path::PathBuf> {
        self.0.attachment_path(destination)
    }
}

/// A document being dragged out of the explorer (KB-003 move).
#[derive(Clone)]
struct DraggedDocument {
    id: DocumentId,
    title: SharedString,
}

/// A whole folder being dragged (KB-003): all its documents move, links
/// rewritten.
#[derive(Clone)]
struct DraggedFolder {
    path: SharedString,
}

/// The chip that follows the pointer during a drag.
struct DragGhost {
    title: SharedString,
    theme: Theme,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(4.))
            .rounded(px(6.))
            .bg(self.theme.elevated)
            .border_1()
            .border_color(self.theme.accent)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .child(self.title.clone())
    }
}

/// Which pane the main area shows (MD-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Edit,
    Reading,
    /// The knowledge graph (GRAPH-001), local or global (GRAPH-005).
    Graph,
}

/// The root view.
pub struct Workspace {
    app: Rc<RefCell<Marius>>,
    vault: Vault,
    theme: Theme,

    search: Entity<Editor>,
    editor: Entity<Editor>,
    preview: Entity<Preview>,
    preview_base: Rc<VaultPreviewContext>,
    graph: Entity<GraphView>,
    mode: ViewMode,

    rows: Vec<Row>,
    /// Explorer rows when no search is active (UI-002): the vault's real
    /// folder structure, built by `tree::rows`.
    tree: Vec<TreeRow>,
    collapsed: BTreeSet<String>,
    selected_row: usize,
    open_document: Option<DocumentId>,
    open_path: SharedString,
    open_title: SharedString,
    /// Last search text actually applied, to skip redundant queries.
    applied_query: String,
    status: SharedString,
    tag_counts: Vec<(SharedString, u64)>,
    /// The open document's tags with real provenance, for the properties
    /// strip (UI-002, UI-010). One query per open, not per frame.
    open_tags: Vec<(SharedString, Provenance)>,
    focus_handle: FocusHandle,
}

impl Workspace {
    pub fn new(
        app: Rc<RefCell<Marius>>,
        vault: Vault,
        appearance: Appearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = Theme::new(appearance);
        let resolver = Box::new(VaultResolver {
            app: app.clone(),
            vault: vault.id,
            cache: RefCell::new(HashMap::new()),
        });
        let editor = cx.new(|cx| Editor::new(cx, theme, resolver));
        let search = cx.new(|cx| Editor::single_line(cx, theme, "Search, or type a title…  (⌘K)"));
        let preview_base = Rc::new(VaultPreviewContext {
            app: app.clone(),
            vault: vault.id,
            vault_root: vault.root.clone(),
            base: RefCell::new(None),
        });
        let preview =
            cx.new(|_| Preview::new(theme, Box::new(ForwardingContext(preview_base.clone()))));
        cx.subscribe_in(&preview, window, Self::on_preview_event)
            .detach();
        let graph = cx.new(|_| GraphView::new(app.clone(), vault.id, theme));
        cx.subscribe_in(&graph, window, Self::on_graph_event)
            .detach();

        cx.subscribe_in(&search, window, Self::on_search_event)
            .detach();
        cx.subscribe_in(&editor, window, Self::on_editor_event)
            .detach();
        window.focus(&search.focus_handle(cx));

        let mut this = Self {
            app,
            vault,
            theme,
            search,
            editor,
            preview,
            preview_base,
            graph,
            mode: ViewMode::Edit,
            rows: Vec::new(),
            tree: Vec::new(),
            collapsed: BTreeSet::new(),
            selected_row: 0,
            open_document: None,
            open_path: "no document".into(),
            open_title: "Marius".into(),
            applied_query: String::from("\u{0}never"),
            status: "ready".into(),
            tag_counts: Vec::new(),
            open_tags: Vec::new(),
            focus_handle: cx.focus_handle(),
        };
        this.refresh_results(cx);
        this.refresh_tags();
        if let Some(first) = this.rows.first().map(|r| r.id) {
            this.open(first, cx);
        }
        this
    }

    /// Complete a drag: move the document, which also rewrites the links
    /// that point at it (MD-007) — the same path `marius move` takes, so the
    /// gesture and the command cannot disagree.
    fn drop_document(&mut self, id: DocumentId, folder: Option<VaultPath>, cx: &mut Context<Self>) {
        let result = self.app.borrow_mut().move_document(id, folder.as_ref());
        match result {
            Ok(report) if report.from == report.to => {
                self.status = "already there".into();
            }
            Ok(report) => {
                self.status = if report.links_rewritten > 0 {
                    format!(
                        "moved to {} · {} link(s) rewritten in {} file(s)",
                        report.to,
                        report.links_rewritten,
                        report.files_rewritten.len()
                    )
                    .into()
                } else {
                    format!("moved to {}", report.to).into()
                };
                if self.open_document == Some(id) {
                    self.open_path = report.to.to_string().into();
                }
                self.rebuild_tree(cx);
            }
            Err(error) => self.status = format!("move: {error}").into(),
        }
        cx.notify();
    }

    /// Complete a folder drag through the same service as `marius move`.
    fn drop_folder(&mut self, from: &str, new_parent: Option<VaultPath>, cx: &mut Context<Self>) {
        let Ok(from_path) = VaultPath::new(from) else {
            return;
        };
        let vault = self.vault.clone();
        let result = self
            .app
            .borrow_mut()
            .move_folder(&vault, &from_path, new_parent.as_ref());
        match result {
            Ok(report) if report.from == report.to => {
                self.status = "already there".into();
            }
            Ok(report) => {
                self.status = format!(
                    "moved {} document(s) to {}/ · {} link(s) rewritten",
                    report.documents_moved, report.to, report.links_rewritten
                )
                .into();
                // The open document may have travelled with the folder.
                if let Some(id) = self.open_document {
                    let app = self.app.borrow();
                    if let Ok(document) = app.document(id) {
                        self.open_path = document.path.to_string().into();
                        *self.preview_base.base.borrow_mut() = Some(document.path);
                    }
                }
                self.rebuild_tree(cx);
            }
            Err(error) => self.status = format!("move: {error}").into(),
        }
        cx.notify();
    }

    fn browsing(&self) -> bool {
        self.applied_query.is_empty()
    }

    fn visible_len(&self) -> usize {
        if self.browsing() {
            self.tree.len()
        } else {
            self.rows.len()
        }
    }

    // ------------------------------------------------------------- queries

    /// Re-run the list: recency with no query, real search with one.
    fn refresh_results(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().trim().to_owned();
        if query == self.applied_query {
            return;
        }
        self.applied_query = query.clone();

        let app = self.app.borrow();
        self.rows.clear();

        self.tree.clear();
        if query.is_empty() {
            // Browse mode: the explorer tree, not a flat list (UI-002).
            let filter = marius_storage::documents::Filter {
                vaults: vec![self.vault.id],
                exclude_archived: false,
                limit: Some(10_000),
                ..Default::default()
            };
            if let Ok(documents) = marius_storage::documents::list(app.database().conn(), &filter) {
                let triples: Vec<_> = documents
                    .into_iter()
                    .filter(|d| d.kind != marius_model::DocumentKind::Attachment)
                    .map(|d| (d.id, d.path.clone(), d.title.clone()))
                    .collect();
                self.tree = tree::rows(&triples, &self.collapsed);
            }
        } else {
            match app.search(
                &query,
                SearchOptions {
                    limit: 100,
                    ..Default::default()
                },
            ) {
                Ok(results) => {
                    for hit in results.hits {
                        self.rows.push(Row {
                            id: hit.document.id,
                            title: hit.document.title.clone().into(),
                            path: hit.document.path.to_string().into(),
                            snippet: hit.snippet.map(|s| s.replace('\n', " ").into()),
                        });
                    }
                    self.status = format!("{} match(es)", self.rows.len()).into();
                }
                Err(error) => {
                    // A half-typed query is often momentarily invalid; show
                    // why instead of showing nothing silently.
                    self.status = format!("search: {error}").into();
                }
            }
        }
        self.selected_row = 0;
        cx.notify();
    }

    fn refresh_open_tags(&mut self) {
        self.open_tags.clear();
        let Some(id) = self.open_document else { return };
        let app = self.app.borrow();
        if let Ok(tags) = app.document_tags(id) {
            for (tag, assignment) in tags {
                self.open_tags
                    .push((tag.canonical_name.to_string().into(), assignment.provenance));
            }
        }
    }

    /// Rebuild the explorer rows after a collapse toggle, without touching
    /// the applied query.
    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        self.applied_query = String::from("\u{0}never");
        self.refresh_results(cx);
    }

    fn refresh_tags(&mut self) {
        let app = self.app.borrow();
        self.tag_counts = app
            .tag_counts(Some(self.vault.id))
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .map(|(tag, count)| (SharedString::from(tag.canonical_name.to_string()), count))
            .take(14)
            .collect();
    }

    // -------------------------------------------------------------- opening

    /// Open a document in the editor, autosaving the previous one (MD-004).
    fn open(&mut self, id: DocumentId, cx: &mut Context<Self>) {
        if self.open_document == Some(id) {
            return;
        }
        self.autosave(cx);

        let text = {
            let app = self.app.borrow();
            app.document(id).and_then(|d| app.read(&d))
        };
        match text {
            Ok(text) => {
                self.editor
                    .update(cx, |editor, cx| editor.set_text(&text, cx));
                self.open_document = Some(id);
                {
                    let app = self.app.borrow();
                    if let Ok(document) = app.document(id) {
                        self.open_path = document.path.to_string().into();
                        self.open_title = document.title.clone().into();
                    }
                }
                if let Some(row) = self.rows.iter().position(|r| r.id == id) {
                    self.selected_row = row;
                }
                self.refresh_open_tags();
                {
                    let app = self.app.borrow();
                    if let Ok(document) = app.document(id) {
                        *self.preview_base.base.borrow_mut() = Some(document.path.clone());
                    }
                }
                if self.mode == ViewMode::Reading {
                    let text = self.editor.read(cx).text().to_owned();
                    self.preview
                        .update(cx, |preview, cx| preview.set_source(&text, cx));
                }
            }
            Err(error) => self.status = format!("open: {error}").into(),
        }
        cx.notify();
    }

    /// Save if dirty. Called on explicit save, on switch, and on blur.
    fn autosave(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.open_document else { return };
        if !self.editor.read(cx).is_dirty() {
            return;
        }
        let text = self.editor.read(cx).text().to_owned();
        let result = self.app.borrow_mut().save_document(id, &text);
        match result {
            Ok(_) => {
                self.editor.update(cx, |editor, cx| editor.mark_saved(cx));
                self.status = "saved".into();
                self.refresh_tags();
                self.refresh_open_tags();
                // Content changed, so a live query's results may have too.
                self.applied_query = String::from("\u{0}never");
                self.refresh_results(cx);
            }
            Err(error) => self.status = format!("save FAILED: {error}").into(),
        }
        cx.notify();
    }

    fn new_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The search box doubles as the name field: type a title, press ⌘N.
        let typed = self.search.read(cx).text().trim().to_owned();
        // `research/ideas/First thought` creates the folders too — which is
        // also the only way a folder comes to exist: a Markdown vault has no
        // empty directories, so a folder is born with its first note
        // (KB-003). Each segment is sanitised separately; sanitising the
        // whole string would turn the separators into hyphens.
        let name = typed
            .split('/')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(marius_model::sanitize)
            .collect::<Vec<_>>()
            .join("/");
        let name = if name.is_empty() {
            format!("Untitled {}", self.visible_len() + 1)
        } else {
            name
        };
        let Ok(path) = VaultPath::new(format!("{name}.md")) else {
            self.status = format!("cannot create {name:?}").into();
            return;
        };
        // The heading is the note's name, not its address: a note filed under
        // `research/ideas/` is still titled by its leaf.
        let title = name.rsplit('/').next().unwrap_or(&name);
        let body = format!("# {title}\n\n");
        let vault = self.vault.clone();
        let created = self.app.borrow_mut().create_document(&vault, &path, &body);
        match created {
            Ok(document) => {
                self.search.update(cx, |s, cx| s.set_text("", cx));
                self.applied_query = String::from("\u{0}never");
                self.refresh_results(cx);
                self.open(document.id, cx);
                // Creating a note means wanting to write in it.
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                self.status = format!("created {path}").into();
            }
            Err(error) => self.status = format!("create: {error}").into(),
        }
        cx.notify();
    }

    fn reindex(&mut self, cx: &mut Context<Self>) {
        self.autosave(cx);
        let vault = self.vault.clone();
        let report = self.app.borrow_mut().index(&vault);
        self.status = match report {
            Ok(report) => format!("indexed: {}", report.summary()).into(),
            Err(error) => format!("index: {error}").into(),
        };
        self.applied_query = String::from("\u{0}never");
        self.refresh_results(cx);
        self.refresh_tags();
        cx.notify();
    }

    // --------------------------------------------------------------- events

    fn on_search_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::Edited => self.refresh_results(cx),
            // Enter opens the selected result *and hands it the keyboard* —
            // search, arrive, type; no mouse in between (UI-003).
            EditorEvent::Submitted => {
                if self.browsing() {
                    match self.tree.get(self.selected_row).cloned() {
                        Some(TreeRow::Document { id, .. }) => {
                            self.open(id, cx);
                            let handle = self.editor.read(cx).focus_handle(cx);
                            window.focus(&handle);
                        }
                        Some(TreeRow::Folder { path, .. }) => {
                            tree::toggle(&mut self.collapsed, &path);
                            self.rebuild_tree(cx);
                        }
                        None => {}
                    }
                } else if let Some(row) = self.rows.get(self.selected_row).map(|r| r.id) {
                    self.open(row, cx);
                    let handle = self.editor.read(cx).focus_handle(cx);
                    window.focus(&handle);
                }
            }
            EditorEvent::NavigateDown => {
                if self.selected_row + 1 < self.visible_len() {
                    self.selected_row += 1;
                    cx.notify();
                }
            }
            EditorEvent::NavigateUp => {
                self.selected_row = self.selected_row.saturating_sub(1);
                cx.notify();
            }
            EditorEvent::Cancelled => {
                // First escape clears the query; a second, with nothing left
                // to clear, returns to the document.
                if self.search.read(cx).text().is_empty() {
                    let handle = self.editor.read(cx).focus_handle(cx);
                    window.focus(&handle);
                } else {
                    self.search.update(cx, |s, cx| s.set_text("", cx));
                    self.refresh_results(cx);
                }
            }
            _ => {}
        }
    }

    fn on_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::SaveRequested => self.autosave(cx),
            EditorEvent::Edited => cx.notify(), // dirty dot, line:col
            // Escape in the document goes back to the list (UI-003's loop:
            // ⌘K down, Escape up). The old query is selected, not kept as a
            // live cursor: typing should replace it, not append to it.
            EditorEvent::Cancelled => {
                let handle = self.search.read(cx).focus_handle(cx);
                window.focus(&handle);
                self.search
                    .update(cx, |search, cx| search.select_all_text(cx));
            }
            _ => {}
        }
    }

    fn on_toggle_reading(
        &mut self,
        _: &ToggleReading,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = match self.mode {
            ViewMode::Edit => {
                // Render from the buffer, not the disk: unsaved edits are
                // exactly what the writer wants to proof-read.
                let text = self.editor.read(cx).text().to_owned();
                self.preview
                    .update(cx, |preview, cx| preview.set_source(&text, cx));
                ViewMode::Reading
            }
            ViewMode::Reading | ViewMode::Graph => {
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                ViewMode::Edit
            }
        };
        cx.notify();
    }

    fn on_toggle_graph(&mut self, _: &ToggleGraph, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            ViewMode::Graph => {
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                ViewMode::Edit
            }
            _ => {
                let focus = self.open_document;
                self.graph.update(cx, |graph, cx| graph.show(focus, cx));
                ViewMode::Graph
            }
        };
        cx.notify();
    }

    fn on_graph_event(
        &mut self,
        _: &Entity<GraphView>,
        event: &GraphEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            GraphEvent::Open(id) => {
                // Clicking a node opens the document (GRAPH-001).
                self.mode = ViewMode::Edit;
                self.open(*id, cx);
                let handle = self.editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                cx.notify();
            }
        }
    }

    fn on_preview_event(
        &mut self,
        _: &Entity<Preview>,
        event: &PreviewEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match event {
            PreviewEvent::OpenWiki(target) | PreviewEvent::OpenRelative(target) => target.clone(),
        };
        let resolved = {
            let app = self.app.borrow();
            marius_storage::documents::resolve_relative(
                app.database().conn(),
                self.vault.id,
                self.preview_base.base.borrow().as_ref(),
                &target,
                &[],
            )
            .ok()
            .flatten()
        };
        match resolved {
            Some((found, _)) => {
                // Browsing stays in reading mode; that is what links are for.
                self.open(found.document, cx);
            }
            None => self.status = format!("{target:?} does not resolve").into(),
        }
    }

    fn on_toggle_appearance(
        &mut self,
        _: &ToggleAppearance,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.theme = Theme::new(self.theme.appearance.toggled());
        let theme = self.theme;
        self.editor
            .update(cx, |editor, cx| editor.set_theme(theme, cx));
        self.search
            .update(cx, |search, cx| search.set_theme(theme, cx));
        self.preview
            .update(cx, |preview, cx| preview.set_theme(theme, cx));
        self.graph.update(cx, |graph, _| graph.set_theme(theme));
        cx.notify();
    }

    // ------------------------------------------------------------ rendering

    /// One row of the folder tree (UI-002). A folder shows a chevron and its
    /// recursive document count; clicking it folds or unfolds. Depth becomes
    /// indentation, which is the whole of what makes it read as a tree.
    fn render_tree_row(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;
        let Some(row) = self.tree.get(ix).cloned() else {
            return div().into_any_element();
        };
        let selected = ix == self.selected_row;
        let indent = px(12. + row.depth() as f32 * 14.);
        match row {
            TreeRow::Folder {
                path,
                name,
                collapsed,
                documents,
                ..
            } => div()
                .id(ix)
                .pl(indent)
                .pr(px(12.))
                .py(px(4.))
                .cursor_pointer()
                .bg(if selected {
                    theme.selected
                } else {
                    theme.surface
                })
                .hover(|style| style.bg(theme.hover))
                .on_drag(
                    DraggedFolder {
                        path: path.clone().into(),
                    },
                    move |dragged, _position, _window, cx| {
                        let title: SharedString = format!("📁 {}", dragged.path).into();
                        cx.new(|_| DragGhost { title, theme })
                    },
                )
                .drag_over::<DraggedDocument>(move |style, _dragged, _window, _cx| {
                    style
                        .bg(theme.selected)
                        .border_1()
                        .border_color(theme.accent)
                })
                .drag_over::<DraggedFolder>(move |style, _dragged, _window, _cx| {
                    style
                        .bg(theme.selected)
                        .border_1()
                        .border_color(theme.accent)
                })
                .on_drop(cx.listener({
                    let target = path.clone();
                    move |this: &mut Self, dragged: &DraggedDocument, _window, cx| {
                        match VaultPath::new(&target) {
                            Ok(folder) => this.drop_document(dragged.id, Some(folder), cx),
                            Err(error) => {
                                this.status = format!("move: {error}").into();
                            }
                        }
                    }
                }))
                .on_drop(cx.listener({
                    let target = path.clone();
                    move |this: &mut Self, dragged: &DraggedFolder, _window, cx| {
                        match VaultPath::new(&target) {
                            Ok(parent) => this.drop_folder(dragged.path.as_ref(), Some(parent), cx),
                            Err(error) => {
                                this.status = format!("move: {error}").into();
                            }
                        }
                    }
                }))
                .flex()
                .flex_row()
                .gap(px(6.))
                .child(
                    div()
                        .text_color(theme.muted)
                        .w(px(12.))
                        .child(if collapsed { "▸" } else { "▾" }),
                )
                .child(div().text_color(theme.text).child(name))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted)
                        .child(format!("{documents}")),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this: &mut Self, _e, _w, cx| {
                        this.selected_row = ix;
                        tree::toggle(&mut this.collapsed, &path);
                        this.rebuild_tree(cx);
                    }),
                )
                .into_any_element(),
            TreeRow::Document { id, title, .. } => {
                let open = self.open_document == Some(id);
                let payload = DraggedDocument {
                    id,
                    title: title.clone().into(),
                };
                div()
                    .id(ix)
                    .pl(indent + px(18.))
                    .pr(px(12.))
                    .py(px(4.))
                    .cursor_pointer()
                    .bg(if selected || open {
                        theme.selected
                    } else {
                        theme.surface
                    })
                    .hover(|style| style.bg(theme.hover))
                    .text_color(theme.text)
                    .child(title)
                    .on_drag(payload, move |dragged, _position, _window, cx| {
                        let ghost = DragGhost {
                            title: dragged.title.clone(),
                            theme,
                        };
                        cx.new(|_| ghost)
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this: &mut Self, _e, _w, cx| {
                            this.selected_row = ix;
                            this.open(id, cx);
                        }),
                    )
                    .into_any_element()
            }
        }
    }

    /// One flat search result, with its snippet (SRCH-003).
    fn render_search_row(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let is_selected = ix == self.selected_row || self.open_document == Some(row.id);
        let mut item = div()
            .id(ix)
            .px(px(12.))
            .py(px(6.))
            .cursor_pointer()
            .child(div().text_color(theme.text).child(row.title.clone()))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(row.path.clone()),
            );
        if let Some(snippet) = &row.snippet {
            item = item.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(snippet.clone()),
            );
        }
        let id = row.id;
        item.bg(if is_selected {
            theme.selected
        } else {
            theme.surface
        })
        .hover(|style| style.bg(theme.hover))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut Self, _e, _w, cx| {
                this.open(id, cx);
            }),
        )
        .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let rows = self.rows.clone();
        let searching = !self.applied_query.is_empty() && self.applied_query != "\u{0}never";

        let count = if searching {
            rows.len()
        } else {
            self.tree.len()
        };
        let list = uniform_list(
            "documents",
            count,
            cx.processor(
                move |this: &mut Self, range: std::ops::Range<usize>, _w, cx| {
                    range
                        .map(|ix| {
                            if this.applied_query.is_empty() {
                                this.render_tree_row(ix, cx)
                            } else {
                                this.render_search_row(ix, cx)
                            }
                        })
                        .collect()
                },
            ),
        )
        .flex_grow();

        let mut tag_chips = div().flex().flex_wrap().gap(px(4.)).p(px(8.));
        for (tag, count) in &self.tag_counts {
            let tag_for_click = tag.clone();
            tag_chips = tag_chips.child(
                div()
                    .id(SharedString::from(format!("tag-{tag}")))
                    .px(px(6.))
                    .py(px(2.))
                    .text_size(px(11.))
                    .text_color(theme.tag_manual)
                    .bg(theme.elevated)
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover))
                    .child(format!("#{tag} {count}"))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this: &mut Self, _e, _w, cx| {
                            let query = format!("tag:{tag_for_click}");
                            this.search.update(cx, |s, cx| s.set_text(&query, cx));
                            this.refresh_results(cx);
                        }),
                    ),
            );
        }

        div()
            .flex()
            .flex_col()
            .w(px(300.))
            .h_full()
            .bg(theme.surface)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div().p(px(8.)).child(
                    div()
                        .h(px(32.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme.border)
                        .child(self.search.clone()),
                ),
            )
            .child(
                div()
                    .id("vault-root-drop")
                    .px(px(12.))
                    .py(px(2.))
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .drag_over::<DraggedDocument>(move |style, _dragged, _window, _cx| {
                        style.bg(theme.selected)
                    })
                    .drag_over::<DraggedFolder>(move |style, _dragged, _window, _cx| {
                        style.bg(theme.selected)
                    })
                    .on_drop(cx.listener(
                        |this: &mut Self, dragged: &DraggedDocument, _window, cx| {
                            this.drop_document(dragged.id, None, cx);
                        },
                    ))
                    .on_drop(cx.listener(
                        |this: &mut Self, dragged: &DraggedFolder, _window, cx| {
                            this.drop_folder(dragged.path.as_ref(), None, cx);
                        },
                    ))
                    .child(if searching {
                        format!("{} result(s)", rows.len())
                    } else {
                        let documents = self
                            .tree
                            .iter()
                            .filter(|row| matches!(row, TreeRow::Document { .. }))
                            .count();
                        format!("{documents} document(s) · ⌘N folder/name creates folders")
                    }),
            )
            .child(list)
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(tag_chips),
            )
    }

    /// The open document's identity and tags, colour-coded by provenance:
    /// green is yours, amber is a model's unreviewed proposal, blue is a
    /// proposal you accepted (UI-010).
    fn render_properties(&self) -> impl IntoElement {
        let theme = self.theme;
        let title = self.open_title.clone();

        let mut chips = div().flex().flex_row().flex_wrap().gap(px(4.));
        for (tag, provenance) in &self.open_tags {
            let marker = match provenance {
                Provenance::AiGenerated => "~",
                Provenance::AiConfirmed => "+",
                _ => "",
            };
            chips = chips.child(
                div()
                    .px(px(6.))
                    .py(px(1.))
                    .text_size(px(11.))
                    .rounded(px(9.))
                    .bg(theme.elevated)
                    .text_color(theme.tag_colour(*provenance))
                    .child(format!("{marker}#{tag}")),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(16.))
            .py(px(7.))
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .child(div().font_weight(gpui::FontWeight::BOLD).child(title))
            .child(chips)
    }

    fn render_status(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let (line, column) = self.editor.read(cx).cursor_line_column();
        let dirty = self.editor.read(cx).is_dirty();
        let open_path = self.open_path.clone();

        div()
            .flex()
            .flex_row()
            .justify_between()
            .px(px(12.))
            .py(px(5.))
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.))
            .text_color(theme.muted)
            .child(div().flex().flex_row().gap(px(10.)).child(format!(
                "{}{}",
                open_path,
                if dirty { "  ●" } else { "" }
            )))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(14.))
                    .child(format!("ln {}, col {}", line + 1, column + 1))
                    .child(SharedString::from(format!(
                        "{} · {} · ⌘K search · ⌘N new · ⌘S save · ⌘E {} · ⌘G graph · ⌘⇧L {}",
                        self.vault.name,
                        self.status,
                        match self.mode {
                            ViewMode::Edit => "read",
                            ViewMode::Reading | ViewMode::Graph => "edit",
                        },
                        self.theme.appearance.toggled().label(),
                    ))),
            )
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                let handle = this.search.read(cx).focus_handle(cx);
                window.focus(&handle);
                this.search.update(cx, |search, cx| {
                    search.set_text("", cx);
                });
                this.refresh_results(cx);
            }))
            .on_action(cx.listener(Self::on_toggle_appearance))
            .on_action(cx.listener(Self::on_toggle_reading))
            .on_action(cx.listener(Self::on_toggle_graph))
            .on_action(cx.listener(|this, _: &NewNote, window, cx| this.new_note(window, cx)))
            .on_action(cx.listener(|this, _: &ReindexVault, _w, cx| this.reindex(cx)))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .text_color(theme.text)
            .text_size(px(14.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow()
                    .min_h(px(0.))
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_grow()
                            .h_full()
                            .min_w(px(0.))
                            .child(self.render_properties())
                            .child(div().flex_grow().min_h(px(0.)).child(match self.mode {
                                ViewMode::Edit => self.editor.clone().into_any_element(),
                                ViewMode::Reading => self.preview.clone().into_any_element(),
                                ViewMode::Graph => self.graph.clone().into_any_element(),
                            })),
                    ),
            )
            .child(self.render_status(cx))
    }
}
