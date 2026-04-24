//! Subsequence-match fuzzy scorer. Plenty for a few hundred
//! entries; swap in `nucleo-matcher` or similar if you have a
//! million-entry file picker.

use crate::Entry;

/// Score `haystack` against `pattern`. `None` if the pattern isn't a
/// subsequence of the haystack (case-insensitive). Higher is better.
///
/// Bonuses:
/// - `+4` per consecutive match
/// - `+6` for matching the first character of the haystack
/// - `+3` when the preceding character is a non-alphanumeric (word
///   start like `/`, `-`, `_`, space)
/// - `+2` when the matched haystack character is uppercase
pub fn score(pattern: &str, haystack: &str) -> Option<i64> {
    let pat: Vec<char> = pattern.chars().flat_map(|c| c.to_lowercase()).collect();
    let hay: Vec<char> = haystack.chars().flat_map(|c| c.to_lowercase()).collect();
    let hay_original: Vec<char> = haystack.chars().collect();
    let mut pi = 0;
    let mut score: i64 = 0;
    let mut prev_match: Option<usize> = None;
    for (i, c) in hay.iter().enumerate() {
        if pi >= pat.len() {
            break;
        }
        if *c == pat[pi] {
            score += 1;
            if prev_match == Some(i.saturating_sub(1)) {
                score += 4;
            }
            if i == 0 {
                score += 6;
            } else if let Some(prev) = hay_original.get(i.saturating_sub(1))
                && !prev.is_alphanumeric()
            {
                score += 3;
            }
            if hay_original.get(i).is_some_and(|c| c.is_uppercase()) {
                score += 2;
            }
            prev_match = Some(i);
            pi += 1;
        }
    }
    (pi == pat.len()).then_some(score)
}

/// Filter and sort `entries` by their match score against `query`.
/// Returns indices into `entries` ordered best match first. An
/// empty query yields every index in declaration order.
///
/// `haystack_of` builds the string to score against — typically
/// `title + " " + subtitle` so both participate in matching.
pub fn filter_and_sort<A, F>(query: &str, entries: &[Entry<A>], haystack_of: F) -> Vec<usize>
where
    F: Fn(&Entry<A>) -> String,
{
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut scored: Vec<(i64, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(idx, e)| score(query, &haystack_of(e)).map(|s| (s, idx)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_match() {
        assert!(score("otf", "open-template-file").is_some());
        assert!(score("ztemp", "open-template-file").is_none());
    }

    #[test]
    fn prefix_beats_middle() {
        let prefix = score("opn", "open template").unwrap();
        let middle = score("opn", "xoopens").unwrap();
        assert!(prefix > middle);
    }

    #[test]
    fn empty_query_matches_all() {
        let entries: Vec<Entry<usize>> = vec![Entry::new("a", 0), Entry::new("b", 1)];
        assert_eq!(filter_and_sort("", &entries, |e| e.title.clone()), vec![0, 1]);
    }

    #[test]
    fn consecutive_run_beats_scattered() {
        let tight = score("abc", "xxabcxx").unwrap();
        let loose = score("abc", "aXbXcXX").unwrap();
        assert!(tight > loose);
    }
}
