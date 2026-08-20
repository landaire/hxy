//! Download + extraction of the upstream `WerWolv/ImHex-Patterns`
//! corpus, shared by the hxy frontends.
//!
//! [`run_fetch`] is synchronous and framework-free; each frontend
//! wraps it in its own background executor (egui: worker thread +
//! `UiInbox`, gpui: `cx.background_spawn`) and renders the
//! [`FetchStatus`] updates its `report` callback emits.

use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::io;
use std::io::Cursor;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

/// Public URL for a tarball of the upstream master branch. GitHub
/// codeload returns the same content that the web "Download ZIP"
/// button serves.
const PATTERNS_ZIP_URL: &str = "https://codeload.github.com/WerWolv/ImHex-Patterns/zip/refs/heads/master";

/// Cap so a misconfigured proxy or hostile mirror can't make us
/// allocate gigabytes. The real corpus sits around 4-5 MB; 64 MiB
/// covers any realistic future growth.
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// Top-level directories inside the upstream repo we actually need
/// at runtime. Everything else (`tests/` alone is ~32 MB of fixture
/// data, plus ImHex-only `encodings/` / `magic/` / `nodes/` /
/// `plugins/` / `themes/` / `yara/` ...) is dropped during
/// extraction so the on-disk install stays in the low single-digit
/// MB range.
const KEEP_TOP_DIRS: &[&str] = &["patterns", "includes"];

#[derive(Clone, Debug)]
pub enum FetchStatus {
    /// Download progress in bytes. Only emitted when the server
    /// returns a Content-Length so the UI knows the denominator.
    Progress { downloaded: u64, total: Option<u64> },
    /// Download finished successfully. Carries the SHA-256 of the
    /// raw zip we extracted -- hosts store this so the next periodic
    /// check can detect server-side drift in O(1).
    Success { sha256_hex: String, extracted_root: PathBuf },
    /// Anything that went wrong end-to-end -- network, decompression,
    /// disk, you name it. The string is meant for the UI; structured
    /// errors don't pay off here because no caller branches on them.
    Failed { message: String },
}

/// Fingerprint a previously-extracted corpus on disk by re-hashing
/// every file under `dir`. Returns `None` if the directory is empty
/// or unreadable -- callers treat that as "no install to compare
/// against, prompt to download".
pub fn fingerprint_existing(dir: &Path) -> Option<String> {
    if !dir.is_dir() {
        return None;
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let read = fs::read_dir(&d).ok()?;
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                entries.push(path);
            }
        }
    }
    if entries.is_empty() {
        return None;
    }
    // Sort so the fingerprint is stable across filesystem orderings.
    entries.sort();
    let mut hasher = Sha256::new();
    for path in &entries {
        let rel = path.strip_prefix(dir).unwrap_or(path);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        if let Ok(bytes) = fs::read(path) {
            hasher.update(&bytes);
        }
        hasher.update([0u8]);
    }
    Some(hex_encode(&hasher.finalize()))
}

/// Download the corpus zip and extract it under `dest`, reporting
/// progress through `report`. Blocking; run it off the UI thread.
pub fn run_fetch(dest: &Path, mut report: impl FnMut(FetchStatus)) -> Result<(String, PathBuf), String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("hxy/", env!("CARGO_PKG_VERSION"), " (imhex-patterns-fetch)"))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;
    let mut response = client
        .get(PATTERNS_ZIP_URL)
        .send()
        .map_err(|e| format!("connect: {e}"))?
        .error_for_status()
        .map_err(|e| format!("HTTP error: {e}"))?;
    let total = response.content_length();
    let mut downloaded: u64 = 0;
    let mut buf: Vec<u8> = Vec::with_capacity(total.unwrap_or(4 * 1024 * 1024) as usize);
    let mut chunk = [0u8; 32 * 1024];
    loop {
        let n = match response.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(format!("read body: {e}")),
        };
        downloaded = downloaded.saturating_add(n as u64);
        if downloaded > MAX_DOWNLOAD_BYTES {
            return Err(format!("download exceeded {} MiB cap; aborting", MAX_DOWNLOAD_BYTES / (1024 * 1024)));
        }
        buf.extend_from_slice(&chunk[..n]);
        report(FetchStatus::Progress { downloaded, total });
    }
    // Hash before extraction so we can fingerprint even if the
    // extraction fails halfway -- gives the user something to
    // diff next launch.
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    let sha = hex_encode(&hasher.finalize());

    extract_zip(&buf, dest).map_err(|e| format!("extract: {e}"))?;
    Ok((sha, dest.to_path_buf()))
}

fn extract_zip(bytes: &[u8], dest: &Path) -> io::Result<()> {
    // Wipe any previous corpus so a stripped-down upstream release
    // doesn't leave orphaned files behind. Cheap because the corpus
    // is small.
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::create_dir_all(dest)?;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| io::Error::other(format!("open zip: {e}")))?;
    // The codeload zip wraps everything under a top-level
    // `ImHex-Patterns-master/` directory. Strip it so the resolver
    // sees `patterns/foo.hexpat` directly under `dest`.
    let strip_prefix = detect_top_dir(&mut archive);
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| io::Error::other(format!("zip entry {i}: {e}")))?;
        let entry_path = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let stripped = match (&strip_prefix, entry_path.strip_prefix(strip_prefix.as_deref().unwrap_or(Path::new(""))))
        {
            (Some(_), Ok(p)) => p.to_path_buf(),
            _ => entry_path,
        };
        if stripped.as_os_str().is_empty() {
            continue;
        }
        // Skip everything whose top-level directory we don't ship
        // at runtime. Keeps the on-disk install around 2-3 MB
        // instead of the ~35 MB the full upstream tarball expands
        // to (most of which is `tests/` fixture data).
        let top = stripped.components().next().and_then(|c| c.as_os_str().to_str());
        match top {
            Some(name) if KEEP_TOP_DIRS.contains(&name) => {}
            _ => continue,
        }
        let out_path = dest.join(&stripped);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut writer = fs::File::create(&out_path)?;
        io::copy(&mut entry, &mut writer)?;
    }
    Ok(())
}

fn detect_top_dir(archive: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Option<PathBuf> {
    let first = archive.by_index(0).ok()?;
    let name = first.enclosed_name()?;
    name.components().next().map(|c| PathBuf::from(c.as_os_str()))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Where the downloaded corpus lives, shared by every frontend so a
/// fetch from one app is visible to the other.
pub fn install_dir() -> Option<PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(crate::APP_NAME).join("imhex-patterns"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_changes_on_content_change() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let h1 = fingerprint_existing(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), b"world").unwrap();
        let h2 = fingerprint_existing(dir.path()).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn fingerprint_empty_dir_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fingerprint_existing(dir.path()).is_none());
    }
}
