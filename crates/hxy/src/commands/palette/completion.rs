//! Inline ghost-completion driver for the calculator palette.
//!
//! `compute_suggestion` is the single entry point: it inspects the
//! palette's current query string, decides what kind of completion
//! makes sense at the cursor (template stem, child field, meta
//! accessor), asks the supplied [`PathResolver`] for candidates,
//! and returns the suffix the host should stage as the inline
//! ghost text. The egui-palette widget renders that suffix
//! selected so a matching keystroke consumes one char and a
//! non-matching one wipes it -- standard browser-URL UX.
//!
//! Completion only fires for `@<expr>` / `=<expr>` queries in the
//! main mode. Other queries pass through with no ghost text.

use hxy_calculator::Expr;
use hxy_calculator::MetaKind;
use hxy_calculator::Path;
use hxy_calculator::PathResolver;

/// Context the user is editing in. Decided by walking back from
/// the end of the query: the trailing identifier is the partial
/// the user is typing; what comes immediately before it picks
/// the kind.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// Beginning of expression or after a binary operator.
    /// Candidates: template stems.
    Top,
    /// After `parent.`. Candidates: direct children of `parent`.
    Segment { parent: Path },
    /// After `parent::`. Candidates: meta accessors (`offset`,
    /// `len`).
    Meta,
}

/// Compute an inline completion for the palette's current query.
/// Returns `None` when the query isn't a calculator expression,
/// when the cursor isn't in a completion context, or when no
/// candidate extends the partial identifier.
pub fn compute_suggestion(query: &str, resolver: &dyn PathResolver) -> Option<String> {
    let rest = strip_calc_prefix(query)?;
    let (prefix, kind) = analyse(rest)?;
    let candidates = candidates_for(kind, resolver);
    pick_suffix(prefix, &candidates)
}

/// Strip the leading `@` or `=` (with any leading whitespace).
/// Returns `None` for a non-calculator query so the caller can
/// skip completion entirely instead of suggesting against
/// command-name fuzzy text.
fn strip_calc_prefix(query: &str) -> Option<&str> {
    let trimmed = query.trim_start();
    let rest = trimmed.strip_prefix('@').or_else(|| trimmed.strip_prefix('='))?;
    Some(rest)
}

/// Decide what's being typed at the end of `input` and which
/// candidate set applies. The returned `&str` is the partial
/// identifier the user has already typed -- the suggestion will
/// extend it.
fn analyse(input: &str) -> Option<(&str, Kind)> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut prefix_start = len;
    while prefix_start > 0 {
        let b = bytes[prefix_start - 1];
        if is_ident_byte(b) {
            prefix_start -= 1;
        } else {
            break;
        }
    }
    // Reject prefixes that would parse as a number ("0x100", "42").
    // The user is mid-literal; offering a function or template stem
    // here would replace the digits with letters and produce a parse
    // error.
    if !input[prefix_start..len].is_empty() && bytes[prefix_start].is_ascii_digit() {
        return None;
    }
    let prefix = &input[prefix_start..len];
    if prefix_start == 0 {
        return Some((prefix, Kind::Top));
    }
    // `::` is two chars; check both before falling through to the
    // single-char preceding-byte match.
    if prefix_start >= 2 && bytes[prefix_start - 1] == b':' && bytes[prefix_start - 2] == b':' {
        return Some((prefix, Kind::Meta));
    }
    let preceding = bytes[prefix_start - 1];
    let kind = match preceding {
        b'.' => {
            let parent = extract_path_before_dot(input, prefix_start - 1)?;
            Kind::Segment { parent }
        }
        b'+' | b'-' | b'*' | b'/' | b'%' | b' ' | b'\t' | b'(' => Kind::Top,
        // After `)`, `]`, `#`, `[`, a single `:`, or any other
        // non-ident character we'd be in an operator-position,
        // array-index slot, or partway through `::`; no name
        // completion is appropriate.
        _ => return None,
    };
    Some((prefix, kind))
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Pull the longest path-shaped substring ending at `dot_idx`
/// out of `input` and parse it. Used to recover the parent path
/// for a `parent.<segment>` completion. Returns `None` when the
/// recovered substring doesn't parse as a path -- the user might
/// have typed something half-formed (`png.[0]`), and we just
/// won't offer completion in that case.
fn extract_path_before_dot(input: &str, dot_idx: usize) -> Option<Path> {
    let bytes = input.as_bytes();
    let mut start = dot_idx;
    while start > 0 {
        let b = bytes[start - 1];
        if is_ident_byte(b) || matches!(b, b'.' | b'[' | b']' | b'#') {
            start -= 1;
        } else {
            break;
        }
    }
    let path_str = &input[start..dot_idx];
    if path_str.is_empty() {
        return None;
    }
    match hxy_calculator::parse(path_str) {
        Ok(Expr::Path(p)) => Some(p),
        _ => None,
    }
}

fn candidates_for(kind: Kind, resolver: &dyn PathResolver) -> Vec<String> {
    match kind {
        Kind::Top => resolver.template_stems(),
        Kind::Segment { parent } => resolver.list_children(&parent),
        Kind::Meta => MetaKind::all().iter().map(|m| m.as_str().to_owned()).collect(),
    }
}

/// Choose a candidate that extends `prefix` and return the
/// suffix to ghost. Match is case-sensitive: the user typing
/// `Of` won't fish out `offset`, because mixing typed-prefix
/// case with canonical-suffix case produces ugly composites
/// like `Offset` rendered as `Of` + `fset`. Ranking is plain
/// alphabetical -- predictable and stable as the user types.
///
/// Returns `None` for an empty prefix: browser URL bars don't
/// pop a suggestion at zero typed chars, and doing so here would
/// constantly surface a random first-alphabetical candidate
/// after every operator (`=1 + |`).
fn pick_suffix(prefix: &str, candidates: &[String]) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let mut matches: Vec<&String> =
        candidates.iter().filter(|c| c.starts_with(prefix) && c.len() > prefix.len()).collect();
    matches.sort();
    let first = matches.first()?;
    let suffix: String = first.chars().skip(prefix.chars().count()).collect();
    if suffix.is_empty() { None } else { Some(suffix) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hxy_calculator::FieldRef;
    use hxy_calculator::ResolveError;

    /// Stub resolver that exposes a couple of templates and
    /// canned children, with no real lookup support.
    struct StubResolver {
        stems: Vec<String>,
        png_children: Vec<String>,
    }

    impl PathResolver for StubResolver {
        fn lookup(&self, _path: &Path) -> Result<FieldRef, ResolveError> {
            Err(ResolveError::NoContext)
        }
        fn template_stems(&self) -> Vec<String> {
            self.stems.clone()
        }
        fn list_children(&self, path: &Path) -> Vec<String> {
            if path.root.eq_ignore_ascii_case("png") && path.segments.is_empty() {
                self.png_children.clone()
            } else {
                Vec::new()
            }
        }
    }

    fn stub() -> StubResolver {
        StubResolver {
            stems: vec!["png".into(), "elf".into(), "jpeg".into()],
            png_children: vec!["signature".into(), "chunks".into(), "IDAT".into()],
        }
    }

    #[test]
    fn no_calc_prefix_no_completion() {
        assert_eq!(compute_suggestion("git", &stub()), None);
        assert_eq!(compute_suggestion("foo @bar", &stub()), None);
    }

    #[test]
    fn empty_after_prefix_no_completion() {
        // Plain `=` or `@` with nothing typed yet -- no partial
        // to extend. (The palette already shows a "type an
        // expression" prompt in this state.)
        assert_eq!(compute_suggestion("=", &stub()), None);
        assert_eq!(compute_suggestion("@", &stub()), None);
    }

    #[test]
    fn template_stem_top_level() {
        // `=p` -> ghost `ng` (alphabetically the only `p*` stem)
        assert_eq!(compute_suggestion("=p", &stub()).as_deref(), Some("ng"));
        assert_eq!(compute_suggestion("=el", &stub()).as_deref(), Some("f"));
    }

    #[test]
    fn template_stem_after_open_paren() {
        // `=(p` -> ghost `ng`. The `(` is just a grouping; what
        // follows is a fresh top-level expression, so completion
        // ranks template stems alongside what would normally be
        // available at the start of an expression.
        assert_eq!(compute_suggestion("=(p", &stub()).as_deref(), Some("ng"));
    }

    #[test]
    fn child_segment_after_dot() {
        // `=png.s` -> ghost `ignature`
        assert_eq!(compute_suggestion("=png.s", &stub()).as_deref(), Some("ignature"));
        assert_eq!(compute_suggestion("=png.c", &stub()).as_deref(), Some("hunks"));
        // Case-sensitive: `=png.ID` -> "AT" (the IDAT child).
        assert_eq!(compute_suggestion("=png.ID", &stub()).as_deref(), Some("AT"));
    }

    #[test]
    fn case_sensitive_no_match_when_case_differs() {
        // Lowercase prefix won't pick up the uppercase "IDAT"
        // child, and the wrong-case template prefix yields
        // nothing too.
        assert_eq!(compute_suggestion("=png.id", &stub()), None);
        assert_eq!(compute_suggestion("=PNG", &stub()), None);
    }

    #[test]
    fn child_segment_after_dot_with_arithmetic() {
        // `=0x1 + png.s` -> ghost `ignature`
        assert_eq!(compute_suggestion("=0x1 + png.s", &stub()).as_deref(), Some("ignature"));
    }

    #[test]
    fn meta_after_double_colon() {
        // `=png::o` -> ghost `ffset`; `=png.IDAT::l` -> ghost `en`.
        assert_eq!(compute_suggestion("=png::o", &stub()).as_deref(), Some("ffset"));
        assert_eq!(compute_suggestion("=png.IDAT::l", &stub()).as_deref(), Some("en"));
    }

    #[test]
    fn meta_partial_with_no_match_returns_none() {
        // `=png::z` -- nothing in {offset, len} starts with `z`.
        assert_eq!(compute_suggestion("=png::z", &stub()), None);
    }

    #[test]
    fn no_completion_after_single_colon() {
        // A lone `:` -- partway through typing `::` -- shouldn't
        // suggest anything. The user hasn't committed to the meta
        // accessor yet.
        assert_eq!(compute_suggestion("=png:", &stub()), None);
    }

    #[test]
    fn no_completion_inside_number() {
        // `=0x` is a number prefix; offering a stem here would
        // replace the digits.
        assert_eq!(compute_suggestion("=0x", &stub()), None);
        assert_eq!(compute_suggestion("=42", &stub()), None);
    }

    #[test]
    fn empty_prefix_after_operator_no_completion() {
        // Empty prefix -> no ghost. Matches browser-URL behaviour:
        // we don't pop a random suggestion just because the
        // cursor is at a position where one would be valid.
        assert_eq!(compute_suggestion("=0x1 + ", &stub()), None);
    }

    #[test]
    fn no_completion_after_unsupported_punct() {
        // `]` or `)` is operator territory; no name completion.
        assert_eq!(compute_suggestion("=foo]", &stub()), None);
        assert_eq!(compute_suggestion("=foo)", &stub()), None);
    }
}
