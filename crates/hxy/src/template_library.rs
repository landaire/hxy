//! Auto-detected template library.
//!
//! Scans the user's templates directory for `.bt` files and reads the
//! 010 Editor header convention each one carries at the top:
//!
//! ```text
//! //      File: ZIP.bt
//! // File Mask: *.zip,*.jar
//! //  ID Bytes: 50 4B //PK
//! ```
//!
//! Matching an opened file against these fields is enough to suggest
//! (or auto-run) the right template without a manual picker.
//!
//! Only `.bt` is recognised today -- the comment style is idiomatic to
//! 010 templates. Other runtimes can ship their own detectors later.
//!
//! Only the header is parsed; the body is handed verbatim to the
//! runtime when the user actually runs the template.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashSet;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// How many bytes to read from a data file when magic-matching.
/// 010 Editor `ID Bytes` fields are at most a dozen bytes; 64 gives
/// generous slack.
pub const DETECTION_WINDOW: usize = 64;

/// How many bytes to read from a `.bt` file when parsing its header.
/// Real templates keep their metadata block well inside the first
/// couple KB.
const HEADER_READ_LIMIT: usize = 4096;

#[derive(Clone, Debug)]
pub struct TemplateEntry {
    pub path: PathBuf,
    /// Short display name (`ZIP.bt`). Used for the "Run ZIP.bt" toolbar
    /// label when this template is the best match for a file.
    pub name: String,
    /// Extension globs declared in the header (`*.zip` -> `zip`). Lower-
    /// cased; leading `*.` stripped.
    pub extensions: Vec<String>,
    /// One or more magic byte prefixes. Each entry is the raw bytes
    /// from an `ID Bytes:` field (e.g. `[0x50, 0x4B]` for `PK`).
    pub magic: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub struct TemplateLibrary {
    entries: Vec<TemplateEntry>,
}

impl TemplateLibrary {
    /// Scan `dir` for `.bt` files and parse each one's header. A
    /// missing dir yields an empty library (hosts may call this with
    /// a path that doesn't exist yet).
    pub fn load_from(dir: Option<&Path>) -> Self {
        let Some(dir) = dir else { return Self::default() };
        let Ok(read) = fs::read_dir(dir) else { return Self::default() };
        let mut entries = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("bt") {
                continue;
            }
            if let Some(parsed) = parse_template(&path) {
                entries.push(parsed);
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self { entries }
    }

    pub fn entries(&self) -> &[TemplateEntry] {
        &self.entries
    }

    /// Best match for a file characterised by `extension` and its
    /// leading `head_bytes`. Magic-byte matches beat extension
    /// matches -- a `.zip` extension on a file that starts with `PK`
    /// wins over a `.zip`-declared template that requires a different
    /// magic.
    pub fn suggest(&self, extension: Option<&str>, head_bytes: &[u8]) -> Option<&TemplateEntry> {
        let ext_lower = extension.map(str::to_ascii_lowercase);
        let mut ext_candidate: Option<&TemplateEntry> = None;
        for entry in &self.entries {
            let ext_match = ext_lower.as_ref().is_some_and(|e| entry.extensions.iter().any(|x| x == e));
            let magic_match = !entry.magic.is_empty() && entry.magic.iter().any(|m| head_bytes.starts_with(m));
            if magic_match && (entry.extensions.is_empty() || ext_match) {
                return Some(entry);
            }
            if ext_match && ext_candidate.is_none() {
                ext_candidate = Some(entry);
            }
        }
        ext_candidate
    }

    /// Return every entry sorted by how well it matches
    /// `(extension, head_bytes)`: magic + extension hits first, then
    /// magic-only, then extension-only, then the rest in the
    /// library's default (alphabetical) order. Used by the command
    /// palette's `Run Template` list so the most plausible runner
    /// for the active file floats to the top.
    pub fn rank_entries(&self, extension: Option<&str>, head_bytes: &[u8]) -> Vec<&TemplateEntry> {
        let ext_lower = extension.map(str::to_ascii_lowercase);
        let mut scored: Vec<(u8, usize, &TemplateEntry)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let ext_match = ext_lower.as_ref().is_some_and(|e| entry.extensions.iter().any(|x| x == e));
                let magic_match = !entry.magic.is_empty() && entry.magic.iter().any(|m| head_bytes.starts_with(m));
                let score = match (magic_match, ext_match) {
                    (true, true) => 3,
                    (true, false) => 2,
                    (false, true) => 1,
                    (false, false) => 0,
                };
                (score, idx, entry)
            })
            .collect();
        // Higher score first; ties broken by original alphabetical
        // ordering (already stored in `idx`).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, e)| e).collect()
    }
}

fn parse_template(path: &Path) -> Option<TemplateEntry> {
    let contents = fs::read(path).ok()?;
    let head = &contents[..contents.len().min(HEADER_READ_LIMIT)];
    let text = std::str::from_utf8(head).ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();

    let mut extensions = Vec::new();
    let mut magic = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(body) = trimmed.strip_prefix("//") else { continue };
        let body = body.trim_start();
        if let Some(rest) = header_value(body, "File Mask") {
            extensions.extend(parse_extensions(rest));
        } else if let Some(rest) = header_value(body, "ID Bytes") {
            magic.extend(parse_id_bytes(rest));
        }
    }
    if extensions.is_empty() && magic.is_empty() {
        return None;
    }
    Some(TemplateEntry { path: path.to_path_buf(), name, extensions, magic })
}

/// `"File Mask: *.zip"` -> `Some("*.zip")` for a given field name.
/// Case-insensitive, tolerates extra internal whitespace from the
/// way 010 templates align the colon column.
fn header_value<'a>(body: &'a str, field: &str) -> Option<&'a str> {
    let lower = body.to_ascii_lowercase();
    let field_lower = field.to_ascii_lowercase();
    let idx = lower.find(&field_lower)?;
    if idx != 0 {
        return None;
    }
    let after = body[field.len()..].trim_start();
    let rest = after.strip_prefix(':')?.trim_start();
    Some(rest)
}

/// `"*.zip,*.jar"` -> `["zip", "jar"]`. Strips trailing inline `//...`
/// comment if the template author left one.
fn parse_extensions(raw: &str) -> Vec<String> {
    strip_inline_comment(raw)
        .split(',')
        .filter_map(|chunk| {
            let s = chunk.trim();
            let s = s.strip_prefix("*.").unwrap_or(s);
            let s = s.trim_matches('*').trim();
            if s.is_empty() { None } else { Some(s.to_ascii_lowercase()) }
        })
        .collect()
}

/// `"50 4B //PK"` -> `[[0x50, 0x4B]]`. Multiple magic sequences
/// separated by commas yield separate byte vectors.
fn parse_id_bytes(raw: &str) -> Vec<Vec<u8>> {
    let trimmed = strip_inline_comment(raw);
    trimmed
        .split(',')
        .filter_map(|chunk| {
            let mut out = Vec::new();
            for token in chunk.split_whitespace() {
                let byte = u8::from_str_radix(token, 16).ok()?;
                out.push(byte);
            }
            if out.is_empty() { None } else { Some(out) }
        })
        .collect()
}

/// 010 Editor templates sometimes annotate magic with an ASCII hint
/// like `50 4B //PK`. Strip any trailing `//...` so the parser
/// doesn't try to turn `PK` into a hex byte.
fn strip_inline_comment(s: &str) -> &str {
    match s.find("//") {
        Some(i) => s[..i].trim_end(),
        None => s.trim_end(),
    }
}

/// Extract the filename arguments of every `#include "..."` directive
/// in `text`, in source order with duplicates removed. Malformed lines
/// (missing quotes, unterminated strings) are silently skipped.
pub fn parse_include_directives(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines() {
        let Some(name) = extract_include_target(line) else { continue };
        if name.is_empty() {
            continue;
        }
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

fn extract_include_target(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("#include")?;
    // Must be followed by whitespace or an opening quote -- don't match
    // `#includeextra`.
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '"' || c == '<') {
        return None;
    }
    let rest = rest.trim_start();
    let (open, close) = match rest.chars().next()? {
        '"' => ('"', '"'),
        '<' => ('<', '>'),
        _ => return None,
    };
    let after_open = &rest[open.len_utf8()..];
    let end = after_open.find(close)?;
    Some(after_open[..end].to_owned())
}

/// Summary of walking the `#include` graph rooted at a template.
#[derive(Clone, Debug, Default)]
pub struct IncludeClosure {
    /// Absolute, canonicalised paths of every reachable include,
    /// excluding the root entry itself. Ordered by discovery.
    pub resolved: Vec<PathBuf>,
    /// Directives that failed to resolve -- the raw target string and
    /// the file that referenced it, in discovery order.
    pub missing: Vec<(PathBuf, String)>,
}

/// Walk `#include` directives starting at `entry`, resolving each
/// relative to the file that contains it. Returns every reachable
/// include under `base_dir` (canonicalised). `entry` itself is not
/// included in `resolved`.
///
/// Targets that escape `base_dir` (absolute paths, `..` that walks
/// above the base, symlinks to elsewhere) are reported in `missing`
/// rather than resolved, so a template installer can refuse to copy
/// them. During install-time discovery, pass `entry.parent()` as
/// `base_dir`; at parse-time with the sandbox, pass the templates
/// directory.
pub fn collect_include_closure(entry: &Path, base_dir: &Path) -> IncludeClosure {
    let Ok(canonical_base) = base_dir.canonicalize() else {
        return IncludeClosure::default();
    };
    let mut resolved: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut missing: Vec<(PathBuf, String)> = Vec::new();

    if let Ok(root) = entry.canonicalize() {
        seen.insert(root);
    }

    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(entry.to_path_buf());
    while let Some(current) = queue.pop_front() {
        let Ok(text) = fs::read_to_string(&current) else { continue };
        let parent = current.parent().unwrap_or_else(|| Path::new("."));
        for target in parse_include_directives(&text) {
            match resolve_within(&target, parent, &canonical_base) {
                Some(path) if seen.insert(path.clone()) => {
                    resolved.push(path.clone());
                    queue.push_back(path);
                }
                Some(_) => {}
                None => missing.push((current.clone(), target)),
            }
        }
    }
    IncludeClosure { resolved, missing }
}

/// Resolve `target` relative to `parent`, succeeding only if the
/// canonicalised result exists and is inside `canonical_base`. Any
/// attempt to escape via absolute paths, `..`, or symlinks fails.
fn resolve_within(target: &str, parent: &Path, canonical_base: &Path) -> Option<PathBuf> {
    // Absolute paths always escape the sandbox -- real 010 templates
    // don't use them.
    let target_path = Path::new(target);
    if target_path.is_absolute() {
        return None;
    }
    let candidate = parent.join(target_path);
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.starts_with(canonical_base) {
        return None;
    }
    Some(canonical)
}

/// Outcome of an install action. `copied` always contains at least the
/// primary template; `existing` lists files that were already present
/// and left untouched; `missing` mirrors [`IncludeClosure::missing`]
/// so the caller can surface broken references.
#[derive(Clone, Debug, Default)]
pub struct InstallReport {
    pub copied: Vec<PathBuf>,
    pub existing: Vec<PathBuf>,
    pub missing: Vec<(PathBuf, String)>,
    pub errors: Vec<(PathBuf, String)>,
}

/// Copy `src` into `dest_dir`, along with every template it `#include`s
/// from under `src.parent()`. Pre-existing destination files are kept,
/// not overwritten -- rerunning install on the same template is a
/// no-op. Returns a report describing what happened, including any
/// includes that failed to resolve.
pub fn install_template_with_deps(src: &Path, dest_dir: &Path) -> InstallReport {
    let mut report = InstallReport::default();
    let Some(src_parent) = src.parent() else { return report };
    let closure = collect_include_closure(src, src_parent);
    report.missing = closure.missing.clone();

    let mut install_one = |source: &Path| {
        let Some(name) = source.file_name() else { return };
        let dest = dest_dir.join(name);
        if dest.exists() {
            report.existing.push(dest);
            return;
        }
        match fs::copy(source, &dest) {
            Ok(_) => report.copied.push(dest),
            Err(e) => report.errors.push((source.to_path_buf(), e.to_string())),
        }
    };
    install_one(src);
    for dep in &closure.resolved {
        install_one(dep);
    }
    report
}

/// Read `path` and inline every `#include "..."` recursively,
/// sandboxed to `base_dir`. Targets that escape the sandbox (absolute
/// paths, `..` above the base, symlinks elsewhere) and references to
/// missing files are replaced with a commented-out marker so the
/// caller -- and the template runtime -- can see what happened without
/// a hard failure.
///
/// Cycles terminate on first re-entry; each file is inlined at most
/// once.
pub fn expand_includes(path: &Path, base_dir: &Path) -> std::io::Result<String> {
    let Ok(canonical_base) = base_dir.canonicalize() else {
        return fs::read_to_string(path);
    };
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out = String::new();
    expand_into(path, &canonical_base, &mut seen, &mut out)?;
    Ok(out)
}

fn expand_into(
    path: &Path,
    canonical_base: &Path,
    seen: &mut HashSet<PathBuf>,
    out: &mut String,
) -> std::io::Result<()> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(canonical) {
        return Ok(());
    }
    let text = fs::read_to_string(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for line in text.split_inclusive('\n') {
        if let Some(target) = extract_include_target(line.trim_end_matches(['\r', '\n'])) {
            match resolve_within(&target, parent, canonical_base) {
                Some(resolved) => {
                    out.push_str(&format!("// hxy: begin #include \"{target}\"\n"));
                    expand_into(&resolved, canonical_base, seen, out)?;
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&format!("// hxy: end #include \"{target}\"\n"));
                }
                None => {
                    out.push_str(&format!("// hxy: skipped sandboxed #include \"{target}\"\n"));
                }
            }
            continue;
        }
        out.push_str(line);
    }
    Ok(())
}

/// List every `.bt` file currently installed in `dir`, sorted by name.
/// Used by the "Uninstall template..." palette mode.
pub fn list_installed_templates(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> =
        read.flatten().map(|e| e.path()).filter(|p| p.extension().and_then(|s| s.to_str()) == Some("bt")).collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_mask_with_multiple_globs() {
        let raw = "File Mask: *.zip,*.jar,*.ear";
        let rest = header_value(raw, "File Mask").unwrap();
        assert_eq!(parse_extensions(rest), vec!["zip", "jar", "ear"]);
    }

    #[test]
    fn strips_inline_ascii_hint_on_id_bytes() {
        let raw = "ID Bytes: 50 4B //PK";
        let rest = header_value(raw, "ID Bytes").unwrap();
        assert_eq!(parse_id_bytes(rest), vec![vec![0x50, 0x4B]]);
    }

    #[test]
    fn parses_multiple_magic_sequences() {
        let raw = "ID Bytes: 89 50 4E 47, FF D8 FF";
        let rest = header_value(raw, "ID Bytes").unwrap();
        assert_eq!(parse_id_bytes(rest), vec![vec![0x89, 0x50, 0x4E, 0x47], vec![0xFF, 0xD8, 0xFF]]);
    }

    #[test]
    fn suggest_prefers_magic_match() {
        let png = TemplateEntry {
            path: PathBuf::from("PNG.bt"),
            name: "PNG.bt".into(),
            extensions: vec!["bin".into()],
            magic: vec![vec![0x89, 0x50, 0x4E, 0x47]],
        };
        let other = TemplateEntry {
            path: PathBuf::from("FOO.bt"),
            name: "FOO.bt".into(),
            extensions: vec!["bin".into()],
            magic: vec![vec![0x00, 0x01]],
        };
        let lib = TemplateLibrary { entries: vec![other, png] };
        let head = [0x89, 0x50, 0x4E, 0x47, 0x0D];
        assert_eq!(lib.suggest(Some("bin"), &head).unwrap().name, "PNG.bt");
    }

    #[test]
    fn parse_includes_tolerates_leading_whitespace_and_quotes() {
        let text = "  #include \"A.bt\"\n#include\t\"B.bt\"\n#include <C.bt>\n#includeoops\n// #include \"D.bt\"\n";
        // Only comment-free preprocessor lines count; the leading `//` form
        // is stripped by `trim_start`, so it gets scanned. Accept that -- a
        // commented-out include is rare and the caller still copies a
        // physical file, not an AST.
        let got = parse_include_directives(text);
        assert!(got.contains(&"A.bt".to_owned()));
        assert!(got.contains(&"B.bt".to_owned()));
        assert!(got.contains(&"C.bt".to_owned()));
        assert!(!got.iter().any(|s| s.contains("oops")));
    }

    #[test]
    fn install_copies_root_and_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("src");
        let dst_dir = tmp.path().join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::write(src_dir.join("Main.bt"), "#include \"Dep.bt\"\n").unwrap();
        std::fs::write(src_dir.join("Dep.bt"), "// nothing\n").unwrap();
        std::fs::write(src_dir.join("Unrelated.bt"), "// not referenced\n").unwrap();

        let report = install_template_with_deps(&src_dir.join("Main.bt"), &dst_dir);
        assert!(report.missing.is_empty(), "missing: {:?}", report.missing);
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert!(dst_dir.join("Main.bt").exists());
        assert!(dst_dir.join("Dep.bt").exists());
        assert!(!dst_dir.join("Unrelated.bt").exists());
    }

    #[test]
    fn expand_includes_sandboxes_escape_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("Evil.bt"), "POISON\n").unwrap();
        std::fs::write(base.join("Main.bt"), "#include \"../outside/Evil.bt\"\n").unwrap();

        let expanded = expand_includes(&base.join("Main.bt"), &base).unwrap();
        assert!(!expanded.contains("POISON"), "sandbox breached: {expanded}");
        assert!(expanded.contains("skipped sandboxed"));
    }

    #[test]
    fn expand_includes_inlines_within_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("Main.bt"), "#include \"Dep.bt\"\nAFTER\n").unwrap();
        std::fs::write(base.join("Dep.bt"), "DEP_CONTENT\n").unwrap();

        let expanded = expand_includes(&base.join("Main.bt"), &base).unwrap();
        assert!(expanded.contains("DEP_CONTENT"), "dep not inlined: {expanded}");
        assert!(expanded.contains("AFTER"));
    }

    #[test]
    fn suggest_falls_back_to_extension_when_no_magic_matches() {
        let zip = TemplateEntry {
            path: PathBuf::from("ZIP.bt"),
            name: "ZIP.bt".into(),
            extensions: vec!["zip".into()],
            magic: vec![vec![0x50, 0x4B]],
        };
        let lib = TemplateLibrary { entries: vec![zip] };
        assert_eq!(lib.suggest(Some("zip"), &[0, 0, 0]).unwrap().name, "ZIP.bt");
        assert!(lib.suggest(Some("unrelated"), &[0, 0]).is_none());
    }
}
