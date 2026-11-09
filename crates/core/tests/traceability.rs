//! Keeps the traceability matrix honest.
//!
//! The specification says its requirement IDs "provide the basis for a
//! traceability matrix". A matrix maintained by hand drifts the moment someone
//! adds a requirement, so these tests check `docs/TRACEABILITY.md` against
//! `knowledge_management_requirements.md` mechanically: every ID is accounted
//! for, exactly once, with a status from the fixed vocabulary — and the stated
//! totals match the rows.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/core is two levels below the repository root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

const PREFIXES: &[&str] = &[
    "ARCH", "KB", "MD", "IM", "TAG", "AI", "SRCH", "GRAPH", "SYNC", "PLUG", "UI", "SEC", "NFR",
    "AC",
];

/// Requirement IDs that start a table row, in document order.
fn row_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("| ")?;
            let id = rest.split(" |").next()?.trim();
            let (prefix, number) = id.split_once('-')?;
            (PREFIXES.contains(&prefix)
                && !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit()))
            .then(|| id.to_owned())
        })
        .collect()
}

/// Requirement ID to status, from the matrix.
fn matrix_statuses() -> BTreeMap<String, String> {
    let text = read("docs/TRACEABILITY.md");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("| ") else {
            continue;
        };
        let mut columns = rest.split(" | ");
        let Some(id) = columns.next().map(str::trim) else {
            continue;
        };
        let Some((prefix, number)) = id.split_once('-') else {
            continue;
        };
        if !PREFIXES.contains(&prefix) || number.is_empty() {
            continue;
        }
        if !number.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let status = columns
            .next()
            .map(|s| s.trim().trim_matches('*').to_owned())
            .unwrap_or_default();
        assert!(
            out.insert(id.to_owned(), status).is_none(),
            "{id} appears in the matrix more than once"
        );
    }
    out
}

#[test]
fn every_requirement_in_the_specification_is_in_the_matrix() {
    let spec: BTreeSet<String> = row_ids(&read("knowledge_management_requirements.md"))
        .into_iter()
        .collect();
    assert!(
        spec.len() > 150,
        "only found {} requirement IDs in the specification; has its format changed?",
        spec.len()
    );

    let matrix = matrix_statuses();
    let missing: Vec<&String> = spec.iter().filter(|id| !matrix.contains_key(*id)).collect();
    assert!(
        missing.is_empty(),
        "these requirements are not in docs/TRACEABILITY.md: {missing:?}"
    );
}

#[test]
fn the_matrix_invents_no_requirements() {
    let spec: BTreeSet<String> = row_ids(&read("knowledge_management_requirements.md"))
        .into_iter()
        .collect();
    let invented: Vec<String> = matrix_statuses()
        .into_keys()
        .filter(|id| !spec.contains(id))
        .collect();
    assert!(
        invented.is_empty(),
        "docs/TRACEABILITY.md lists requirements the specification does not: {invented:?}"
    );
}

#[test]
fn every_status_comes_from_the_fixed_vocabulary() {
    // A free-text status ("mostly", "soon") is how a matrix stops meaning
    // anything.
    const ALLOWED: &[&str] = &["done", "partial", "ready", "open"];
    for (id, status) in matrix_statuses() {
        assert!(
            ALLOWED.contains(&status.as_str()),
            "{id} has status {status:?}, which is not one of {ALLOWED:?}"
        );
    }
}

#[test]
fn the_stated_totals_match_the_rows() {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for status in matrix_statuses().into_values() {
        *counts.entry(status).or_default() += 1;
    }
    let text = read("docs/TRACEABILITY.md");
    for (status, count) in &counts {
        let claim = format!("{count} {status}");
        assert!(
            text.contains(&claim),
            "the matrix has {count} {status} rows but does not state {claim:?}"
        );
    }
}

#[test]
fn the_specification_is_present_and_unmodified_in_shape() {
    // The matrix is only meaningful against the specification it was written
    // for. This is a smoke check that the source document is still here and
    // still the one with these sections in it.
    let spec = read("knowledge_management_requirements.md");
    for heading in [
        "## 2. Confirmed architectural decisions",
        "## 3. Functional requirements",
        "### 4.1 Data integrity rules",
        "## 8. Verification and acceptance scenarios",
    ] {
        assert!(
            spec.contains(heading),
            "the specification is missing {heading:?}"
        );
    }
}
