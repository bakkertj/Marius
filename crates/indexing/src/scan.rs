//! Walking a vault directory.
//!
//! Requirements: KB-002 (open an existing directory of Markdown files without
//! conversion), KB-004, KB-005, IM-007 (import an Obsidian vault as-is),
//! NFR-003 (a typical indexed vault opens in about two seconds).

use std::path::{Path, PathBuf};

use marius_model::{DocumentKind, VaultPath};
use time::OffsetDateTime;
use walkdir::WalkDir;

use crate::files;

/// Directories skipped by default.
///
/// These hold another tool's state, not the user's knowledge. `.obsidian` is
/// skipped for *indexing* while remaining readable by the importer — IM-007
/// asks for an Obsidian vault to be opened in place, and its config directory
/// is not a set of notes.
pub const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".marius",
    ".obsidian",
    ".trash",
    ".stfolder",
    ".stversions",
    "node_modules",
    "__pycache__",
    ".venv",
    ".idea",
    ".vscode",
];

/// Files skipped by default.
pub const DEFAULT_IGNORED_FILES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini", ".gitignore"];

/// What to walk and what to skip.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub ignored_dirs: Vec<String>,
    pub ignored_files: Vec<String>,
    /// Index non-text files by name and metadata (MD-005, KB-013). On by
    /// default so `![[diagram.png]]` resolves to something.
    pub include_attachments: bool,
    /// Following symlinks can leave the vault or loop; off by default.
    pub follow_symlinks: bool,
    pub max_depth: Option<usize>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            ignored_dirs: DEFAULT_IGNORED_DIRS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ignored_files: DEFAULT_IGNORED_FILES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            include_attachments: true,
            follow_symlinks: false,
            max_depth: None,
        }
    }
}

impl ScanOptions {
    /// Markdown and text only — the fast path for a first index.
    pub fn text_only() -> Self {
        Self {
            include_attachments: false,
            ..Self::default()
        }
    }
}

/// A file found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: VaultPath,
    pub absolute: PathBuf,
    pub size_bytes: u64,
    /// Filesystem modification time. A *hint* only: it is cheap to compare but
    /// unreliable across clock changes, restores and sync tools, so a
    /// difference triggers a hash rather than being trusted (KB-005).
    pub modified: OffsetDateTime,
    pub kind: DocumentKind,
}

/// What a scan found, plus what it could not read.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    /// Paths that could not be read, with why. Reported rather than silently
    /// omitted: a permission error that quietly removed notes from the index
    /// would look like data loss (NFR-006).
    pub problems: Vec<(PathBuf, String)>,
}

impl ScanResult {
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Walk `root`, returning every indexable file.
pub fn scan(root: &Path, options: &ScanOptions) -> ScanResult {
    let mut result = ScanResult::default();
    let mut walker = WalkDir::new(root).follow_links(options.follow_symlinks);
    if let Some(depth) = options.max_depth {
        walker = walker.max_depth(depth);
    }

    for entry in walker.into_iter().filter_entry(|e| {
        // Pruning at the directory level is what keeps a vault containing a
        // `node_modules` from taking minutes to open (NFR-003).
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        if e.file_type().is_dir() {
            !options.ignored_dirs.iter().any(|d| d == name.as_ref())
        } else {
            true
        }
    }) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let path = error.path().unwrap_or(root).to_path_buf();
                result.problems.push((path, error.to_string()));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if options.ignored_files.contains(&name) || files::is_temp_file(&name) {
            continue;
        }

        let path = match VaultPath::from_fs(root, entry.path()) {
            Ok(path) => path,
            Err(error) => {
                result
                    .problems
                    .push((entry.path().to_path_buf(), error.to_string()));
                continue;
            }
        };
        let kind = DocumentKind::from_extension(path.extension().as_deref());
        if !options.include_attachments && kind == DocumentKind::Attachment {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                result
                    .problems
                    .push((entry.path().to_path_buf(), error.to_string()));
                continue;
            }
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| OffsetDateTime::from(t).checked_to_offset(time::UtcOffset::UTC))
            .unwrap_or_else(OffsetDateTime::now_utc);

        result.files.push(ScannedFile {
            path,
            absolute: entry.path().to_path_buf(),
            size_bytes: metadata.len(),
            modified,
            kind,
        });
    }

    // Deterministic order makes indexing reproducible and progress monotonic.
    result.files.sort_by(|a, b| a.path.cmp(&b.path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn vault() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("notes/daily")).unwrap();
        fs::create_dir_all(root.join(".obsidian/plugins/dataview")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join("attachments")).unwrap();

        fs::write(root.join("index.md"), "# Index\n").unwrap();
        fs::write(root.join("notes/alexandria.md"), "# Alexandria\n").unwrap();
        fs::write(root.join("notes/daily/2024-03-05.md"), "today\n").unwrap();
        fs::write(root.join("notes/plain.txt"), "plain\n").unwrap();
        fs::write(root.join("attachments/diagram.png"), [0x89, 0x50]).unwrap();
        fs::write(root.join(".obsidian/plugins/dataview/main.js"), "//").unwrap();
        fs::write(root.join(".git/objects/abc"), "x").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "//").unwrap();
        fs::write(root.join(".DS_Store"), "x").unwrap();
        dir
    }

    fn paths(result: &ScanResult) -> Vec<String> {
        result.files.iter().map(|f| f.path.to_string()).collect()
    }

    #[test]
    fn an_existing_directory_scans_without_conversion() {
        // KB-002.
        let dir = vault();
        let result = scan(dir.path(), &ScanOptions::default());
        assert_eq!(
            paths(&result),
            vec![
                "attachments/diagram.png",
                "index.md",
                "notes/alexandria.md",
                "notes/daily/2024-03-05.md",
                "notes/plain.txt",
            ]
        );
        assert!(result.problems.is_empty());
    }

    #[test]
    fn tool_directories_are_skipped() {
        let dir = vault();
        let result = scan(dir.path(), &ScanOptions::default());
        let listed = paths(&result).join(" ");
        for skipped in [".obsidian", ".git", "node_modules", ".DS_Store"] {
            assert!(
                !listed.contains(skipped),
                "{skipped} should be skipped: {listed}"
            );
        }
    }

    #[test]
    fn document_kinds_are_inferred_from_extensions() {
        let dir = vault();
        let result = scan(dir.path(), &ScanOptions::default());
        let kinds: Vec<(String, DocumentKind)> = result
            .files
            .iter()
            .map(|f| (f.path.to_string(), f.kind))
            .collect();
        assert!(kinds.contains(&("index.md".into(), DocumentKind::Markdown)));
        assert!(kinds.contains(&("notes/plain.txt".into(), DocumentKind::PlainText)));
        assert!(kinds.contains(&("attachments/diagram.png".into(), DocumentKind::Attachment)));
    }

    #[test]
    fn attachments_can_be_excluded_for_a_fast_text_only_index() {
        let dir = vault();
        let result = scan(dir.path(), &ScanOptions::text_only());
        assert!(!paths(&result).iter().any(|p| p.ends_with(".png")));
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn marius_temporary_files_are_never_indexed() {
        let dir = vault();
        fs::write(
            dir.path()
                .join(".note.md.marius-0192f0c4-1234-7000-8000-000000000001.tmp"),
            "half written",
        )
        .unwrap();
        let result = scan(dir.path(), &ScanOptions::default());
        assert!(!paths(&result).iter().any(|p| p.contains("marius-")));
    }

    #[test]
    fn results_are_in_a_stable_order() {
        // Reproducible indexing and monotonic progress reporting.
        let dir = vault();
        let first = paths(&scan(dir.path(), &ScanOptions::default()));
        let second = paths(&scan(dir.path(), &ScanOptions::default()));
        assert_eq!(first, second);
        assert!(first.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn metadata_is_captured_for_change_detection() {
        // KB-005.
        let dir = vault();
        let result = scan(dir.path(), &ScanOptions::default());
        let index = result
            .files
            .iter()
            .find(|f| f.path.as_str() == "index.md")
            .unwrap();
        assert_eq!(index.size_bytes, "# Index\n".len() as u64);
        assert!(index.modified.year() >= 2024);
        assert!(index.absolute.ends_with("index.md"));
    }

    #[test]
    fn an_empty_directory_scans_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let result = scan(dir.path(), &ScanOptions::default());
        assert!(result.is_empty());
        assert!(result.problems.is_empty());
    }

    #[test]
    fn a_missing_root_is_reported_as_a_problem_not_a_panic() {
        let result = scan(Path::new("/definitely/not/here"), &ScanOptions::default());
        assert!(result.is_empty());
        assert_eq!(result.problems.len(), 1);
    }

    #[test]
    fn depth_can_be_limited() {
        let dir = vault();
        let result = scan(
            dir.path(),
            &ScanOptions {
                max_depth: Some(1),
                ..ScanOptions::default()
            },
        );
        assert_eq!(paths(&result), vec!["index.md"]);
    }

    #[test]
    fn extra_ignores_are_honoured() {
        let dir = vault();
        let result = scan(
            dir.path(),
            &ScanOptions {
                ignored_dirs: vec!["notes".to_owned()],
                ..ScanOptions::default()
            },
        );
        assert!(!paths(&result).iter().any(|p| p.starts_with("notes/")));
    }

    #[test]
    fn symlinks_are_not_followed_by_default() {
        // Following them can leave the vault or loop forever.
        let dir = vault();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), "elsewhere").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
            let result = scan(dir.path(), &ScanOptions::default());
            assert!(!paths(&result).iter().any(|p| p.contains("secret")));
        }
    }
}
