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
//! Only `.bt` is recognised today — the comment style is idiomatic to
//! 010 templates. Other runtimes can ship their own detectors later.
//!
//! Only the header is parsed; the body is handed verbatim to the
//! runtime when the user actually runs the template.

#![cfg(not(target_arch = "wasm32"))]

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
    /// Extension globs declared in the header (`*.zip` → `zip`). Lower-
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
    /// matches — a `.zip` extension on a file that starts with `PK`
    /// wins over a `.zip`-declared template that requires a different
    /// magic.
    pub fn suggest(&self, extension: Option<&str>, head_bytes: &[u8]) -> Option<&TemplateEntry> {
        let ext_lower = extension.map(str::to_ascii_lowercase);
        let mut ext_candidate: Option<&TemplateEntry> = None;
        for entry in &self.entries {
            let ext_match = ext_lower
                .as_ref()
                .is_some_and(|e| entry.extensions.iter().any(|x| x == e));
            let magic_match =
                !entry.magic.is_empty() && entry.magic.iter().any(|m| head_bytes.starts_with(m));
            if magic_match && (entry.extensions.is_empty() || ext_match) {
                return Some(entry);
            }
            if ext_match && ext_candidate.is_none() {
                ext_candidate = Some(entry);
            }
        }
        ext_candidate
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

/// `"File Mask: *.zip"` → `Some("*.zip")` for a given field name.
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

/// `"*.zip,*.jar"` → `["zip", "jar"]`. Strips trailing inline `//...`
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

/// `"50 4B //PK"` → `[[0x50, 0x4B]]`. Multiple magic sequences
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
        assert_eq!(
            parse_id_bytes(rest),
            vec![vec![0x89, 0x50, 0x4E, 0x47], vec![0xFF, 0xD8, 0xFF]]
        );
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
