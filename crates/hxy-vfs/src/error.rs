use std::io;

use thiserror::Error;

/// Errors a handler may raise while mounting or servicing a VFS. Kept
/// typed so UI code can branch on the failure (e.g. to show a distinct
/// message for unsupported compression vs. corrupt archive).
#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("underlying byte source I/O failed")]
    SourceIo(#[source] io::Error),

    #[error("archive is malformed: {0}")]
    Malformed(String),

    #[error("archive uses a feature this plugin doesn't support: {0}")]
    Unsupported(String),

    #[error("plugin internal error: {0}")]
    Internal(String),
}
