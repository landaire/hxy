//! Fuzzy matching, backed by [`nucleo_matcher`] (same engine
//! Helix uses). Handles Unicode case folding, word-boundary and
//! consecutive-match bonuses, SIMD-accelerated scoring -- the
//! bits you'd have to hand-roll otherwise.

use std::borrow::Cow;

use nucleo_matcher::Matcher;
use nucleo_matcher::Utf32Str;
use nucleo_matcher::pattern::Pattern;

use crate::CaseMatching;
use crate::Entry;
use crate::MatcherConfig;
use crate::Normalization;

/// A single match result: the index into the caller's `entries`
/// slice and the set of character positions (into the haystack
/// [`haystack_of`] produced) the query hit. `match_indices` is
/// empty for an empty query.
#[derive(Clone, Debug, Default)]
pub struct MatchResult {
    pub index: usize,
    pub match_indices: Vec<u32>,
}

/// Filter and sort `entries` by their match score against `query`.
/// Returns [`MatchResult`]s ordered best match first, each carrying
/// the positions of the matched characters so the renderer can
/// highlight them. An empty query yields every entry in declaration
/// order with empty `match_indices`.
///
/// `haystack_of` builds the string to score against. It returns a
/// [`Cow<str>`] so callers can return a borrowed reference to an
/// existing field (the common case: `Cow::Borrowed(&entry.title)`)
/// and only pay for an allocation when they genuinely need to
/// concatenate, e.g. `title + " " + subtitle`. The scoring / case /
/// normalisation knobs come straight from the matching nucleo types;
/// see [`MatcherConfig`], [`CaseMatching`], and [`Normalization`].
pub fn filter_and_sort<A, F>(
    query: &str,
    entries: &[Entry<A>],
    matcher_cfg: &MatcherConfig,
    case: CaseMatching,
    normalization: Normalization,
    haystack_of: F,
) -> Vec<MatchResult>
where
    F: for<'e> Fn(&'e Entry<A>) -> Cow<'e, str>,
{
    if query.is_empty() {
        return (0..entries.len()).map(|index| MatchResult { index, match_indices: Vec::new() }).collect();
    }
    let mut matcher = Matcher::new(matcher_cfg.clone());
    let pattern = Pattern::parse(query, case, normalization);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, MatchResult)> = entries
        .iter()
        .enumerate()
        .filter_map(|(idx, e)| {
            buf.clear();
            let haystack = haystack_of(e);
            let utf32 = Utf32Str::new(haystack.as_ref(), &mut buf);
            let mut indices: Vec<u32> = Vec::new();
            pattern
                .indices(utf32, &mut matcher, &mut indices)
                .map(|s| (s, MatchResult { index: idx, match_indices: indices }))
        })
        .collect();
    // Deduplicate and sort each match-index list so the renderer
    // can walk it monotonically. nucleo returns positions per atom
    // which can overlap when multiple atoms hit the same char.
    for (_, mr) in scored.iter_mut() {
        mr.match_indices.sort_unstable();
        mr.match_indices.dedup();
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, mr)| mr).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> (MatcherConfig, CaseMatching, Normalization) {
        (MatcherConfig::DEFAULT, CaseMatching::Smart, Normalization::Smart)
    }

    fn indices_of(results: &[MatchResult]) -> Vec<usize> {
        results.iter().map(|r| r.index).collect()
    }

    #[test]
    fn subsequence_match() {
        let entries: Vec<Entry<()>> = vec![Entry::new("open-template-file", ()), Entry::new("some-other-thing", ())];
        let (m, c, n) = defaults();
        let hits = filter_and_sort("otf", &entries, &m, c, n, |e| Cow::Borrowed(e.title.as_str()));
        assert_eq!(hits.first().map(|r| r.index), Some(0));
    }

    #[test]
    fn prefix_beats_middle() {
        let entries: Vec<Entry<usize>> = vec![Entry::new("xoopens", 0), Entry::new("open template", 1)];
        let (m, c, n) = defaults();
        let hits = filter_and_sort("opn", &entries, &m, c, n, |e| Cow::Borrowed(e.title.as_str()));
        assert_eq!(hits.first().map(|r| r.index), Some(1));
    }

    #[test]
    fn empty_query_matches_all() {
        let entries: Vec<Entry<usize>> = vec![Entry::new("a", 0), Entry::new("b", 1)];
        let (m, c, n) = defaults();
        let hits = filter_and_sort("", &entries, &m, c, n, |e| Cow::Borrowed(e.title.as_str()));
        assert_eq!(indices_of(&hits), vec![0, 1]);
        assert!(hits.iter().all(|r| r.match_indices.is_empty()));
    }

    #[test]
    fn non_matching_entries_dropped() {
        let entries: Vec<Entry<usize>> = vec![Entry::new("zzzzzzz", 0), Entry::new("keep", 1)];
        let (m, c, n) = defaults();
        let hits = filter_and_sort("keep", &entries, &m, c, n, |e| Cow::Borrowed(e.title.as_str()));
        assert_eq!(indices_of(&hits), vec![1]);
    }

    #[test]
    fn match_indices_are_sorted_and_dedup() {
        let entries: Vec<Entry<()>> = vec![Entry::new("open-file", ())];
        let (m, c, n) = defaults();
        let hits = filter_and_sort("open", &entries, &m, c, n, |e| Cow::Borrowed(e.title.as_str()));
        let first = hits.first().expect("match");
        // Every subsequent index must be strictly greater -- sorted
        // + deduped so the renderer can walk it monotonically.
        assert!(first.match_indices.windows(2).all(|w| w[0] < w[1]));
        assert!(!first.match_indices.is_empty());
    }
}
