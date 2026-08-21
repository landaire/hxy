//! Framework-neutral single-instance IPC and CLI parsing for hxy.
//!
//! Desktop-only by construction (interprocess). The egui and gpui
//! frontends each build a thin delivery adapter on top of the neutral
//! socket listener ([`socket::start_server`]) and clap parser
//! ([`cli::Cli`]) here.

pub mod cli;
pub mod socket;

/// Executable name surfaced by clap's generated `--help` / `--version`
/// usage. Both frontends ship the same binary name.
pub const APP_NAME: &str = "hxy";
