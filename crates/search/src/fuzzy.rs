//! Fuzzy subsequence matching (SRCH-006), shared by the quick switcher
//! and the desktop command palette.
//!
//! Subsequence matching with the bonuses every fuzzy-finder user's
//! fingers expect: consecutive hits beat scattered ones, word starts
//! beat word middles, and earlier matches beat later ones.

/// Score `candidate` against `query`; `None` when it does not match.
/// Higher is better. An empty query matches everything equally.
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let query: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    let chars: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = candidate.chars().flat_map(|c| c.to_lowercase()).collect();
    if lower.len() != chars.len() {
        // Case folding changed lengths (ß etc.): fall back to a plain
        // containment check rather than mis-indexing.
        let q: String = query.iter().collect();
        let l: String = lower.iter().collect();
        return l.contains(&q).then_some(0);
    }

    let mut total = 0i64;
    let mut qi = 0usize;
    let mut previous_hit: Option<usize> = None;
    for (index, ch) in lower.iter().enumerate() {
        if qi == query.len() {
            break;
        }
        if *ch != query[qi] {
            continue;
        }
        let mut gain = 1;
        if previous_hit == Some(index.wrapping_sub(1)) {
            gain += 8; // consecutive run
        }
        let word_start =
            index == 0 || matches!(chars[index - 1], ' ' | '-' | '_' | '/' | ':' | '.');
        if word_start {
            gain += 6;
        }
        gain += (24i64 - index as i64).max(0) / 8; // earlier is better
        total += gain;
        previous_hit = Some(index);
        qi += 1;
    }
    (qi == query.len()).then_some(total)
}

/// Filter and rank `items` by their labels. Stable for equal scores, so
/// the caller's ordering is the tiebreak.
pub fn rank<'a, T>(query: &str, items: &'a [T], label: impl Fn(&T) -> &str) -> Vec<(usize, &'a T)> {
    let mut scored: Vec<(i64, usize, &T)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| score(query, label(item)).map(|s| (s, index, item)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .map(|(_, index, item)| (index, item))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequences_match_and_missing_letters_do_not() {
        assert!(score("tgr", "Toggle graph").is_some());
        assert!(score("xyz", "Toggle graph").is_none());
        assert!(score("", "anything").is_some());
    }

    #[test]
    fn word_starts_and_runs_beat_scatter() {
        let initials = score("nn", "New note").unwrap();
        let buried = score("nn", "Unindent line nn").unwrap();
        assert!(initials > buried);
        let consecutive = score("graph", "Toggle graph").unwrap();
        let scattered = score("graph", "Great raspberry pie, huh").unwrap();
        assert!(consecutive > scattered);
    }

    #[test]
    fn ranking_is_stable_and_case_insensitive() {
        let items = ["Zoom in", "Zoom out", "Zoom: reset", "Open graph"];
        let ranked = rank("zoom", &items, |s| s);
        assert_eq!(ranked.len(), 3);
        assert_eq!(*ranked[0].1, "Zoom in");
        assert_eq!(*rank("ZOOM IN", &items, |s| s)[0].1, "Zoom in");
    }
}
