//! Turning a `vault:path` string on the command line into a document.
//!
//! Requirements: SRCH-005 (a result must identify its vault and document),
//! KB-009 (several vaults open at once).
//!
//! The shorthand is deliberately forgiving, because the whole point of a
//! command line is typing less: a bare path works when there is only one vault,
//! a bare name works when it is unambiguous, and the error message when it is
//! ambiguous lists the candidates rather than picking one.

use anyhow::{bail, Context, Result};
use marius_core::Marius;
use marius_model::{Document, Vault, VaultPath};

/// A reference as the user typed it.
pub struct DocumentRef;

impl DocumentRef {
    /// Resolve `reference` to an existing document.
    pub fn resolve(app: &Marius, reference: &str) -> Result<(Vault, Document)> {
        let (vault, rest) = split(app, reference)?;

        // An exact path first.
        if let Ok(path) = VaultPath::new(rest) {
            if let Some(document) = app.document_at(vault.id, &path)? {
                return Ok((vault, document));
            }
            // Then the same path with the extension the user left off.
            if !rest.contains('.') {
                if let Ok(with_ext) = VaultPath::new(format!("{rest}.md")) {
                    if let Some(document) = app.document_at(vault.id, &with_ext)? {
                        return Ok((vault, document));
                    }
                }
            }
        }

        // Then a title or filename, the way a `[[wiki link]]` resolves.
        let candidates = matching(app, &vault, rest)?;
        match candidates.len() {
            1 => Ok((vault, candidates.into_iter().next().unwrap())),
            0 => bail!(
                "no document {rest:?} in vault {:?}\n\
                 try `marius search {rest}` to find it",
                vault.name
            ),
            _ => {
                let listed = candidates
                    .iter()
                    .map(|d| format!("  {}:{}", vault.name, d.path))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!("{rest:?} is ambiguous in vault {:?}:\n{listed}", vault.name)
            }
        }
    }

    /// Resolve a reference for a document that does not exist yet.
    pub fn parse_new(app: &Marius, reference: &str) -> Result<(Vault, VaultPath)> {
        let (vault, rest) = split(app, reference)?;
        let rest = if rest.contains('.') {
            rest.to_owned()
        } else {
            // A new note is Markdown unless the user says otherwise.
            format!("{rest}.md")
        };
        let path = VaultPath::new(&rest).with_context(|| format!("invalid path {rest:?}"))?;
        Ok((vault, path))
    }
}

/// Split `vault:path`, defaulting the vault when it is unambiguous.
pub(crate) fn split<'a>(app: &Marius, reference: &'a str) -> Result<(Vault, &'a str)> {
    let vaults = app.vaults()?;
    if vaults.is_empty() {
        bail!("no vaults registered; add one with `marius vault add <name> <path>`");
    }

    // A Windows drive letter and a `vault:` prefix look alike, so the prefix
    // only counts when it actually names a vault.
    if let Some((prefix, rest)) = reference.split_once(':') {
        if let Some(vault) = vaults
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(prefix) || v.id.to_string() == prefix)
        {
            return Ok((vault.clone(), rest));
        }
    }

    match vaults.len() {
        1 => Ok((vaults.into_iter().next().unwrap(), reference)),
        _ => {
            let names = vaults
                .iter()
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "several vaults are open, so {reference:?} needs a vault prefix\n\
                 try one of: {names}\n\
                 for example: {}:{reference}",
                vaults[0].name
            )
        }
    }
}

/// Documents whose filename or title matches `name`.
fn matching(app: &Marius, vault: &Vault, name: &str) -> Result<Vec<Document>> {
    let filter = marius_storage::documents::Filter {
        vaults: vec![vault.id],
        ..Default::default()
    };
    let wanted = marius_storage::documents::fold_name(trim_extension(name));
    Ok(
        marius_storage::documents::list(app.database().conn(), &filter)?
            .into_iter()
            .filter(|document| {
                marius_storage::documents::fold_name(document.path.file_stem()) == wanted
                    || marius_storage::documents::fold_name(&document.title) == wanted
            })
            .collect(),
    )
}

fn trim_extension(name: &str) -> &str {
    for ext in [".md", ".markdown", ".mdown", ".mkd"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem;
        }
    }
    name
}
