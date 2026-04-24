//! Host-facing types for the plugin `commands` interface. These
//! mirror the wit-bindgen-generated records but live as plain Rust
//! types so the rest of the host (and the egui app on top of it)
//! never has to import the bindgen path.
//!
//! Conversion happens in [`PluginCommand::from_wit`] /
//! [`InvokeOutcome::from_wit`]; the wit types come from
//! `bindings::handler_world::exports::hxy::vfs::commands::*`.

use crate::bindings::handler_world::exports::hxy::vfs::commands as wit;

/// One palette entry contributed by a plugin. The host prefixes
/// `label` with the plugin's name when rendering ("xeedee:
/// Connect") so the entries are visually grouped without the
/// plugin having to know its own name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginCommand {
    pub id: String,
    pub label: String,
    pub subtitle: Option<String>,
    pub icon: Option<String>,
    /// Cosmetic hint that activating this entry will open a sub-
    /// palette. Does not affect behavior -- the actual outcome is
    /// determined by [`InvokeOutcome`].
    pub has_children: bool,
}

impl PluginCommand {
    pub(crate) fn from_wit(c: wit::Command) -> Self {
        Self {
            id: c.id,
            label: c.label,
            subtitle: c.subtitle,
            icon: c.icon,
            has_children: c.has_children,
        }
    }
}

/// Result of [`PluginHandler::invoke_command`](crate::PluginHandler::invoke_command).
/// Drives what the host does next after the user picks a plugin
/// command from the palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvokeOutcome {
    /// Plugin already executed whatever side effect it wanted; the
    /// host should close the palette.
    Done,
    /// Plugin returned a sub-menu; the host should push a new
    /// palette mode populated with these commands.
    Cascade(Vec<PluginCommand>),
    /// Plugin asked the host to open a tab backed by a token. The
    /// token is opaque to the host -- it is whatever the plugin
    /// generated (typically via [`crate::fresh_token`]) and will be
    /// handed back to the plugin when the host materializes the
    /// mount.
    Mount(MountRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRequest {
    pub token: String,
    pub title: String,
}

impl InvokeOutcome {
    pub(crate) fn from_wit(r: wit::InvokeResult) -> Self {
        match r {
            wit::InvokeResult::Done => Self::Done,
            wit::InvokeResult::Cascade(list) => Self::Cascade(list.into_iter().map(PluginCommand::from_wit).collect()),
            wit::InvokeResult::Mount(req) => Self::Mount(MountRequest { token: req.token, title: req.title }),
        }
    }
}
