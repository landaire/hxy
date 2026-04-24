//! Command-line argument parsing for the hxy binary.
//!
//! Kept tiny on purpose -- the only thing the CLI accepts is a list
//! of file paths to open. If you need more, add it here and route it
//! through [`HxyApp`](crate::HxyApp). Single-instance forwarding
//! lives in [`crate::ipc`]; this module is parse-only.

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;

use clap::Parser;

/// `hxy [FILES]...` -- positional file paths get opened in new tabs
/// when the GUI starts (or are forwarded to an already-running
/// instance via [`crate::ipc`]).
#[derive(Parser, Debug)]
#[command(name = crate::APP_NAME, version, about = "A hex editor", long_about = None)]
pub struct Cli {
    /// Files to open. Each becomes its own tab. Relative paths are
    /// resolved against the current working directory before the
    /// process exits, so a forwarded path opens the right file even
    /// when the running instance was launched from a different CWD.
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }

    /// Resolve every path to an absolute form against the current
    /// working directory and drop entries that don't exist on disk.
    /// Run before forwarding so the receiving process doesn't depend
    /// on this process's CWD, and so a typo doesn't quietly become a
    /// "file not found" tab in the running instance.
    pub fn resolved_files(&self) -> Vec<PathBuf> {
        let cwd = std::env::current_dir().ok();
        self.files
            .iter()
            .filter_map(|p| {
                let abs = if p.is_absolute() {
                    p.clone()
                } else {
                    cwd.as_ref().map(|c| c.join(p)).unwrap_or_else(|| p.clone())
                };
                match std::fs::canonicalize(&abs) {
                    Ok(canonical) => Some(canonical),
                    Err(e) => {
                        tracing::warn!(error = %e, path = %abs.display(), "skipping CLI path that didn't resolve");
                        None
                    }
                }
            })
            .collect()
    }
}
