//! The document explorer's folder tree.
//!
//! Requirements: UI-002 (document explorer), KB-003 (folders as first-class
//! things to browse and create), KB-004 (readable directory structures).
//!
//! Pure data, like every other decision-bearing module in this crate: the
//! tree is built from `(id, path, title)` triples and a set of collapsed
//! folder paths, flattened to the rows the sidebar paints. GPUI never sees a
//! recursive structure, and the fold/unfold behaviour is tested without a
//! window.
//!
//! One honest limitation, inherited from what a Markdown vault *is*: a folder
//! exists because documents are in it. The index records files, so an empty
//! directory does not appear here — the way to create a folder is to create
//! a note inside it (`research/ideas/First thought` + ⌘N), exactly as with
//! git. A "new empty folder" affordance would produce ghosts that vanish on
//! the next reindex.

use std::collections::{BTreeMap, BTreeSet};

use marius_model::{DocumentId, VaultPath};

/// One paintable sidebar row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    Folder {
        /// Full vault-relative folder path — the collapse key.
        path: String,
        /// The last segment, shown in the row.
        name: String,
        depth: usize,
        collapsed: bool,
        /// Documents anywhere beneath, so a collapsed folder still says how
        /// much it holds.
        documents: usize,
    },
    Document {
        id: DocumentId,
        title: String,
        depth: usize,
    },
}

impl TreeRow {
    pub fn depth(&self) -> usize {
        match self {
            Self::Folder { depth, .. } | Self::Document { depth, .. } => *depth,
        }
    }
}

#[derive(Default)]
struct Node {
    folders: BTreeMap<String, Node>,
    /// (sort key, title, id)
    documents: Vec<(String, String, DocumentId)>,
}

impl Node {
    fn insert(&mut self, components: &[&str], title: &str, id: DocumentId) {
        match components {
            [] => {}
            [_file] => self
                .documents
                .push((title.to_lowercase(), title.to_owned(), id)),
            [folder, rest @ ..] => self
                .folders
                .entry((*folder).to_owned())
                .or_default()
                .insert(rest, title, id),
        }
    }

    fn count(&self) -> usize {
        self.documents.len() + self.folders.values().map(Node::count).sum::<usize>()
    }

    fn flatten(
        &self,
        prefix: &str,
        depth: usize,
        collapsed: &BTreeSet<String>,
        out: &mut Vec<TreeRow>,
    ) {
        // Folders first, then this level's documents — the convention every
        // file browser shares, so nothing has to be learned.
        for (name, node) in &self.folders {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let is_collapsed = collapsed.contains(&path);
            out.push(TreeRow::Folder {
                path: path.clone(),
                name: name.clone(),
                depth,
                collapsed: is_collapsed,
                documents: node.count(),
            });
            if !is_collapsed {
                node.flatten(&path, depth + 1, collapsed, out);
            }
        }
        let mut documents = self.documents.clone();
        documents.sort();
        for (_, title, id) in documents {
            out.push(TreeRow::Document { id, title, depth });
        }
    }
}

/// Build the visible rows for the explorer.
pub fn rows(
    documents: &[(DocumentId, VaultPath, String)],
    collapsed: &BTreeSet<String>,
) -> Vec<TreeRow> {
    let mut root = Node::default();
    for (id, path, title) in documents {
        let components: Vec<&str> = path.components().collect();
        root.insert(&components, title, *id);
    }
    let mut out = Vec::new();
    root.flatten("", 0, collapsed, &mut out);
    out
}

/// Toggle one folder's collapsed state.
pub fn toggle(collapsed: &mut BTreeSet<String>, path: &str) {
    if !collapsed.remove(path) {
        collapsed.insert(path.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn doc(path: &str, title: &str) -> (DocumentId, VaultPath, String) {
        (
            DocumentId::new(),
            VaultPath::new(path).unwrap(),
            title.to_owned(),
        )
    }

    fn labels(rows: &[TreeRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                TreeRow::Folder {
                    name,
                    depth,
                    collapsed,
                    documents,
                    ..
                } => format!(
                    "{}{} {}{}",
                    "  ".repeat(*depth),
                    if *collapsed { ">" } else { "v" },
                    name,
                    format!(" ({documents})"),
                ),
                TreeRow::Document { title, depth, .. } => {
                    format!("{}- {}", "  ".repeat(*depth), title)
                }
            })
            .collect()
    }

    #[test]
    fn folders_nest_and_sort_before_documents() {
        let docs = vec![
            doc("zebra.md", "Zebra"),
            doc("notes/alexandria.md", "Alexandria"),
            doc("research/deep/survey.md", "Survey"),
            doc("research/pasha.md", "Pasha"),
            doc("inbox/mail.md", "Mail"),
        ];
        let rows = rows(&docs, &BTreeSet::new());
        assert_eq!(
            labels(&rows),
            vec![
                "v inbox (1)",
                "  - Mail",
                "v notes (1)",
                "  - Alexandria",
                "v research (2)",
                "  v deep (1)",
                "    - Survey",
                "  - Pasha",
                "- Zebra",
            ]
        );
    }

    #[test]
    fn collapsing_hides_everything_beneath_but_keeps_the_count() {
        let docs = vec![
            doc("research/deep/survey.md", "Survey"),
            doc("research/pasha.md", "Pasha"),
            doc("top.md", "Top"),
        ];
        let mut collapsed = BTreeSet::new();
        toggle(&mut collapsed, "research");
        let rows = rows(&docs, &collapsed);
        assert_eq!(labels(&rows), vec!["> research (2)", "- Top"]);
    }

    #[test]
    fn collapsing_a_nested_folder_leaves_its_siblings_visible() {
        let docs = vec![
            doc("research/deep/survey.md", "Survey"),
            doc("research/pasha.md", "Pasha"),
        ];
        let mut collapsed = BTreeSet::new();
        toggle(&mut collapsed, "research/deep");
        let rows = rows(&docs, &collapsed);
        assert_eq!(
            labels(&rows),
            vec!["v research (2)", "  > deep (1)", "  - Pasha"]
        );
    }

    #[test]
    fn toggle_round_trips() {
        let mut collapsed = BTreeSet::new();
        toggle(&mut collapsed, "a");
        assert!(collapsed.contains("a"));
        toggle(&mut collapsed, "a");
        assert!(collapsed.is_empty());
    }

    #[test]
    fn a_collapsed_state_for_a_folder_that_no_longer_exists_is_harmless() {
        let mut collapsed = BTreeSet::new();
        toggle(&mut collapsed, "ghost");
        let docs = vec![doc("real/a.md", "A")];
        assert_eq!(
            labels(&rows(&docs, &collapsed)),
            vec!["v real (1)", "  - A"]
        );
    }

    #[test]
    fn documents_sort_by_title_case_insensitively() {
        let docs = vec![
            doc("b.md", "banana"),
            doc("a.md", "Apple"),
            doc("c.md", "cherry"),
        ];
        let rows = rows(&docs, &BTreeSet::new());
        assert_eq!(labels(&rows), vec!["- Apple", "- banana", "- cherry"]);
    }

    #[test]
    fn an_empty_vault_yields_no_rows() {
        assert!(rows(&[], &BTreeSet::new()).is_empty());
    }

    #[test]
    fn same_folder_names_at_different_depths_collapse_independently() {
        let docs = vec![doc("notes/a.md", "A"), doc("archive/notes/b.md", "B")];
        let mut collapsed = BTreeSet::new();
        toggle(&mut collapsed, "notes");
        let rows = rows(&docs, &collapsed);
        assert_eq!(
            labels(&rows),
            vec!["v archive (1)", "  v notes (1)", "    - B", "> notes (1)"],
            "collapsing the top-level `notes` must not touch `archive/notes`"
        );
    }
}
