//! Fuzzy matching, backed by [`nucleo_matcher`] (same engine
//! Helix uses). Handles Unicode case folding, word-boundary and
//! consecutive-match bonuses, SIMD-accelerated scoring -- the
//! bits you'd have to hand-roll otherwise.

use nucleo_matcher::Matcher;
use nucleo_matcher::Utf32Str;
use nucleo_matcher::pattern::Pattern;

use crate::CaseMatching;
use crate::Entry;
use crate::MatcherConfig;
use crate::Normalization;

/// Filter and sort `entries` by their match score against `query`.
/// Returns indices into `entries` ordered best match first. An empty
/// query yields every index in declaration order.
///
/// `haystack_of` builds the string to score against -- typically
/// `title + " " + subtitle` so both participate in matching. The
/// scoring / case / normalisation knobs come straight from the
/// matching nucleo types; see [`MatcherConfig`], [`CaseMatching`],
/// and [`Normalization`].
pub fn filter_and_sort<A, F>(
    query: &str,
    entries: &[Entry<A>],
    matcher_cfg: &MatcherConfig,
    case: CaseMatching,
    normalization: Normalization,
    haystack_of: F,
) -> Vec<usize>
where
    F: Fn(&Entry<A>) -> String,
{
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut matcher = Matcher::new(matcher_cfg.clone());
    let pattern = Pattern::parse(query, case, normalization);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(idx, e)| {
            buf.clear();
            let haystack = haystack_of(e);
            let utf32 = Utf32Str::new(&haystack, &mut buf);
            pattern.score(utf32, &mut matcher).map(|s| (s, idx))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> (MatcherConfig, CaseMatching, Normalization) {
        (MatcherConfig::DEFAULT, CaseMatching::Smart, Normalization::Smart)
    }

    #[test]
    fn subsequence_match() {
        let entries: Vec<Entry<()>> = vec![Entry::new("open-template-file", ()), Entry::new("some-other-thing", ())];
        let (m, c, n) = defaults();
        let hits = filter_and_sort("otf", &entries, &m, c, n, |e| e.title.clone());
        assert_eq!(hits.first(), Some(&0));
    }

    #[test]
    fn prefix_beats_middle() {
        let entries: Vec<Entry<usize>> = vec![Entry::new("xoopens", 0), Entry::new("open template", 1)];
        let (m, c, n) = defaults();
        let hits = filter_and_sort("opn", &entries, &m, c, n, |e| e.title.clone());
        assert_eq!(hits.first(), Some(&1));
    }

    #[test]
    fn empty_query_matches_all() {
        let entries: Vec<Entry<usize>> = vec![Entry::new("a", 0), Entry::new("b", 1)];
        let (m, c, n) = defaults();
        assert_eq!(filter_and_sort("", &entries, &m, c, n, |e| e.title.clone()), vec![0, 1]);
    }

    #[test]
    fn non_matching_entries_dropped() {
        let entries: Vec<Entry<usize>> = vec![Entry::new("zzzzzzz", 0), Entry::new("keep", 1)];
        let (m, c, n) = defaults();
        let hits = filter_and_sort("keep", &entries, &m, c, n, |e| e.title.clone());
        assert_eq!(hits, vec![1]);
    }
}
