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
        Self { id: c.id, label: c.label, subtitle: c.subtitle, icon: c.icon, has_children: c.has_children }
    }
}

/// Result of [`PluginHandler::invoke_command`](crate::PluginHandler::invoke_command)
/// or [`PluginHandler::respond_to_prompt`](crate::PluginHandler::respond_to_prompt).
/// Drives what the host does next after the user picks a plugin
/// command from the palette (or answers a prompt the plugin
/// previously raised).
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
    /// Plugin needs a string from the user before it can decide
    /// the next outcome. The host renders the request as a palette
    /// argument-style prompt (matching Go-To Offset / Select Range)
    /// and routes the typed answer back via
    /// [`PluginHandler::respond_to_prompt`] using the *same* command
    /// id the original `invoke` carried, so the plugin can
    /// correlate against its own state when chaining.
    Prompt(PromptRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRequest {
    pub token: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRequest {
    /// Rendered as the palette's input hint, so plugins should
    /// phrase it like the existing built-in hints (`Token name`,
    /// `Xbox IP address`, ...) -- short, no terminating
    /// punctuation.
    pub title: String,
    /// Optional pre-fill for the input. `None` leaves the palette
    /// query empty.
    pub default_value: Option<String>,
}

impl InvokeOutcome {
    pub(crate) fn from_wit(r: wit::InvokeResult) -> Self {
        match r {
            wit::InvokeResult::Done => Self::Done,
            wit::InvokeResult::Cascade(list) => Self::Cascade(list.into_iter().map(PluginCommand::from_wit).collect()),
            wit::InvokeResult::Mount(req) => Self::Mount(MountRequest { token: req.token, title: req.title }),
            wit::InvokeResult::Prompt(req) => {
                Self::Prompt(PromptRequest { title: req.title, default_value: req.default_value })
            }
        }
    }
}
