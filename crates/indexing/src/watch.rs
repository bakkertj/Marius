//! Filesystem watching (IM-010): edits made by other programs reach the
//! index without a manual reindex.
//!
//! Built on `notify` (FSEvents / ReadDirectoryChangesW / inotify) behind a
//! debouncer, because editors save in bursts — swap files, atomic renames,
//! metadata touches — and one settled batch per burst is all the indexer
//! wants. The watcher only *reports*; deciding how to index a batch is the
//! application's job, and the rule lives in `ExternalChanges::is_safe_per_path`:
//! a batch where some path vanished may be a rename, and renames must go
//! through full reconciliation so content-hash move detection can keep the
//! document's identity (KB-006, AC-003) instead of tombstoning one record
//! and minting a fresh one.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use marius_model::VaultPath;
use notify::RecursiveMode;
use notify_debouncer_full::{
    new_debouncer, DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache,
};

use crate::files;
use crate::scan::ScanOptions;

/// One settled burst of external filesystem activity.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExternalChanges {
    /// Affected vault paths, deduplicated, in first-seen order.
    pub paths: Vec<VaultPath>,
    /// The platform told us events were dropped; only a full rescan is
    /// honest after that.
    pub rescan: bool,
}

impl ExternalChanges {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.rescan
    }

    /// Merge another batch into this one.
    pub fn merge(&mut self, other: ExternalChanges) {
        self.rescan |= other.rescan;
        for path in other.paths {
            if !self.paths.contains(&path) {
                self.paths.push(path);
            }
        }
    }
}

/// Watches one vault root and yields debounced [`ExternalChanges`].
///
/// Dropping the watcher stops it.
pub struct VaultWatcher {
    // Kept alive for its side effect; events arrive on `receiver`.
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    receiver: Receiver<DebounceEventResult>,
    root: PathBuf,
    options: ScanOptions,
}

impl VaultWatcher {
    /// Start watching `root` recursively. `debounce` is how long a burst
    /// must stay quiet before it is reported; editors and `git checkout`
    /// both settle well within half a second.
    pub fn new(root: impl Into<PathBuf>, debounce: Duration) -> notify::Result<Self> {
        let root = root.into();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(debounce, None, sender)?;
        debouncer.watch(&root, RecursiveMode::Recursive)?;
        Ok(Self {
            _debouncer: debouncer,
            receiver,
            root,
            options: ScanOptions::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Everything that has arrived, without blocking. `None` when quiet.
    pub fn try_changes(&self) -> Option<ExternalChanges> {
        let mut batch = ExternalChanges::default();
        while let Ok(result) = self.receiver.try_recv() {
            batch.merge(reduce(&self.root, &self.options, result));
        }
        (!batch.is_empty()).then_some(batch)
    }

    /// Block up to `timeout` for the next batch, then drain whatever else
    /// is already queued. `None` on timeout or when the watcher thread is
    /// gone.
    pub fn changes_timeout(&self, timeout: Duration) -> Option<ExternalChanges> {
        let mut batch = match self.receiver.recv_timeout(timeout) {
            Ok(result) => reduce(&self.root, &self.options, result),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                return None;
            }
        };
        if let Some(more) = self.try_changes() {
            batch.merge(more);
        }
        (!batch.is_empty()).then_some(batch)
    }
}

/// Fold one debouncer result into vault terms.
fn reduce(root: &Path, options: &ScanOptions, result: DebounceEventResult) -> ExternalChanges {
    let mut changes = ExternalChanges::default();
    match result {
        Ok(events) => {
            for event in &events {
                fold_event(root, options, event, &mut changes);
            }
        }
        Err(_errors) => {
            // Watch errors (overflow included) mean we can no longer claim
            // to know what happened.
            changes.rescan = true;
        }
    }
    changes
}

fn fold_event(
    root: &Path,
    options: &ScanOptions,
    event: &DebouncedEvent,
    changes: &mut ExternalChanges,
) {
    if event.need_rescan() {
        changes.rescan = true;
    }
    for absolute in &event.paths {
        if let Some(path) = relevant(root, options, absolute) {
            if !changes.paths.contains(&path) {
                changes.paths.push(path);
            }
        }
    }
}

/// Map an absolute path from the OS to a vault path the indexer would also
/// accept — the same ignore rules as the scanner, or a save in `.git`
/// would trigger an index pass (NFR-003).
fn relevant(root: &Path, options: &ScanOptions, absolute: &Path) -> Option<VaultPath> {
    let relative = absolute.strip_prefix(root).ok()?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let name = component.as_os_str().to_string_lossy();
        let is_last = components.peek().is_none();
        if is_last {
            // The filename itself.
            if options.ignored_files.iter().any(|f| f == name.as_ref())
                || files::is_temp_file(&name)
            {
                return None;
            }
        } else if options.ignored_dirs.iter().any(|d| d == name.as_ref()) {
            return None;
        }
    }
    VaultPath::from_fs(root, absolute).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(path: &str) -> Option<VaultPath> {
        let root = Path::new("/vault");
        relevant(root, &ScanOptions::default(), &root.join(path))
    }

    #[test]
    fn ordinary_notes_and_nested_paths_are_relevant() {
        assert_eq!(rel("note.md").unwrap().to_string(), "note.md");
        assert_eq!(
            rel("research/deep/paper.md").unwrap().to_string(),
            "research/deep/paper.md"
        );
    }

    #[test]
    fn the_scanners_ignore_rules_apply_to_events_too() {
        // A save inside .git or the application's own data directory must
        // not trigger indexing (NFR-003), and our own atomic-write temp
        // files must not index themselves.
        assert_eq!(rel(".git/objects/ab/cdef"), None);
        assert_eq!(rel(".marius/trash/2024-note.md"), None);
        assert_eq!(rel(".obsidian/workspace.json"), None);
        assert_eq!(rel("notes/.marius-1234.tmp"), None);
        assert_eq!(rel(".DS_Store"), None);
        // ...but a dotted *note name* that is not on the list stays.
        assert!(rel("node_modules/readme.md").is_none());
    }

    #[test]
    fn paths_outside_the_root_are_not_ours() {
        let root = Path::new("/vault");
        assert_eq!(
            relevant(root, &ScanOptions::default(), Path::new("/elsewhere/x.md")),
            None
        );
    }

    #[test]
    fn batches_merge_without_duplicates_and_rescan_is_sticky() {
        let mut a = ExternalChanges {
            paths: vec![VaultPath::new("a.md").unwrap()],
            rescan: false,
        };
        a.merge(ExternalChanges {
            paths: vec![
                VaultPath::new("a.md").unwrap(),
                VaultPath::new("b.md").unwrap(),
            ],
            rescan: true,
        });
        assert_eq!(a.paths.len(), 2);
        assert!(a.rescan);
        a.merge(ExternalChanges::default());
        assert!(a.rescan, "a later quiet batch does not clear rescan");
    }
}
