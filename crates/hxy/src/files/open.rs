//! File open flow: native file picker, dropped paths, VFS-entry
//! reads. Drives [`HxyApp::request_open_filesystem`] for the
//! filesystem case.

#![cfg(not(target_arch = "wasm32"))]

use hxy_vfs::TabSource;

use crate::app::HxyApp;
use crate::files::FileOpenError;

pub fn handle_open_file(app: &mut HxyApp) {
    match pick_file() {
        Ok((name, path)) => {
            app.request_open_filesystem(name, path);
        }
        Err(FileOpenError::Cancelled) => {}
        Err(e) => {
            tracing::warn!(error = %e, "open file");
        }
    }
}

/// Show the OS file picker. Returns the picked path (and a
/// display name derived from its file_name), or `Cancelled`
/// when the user dismissed the dialog. Doesn't read any
/// bytes -- that happens later through the streaming open
/// path.
pub fn pick_file() -> Result<(String, std::path::PathBuf), FileOpenError> {
    let Some(path) = rfd::FileDialog::new().pick_file() else {
        return Err(FileOpenError::Cancelled);
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    Ok((name, path))
}

/// Build a `FileOpenError::Read` for the case where a tab's parent
/// (workspace mount or plugin mount) has been closed before the
/// child entry could be opened.
pub fn parent_missing(parent: &TabSource) -> FileOpenError {
    FileOpenError::Read {
        path: std::path::PathBuf::from(format!("{parent:?}")),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "parent tab / mount not available"),
    }
}

/// Read every byte under `path` out of a `FileSystem` impl. Used
/// when opening a child entry of a workspace / plugin mount as its
/// own tab.
pub fn read_vfs_entry(fs: &dyn hxy_vfs::vfs::FileSystem, path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fs.open_file(path).map_err(|e| std::io::Error::other(format!("open {path}: {e}")))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}
