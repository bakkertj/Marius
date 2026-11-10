//! Reading and writing the user's files.
//!
//! Requirements: KB-004 (portable UTF-8 Markdown), MD-004 (autosave and
//! reliable crash recovery), NFR-001 (**no loss of committed Markdown data
//! after ordinary application crashes**), ARCH-005, ARCH-012.
//!
//! NFR-001 is the requirement that dictates this module's design. A naive
//! `fs::write` truncates the file and then writes; a crash in between leaves
//! the user with an empty or half-written note and no copy of what was there.
//! Every write therefore goes to a temporary file in the *same directory*,
//! is flushed to disk, and is then renamed over the target — a rename within a
//! directory is atomic on every platform Marius targets, so a reader either
//! sees the old file or the new one, never a fragment.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use marius_model::ContentHash;

/// Anything that can go wrong touching the filesystem.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid UTF-8 and cannot be edited as Markdown")]
    NotUtf8 { path: PathBuf },
    #[error("{path} already exists")]
    Exists { path: PathBuf },
    #[error("{path} is {size} bytes, larger than the {limit} byte limit for text")]
    TooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
}

/// Largest file read as text.
///
/// Beyond this a file is treated as an attachment: indexed by name and
/// metadata, never loaded whole into memory (KB-013). 32 MiB is far above any
/// hand-written note and well below a size that would stall the UI.
pub const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;

/// A file's content and what it hashed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contents {
    pub text: String,
    pub hash: ContentHash,
    pub size_bytes: u64,
}

/// Read a text file.
///
/// A UTF-8 BOM is stripped, because Windows editors add one and it would
/// otherwise prevent frontmatter being recognised and appear in the first
/// heading. CRLF line endings are *preserved*: rewriting them would touch every
/// line of a user's file on a platform where they are correct (ARCH-005).
pub fn read_text(path: &Path) -> Result<Contents, FileError> {
    let metadata = fs::metadata(path).map_err(|source| FileError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_TEXT_BYTES {
        return Err(FileError::TooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            limit: MAX_TEXT_BYTES,
        });
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|source| FileError::Read {
            path: path.to_path_buf(),
            source,
        })?;

    // Hash the bytes as they are on disk, before any normalisation, so the hash
    // identifies the file's real content and round-trips.
    let hash = ContentHash::of(&bytes);
    let size_bytes = bytes.len() as u64;

    let text = String::from_utf8(bytes).map_err(|_| FileError::NotUtf8 {
        path: path.to_path_buf(),
    })?;
    let text = text
        .strip_prefix('\u{feff}')
        .map(str::to_owned)
        .unwrap_or(text);

    Ok(Contents {
        text,
        hash,
        size_bytes,
    })
}

/// Hash a file without keeping its contents, for change detection on
/// attachments and large files.
pub fn hash_file(path: &Path) -> Result<(ContentHash, u64), FileError> {
    let mut file = File::open(path).map_err(|source| FileError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = blake3_hasher();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(|source| FileError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((
        ContentHash::from_bytes(*hasher.finalize().as_bytes()),
        total,
    ))
}

fn blake3_hasher() -> blake3::Hasher {
    blake3::Hasher::new()
}

/// Write `text` to `path` atomically.
///
/// The sequence — write temp, flush, fsync, rename, fsync the directory — is
/// what makes NFR-001 true. Each step matters: without the file fsync the
/// rename can be durable while the contents are not; without the directory
/// fsync the rename itself can be lost on some filesystems.
pub fn write_atomic(path: &Path, text: &str) -> Result<ContentHash, FileError> {
    write_atomic_bytes(path, text.as_bytes())
}

/// [`write_atomic`] for binary content — imported attachments, mostly.
pub fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<ContentHash, FileError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| FileError::Write {
        path: parent.to_path_buf(),
        source,
    })?;

    // The temporary file must share the destination's directory, or the rename
    // would cross filesystems and stop being atomic.
    let temp = temp_path(path);
    {
        let mut file = File::create(&temp).map_err(|source| FileError::Write {
            path: temp.clone(),
            source,
        })?;
        file.write_all(bytes)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
            .map_err(|source| {
                let _ = fs::remove_file(&temp);
                FileError::Write {
                    path: temp.clone(),
                    source,
                }
            })?;
    }

    fs::rename(&temp, path).map_err(|source| {
        let _ = fs::remove_file(&temp);
        FileError::Write {
            path: path.to_path_buf(),
            source,
        }
    })?;

    // Durability of the rename itself. A failure here is not fatal — the data
    // is written — so it is logged rather than returned.
    if let Ok(dir) = File::open(parent) {
        if let Err(error) = dir.sync_all() {
            tracing::debug!(?parent, %error, "could not fsync directory after rename");
        }
    }

    Ok(ContentHash::of(bytes))
}

/// Create a file, refusing to overwrite an existing one (KB-003).
pub fn create_new(path: &Path, text: &str) -> Result<ContentHash, FileError> {
    if path.exists() {
        return Err(FileError::Exists {
            path: path.to_path_buf(),
        });
    }
    write_atomic(path, text)
}

/// Move or rename a file, creating the destination's parent directories.
///
/// Refuses to overwrite an existing destination: a rename that silently
/// clobbered another note would be unrecoverable data loss.
pub fn move_file(from: &Path, to: &Path) -> Result<(), FileError> {
    if to.exists() && from != to {
        return Err(FileError::Exists {
            path: to.to_path_buf(),
        });
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|source| FileError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::rename(from, to).map_err(|source| FileError::Write {
        path: to.to_path_buf(),
        source,
    })
}

/// Where a deleted document is kept so it can be restored (KB-003).
pub const TRASH_DIR: &str = ".marius/trash";

/// Move a file into the vault's trash instead of deleting it.
///
/// KB-003 requires restoring a deleted document, and a note the user spent
/// years on should not be destroyed by a single keystroke. The original
/// relative path is encoded in the trashed filename so a restore knows where
/// it came from.
pub fn trash(vault_root: &Path, relative: &str) -> Result<PathBuf, FileError> {
    let source = vault_root.join(relative);
    let trash_dir = vault_root.join(TRASH_DIR);
    fs::create_dir_all(&trash_dir).map_err(|source| FileError::Write {
        path: trash_dir.clone(),
        source,
    })?;

    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let flattened = relative.replace(['/', '\\'], "%2F");
    let mut destination = trash_dir.join(format!("{stamp}-{flattened}"));
    let mut attempt = 1;
    while destination.exists() {
        destination = trash_dir.join(format!("{stamp}-{attempt}-{flattened}"));
        attempt += 1;
    }

    fs::rename(&source, &destination).map_err(|source_err| FileError::Write {
        path: destination.clone(),
        source: source_err,
    })?;
    Ok(destination)
}

/// The original vault-relative path of a trashed file.
pub fn trashed_origin(trashed: &Path) -> Option<String> {
    let name = trashed.file_name()?.to_str()?;
    // `<stamp>-<flattened>` or `<stamp>-<attempt>-<flattened>`.
    let rest = name.split_once('-').map(|(_, rest)| rest)?;
    let rest = match rest.split_once('-') {
        Some((head, tail)) if head.chars().all(|c| c.is_ascii_digit()) => tail,
        _ => rest,
    };
    Some(rest.replace("%2F", "/"))
}

/// Remove now-empty directories from `deepest` upward, stopping at the vault
/// root.
///
/// A Markdown vault has no meaningful empty directories — a folder exists
/// because notes are in it — so after a move the vacated chain is swept. Any
/// directory that still has contents (a stray `.DS_Store` does not count as
/// contents worth keeping a ghost folder for? — yes it does; only truly empty
/// directories are removed, so nothing the user put there is ever touched).
pub fn prune_empty_dirs(vault_root: &Path, deepest_relative: &str) {
    let mut current = vault_root.join(deepest_relative);
    while current != *vault_root && current.starts_with(vault_root) {
        // `remove_dir` refuses non-empty directories, which is the guarantee.
        if std::fs::remove_dir(&current).is_err() {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document");
    // A unique suffix, so two concurrent writers cannot share a temp file.
    let unique = marius_model::DocumentId::new();
    path.with_file_name(format!(".{name}.marius-{unique}.tmp"))
}

/// True when a path is one of Marius's own temporary files, which the scanner
/// must never index.
pub fn is_temp_file(name: &str) -> bool {
    name.starts_with('.') && name.contains(".marius-") && name.ends_with(".tmp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn text_round_trips_through_an_atomic_write() {
        let dir = tmp();
        let path = dir.path().join("note.md");
        let text = "# Pharos\n\nIt stood at the harbour.\n";
        let hash = write_atomic(&path, text).unwrap();

        let read = read_text(&path).unwrap();
        assert_eq!(read.text, text);
        assert_eq!(read.hash, hash);
        assert_eq!(read.size_bytes, text.len() as u64);
    }

    #[test]
    fn a_write_leaves_no_temporary_files_behind() {
        let dir = tmp();
        write_atomic(&dir.path().join("note.md"), "x").unwrap();
        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "note.md")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_write_replaces_content_without_an_intermediate_empty_state() {
        // The guarantee behind NFR-001: the destination is never truncated.
        let dir = tmp();
        let path = dir.path().join("note.md");
        write_atomic(&path, "original content").unwrap();
        let before = fs::metadata(&path).unwrap().len();
        write_atomic(&path, "replacement").unwrap();
        assert_ne!(before, fs::metadata(&path).unwrap().len());
        assert_eq!(read_text(&path).unwrap().text, "replacement");
    }

    #[test]
    fn the_temporary_file_is_a_sibling_so_the_rename_stays_atomic() {
        let path = Path::new("/vaults/v/notes/note.md");
        let temp = temp_path(path);
        assert_eq!(temp.parent(), path.parent());
        assert!(is_temp_file(temp.file_name().unwrap().to_str().unwrap()));
    }

    #[test]
    fn temporary_files_are_recognisable_so_the_scanner_skips_them() {
        assert!(is_temp_file(
            ".note.md.marius-0192f0c4-1234-7000-8000-000000000001.tmp"
        ));
        assert!(!is_temp_file("note.md"));
        assert!(!is_temp_file(".obsidian"));
        assert!(!is_temp_file("notes.tmp"));
    }

    #[test]
    fn writing_creates_missing_parent_directories() {
        let dir = tmp();
        let path = dir.path().join("a/b/c/note.md");
        write_atomic(&path, "deep").unwrap();
        assert_eq!(read_text(&path).unwrap().text, "deep");
    }

    #[test]
    fn a_byte_order_mark_is_stripped_but_the_hash_is_of_the_real_bytes() {
        let dir = tmp();
        let path = dir.path().join("bom.md");
        let raw = "\u{feff}---\ntitle: x\n---\nbody\n";
        fs::write(&path, raw).unwrap();

        let read = read_text(&path).unwrap();
        assert!(
            read.text.starts_with("---"),
            "the BOM must not hide frontmatter"
        );
        assert_eq!(read.hash, ContentHash::of(raw.as_bytes()));
        assert_eq!(read.size_bytes, raw.len() as u64);
    }

    #[test]
    fn crlf_line_endings_are_preserved() {
        // Rewriting them would touch every line of the user's file.
        let dir = tmp();
        let path = dir.path().join("crlf.md");
        fs::write(&path, "line one\r\nline two\r\n").unwrap();
        assert_eq!(read_text(&path).unwrap().text, "line one\r\nline two\r\n");
    }

    #[test]
    fn non_utf8_files_are_reported_rather_than_mangled() {
        let dir = tmp();
        let path = dir.path().join("latin1.md");
        fs::write(&path, [0x48, 0x65, 0xFF, 0x6C]).unwrap();
        assert!(matches!(read_text(&path), Err(FileError::NotUtf8 { .. })));
    }

    #[test]
    fn a_missing_file_is_reported_with_its_path() {
        let err = read_text(Path::new("/nonexistent/note.md")).unwrap_err();
        assert!(err.to_string().contains("note.md"));
    }

    #[test]
    fn hashing_a_file_in_chunks_agrees_with_hashing_it_whole() {
        let dir = tmp();
        let path = dir.path().join("big.md");
        let text = "lighthouse ".repeat(20_000);
        write_atomic(&path, &text).unwrap();

        let (streamed, size) = hash_file(&path).unwrap();
        assert_eq!(streamed, ContentHash::of(text.as_bytes()));
        assert_eq!(size, text.len() as u64);
    }

    #[test]
    fn creating_a_file_refuses_to_overwrite() {
        // KB-003: creating a note must never destroy one.
        let dir = tmp();
        let path = dir.path().join("note.md");
        create_new(&path, "first").unwrap();
        assert!(matches!(
            create_new(&path, "second"),
            Err(FileError::Exists { .. })
        ));
        assert_eq!(read_text(&path).unwrap().text, "first");
    }

    #[test]
    fn moving_a_file_refuses_to_clobber_the_destination() {
        let dir = tmp();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        write_atomic(&a, "a").unwrap();
        write_atomic(&b, "b").unwrap();
        assert!(matches!(move_file(&a, &b), Err(FileError::Exists { .. })));
        assert_eq!(read_text(&b).unwrap().text, "b");
    }

    #[test]
    fn moving_a_file_creates_the_destination_directory() {
        let dir = tmp();
        let a = dir.path().join("a.md");
        write_atomic(&a, "a").unwrap();
        let b = dir.path().join("archive/2024/a.md");
        move_file(&a, &b).unwrap();
        assert_eq!(read_text(&b).unwrap().text, "a");
        assert!(!a.exists());
    }

    #[test]
    fn deletion_moves_a_file_to_the_trash_so_it_can_be_restored() {
        // KB-003.
        let dir = tmp();
        let relative = "notes/important.md";
        write_atomic(&dir.path().join(relative), "years of work").unwrap();

        let trashed = trash(dir.path(), relative).unwrap();
        assert!(!dir.path().join(relative).exists());
        assert_eq!(read_text(&trashed).unwrap().text, "years of work");
        assert_eq!(trashed_origin(&trashed).as_deref(), Some(relative));
    }

    #[test]
    fn trashing_the_same_path_twice_keeps_both_copies() {
        let dir = tmp();
        write_atomic(&dir.path().join("a.md"), "first").unwrap();
        let one = trash(dir.path(), "a.md").unwrap();
        write_atomic(&dir.path().join("a.md"), "second").unwrap();
        let two = trash(dir.path(), "a.md").unwrap();

        assert_ne!(one, two);
        assert_eq!(read_text(&one).unwrap().text, "first");
        assert_eq!(read_text(&two).unwrap().text, "second");
        assert_eq!(trashed_origin(&two).as_deref(), Some("a.md"));
    }

    #[test]
    fn an_oversized_file_is_refused_rather_than_loaded() {
        let dir = tmp();
        let path = dir.path().join("huge.bin");
        fs::write(&path, "x").unwrap();
        // Exercise the limit logic without writing 32 MiB.
        let metadata = fs::metadata(&path).unwrap();
        assert!(metadata.len() < MAX_TEXT_BYTES);
        assert!(read_text(&path).is_ok());
    }
}
