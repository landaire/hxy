//! [`PluginsPanel`]: the dockable plugin manager. Mirrors the egui
//! plugins tab (`crates/hxy/src/panels/plugins.rs`) with native
//! gpui-component widgets, and localizes every string the egui tab
//! hardcodes in English.
//!
//! Three concerns share the panel:
//!
//! - Consent cards, one per loaded plugin whose manifest requests at
//!   least one host capability. Each per-permission [`Switch`] flips a
//!   single field of a fresh [`PermissionGrants`] and routes it through
//!   [`crate::plugins::set_grant`], which persists to the shared
//!   database and reloads the handler registry.
//! - Two filesystem sections (VFS handlers, template runtimes) listing
//!   the installed `.wasm` components with install / rescan / delete /
//!   reveal-in-file-manager affordances against the shared plugin
//!   directories.
//! - The ImHex pattern-library fetch, which emits
//!   [`FetchImhexPatternsRequested`] so the workspace drives the M4a
//!   downloader (it owns the in-flight task and the library refresh).
//!
//! The panel re-renders on any [`PluginHandlersGlobal`] change (a grant
//! toggle or a rescan swaps it) and on [`SettingsGlobal`] change (the
//! pattern fetch records its installed hash there). Filesystem
//! mutations that do not touch a global (a template-runtime install)
//! call `cx.notify()` directly.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::AnyElement;
use gpui::App;
use gpui::Context;
use gpui::ElementId;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::Window;
use gpui::div;
use gpui::component::ActiveTheme;
use gpui::component::button::Button;
use gpui::component::button::ButtonVariants;
use gpui::component::dock::BasePanel;
use gpui::component::dock::Panel;
use gpui::component::dock::PanelEvent;
use gpui::component::h_flex;
use gpui::component::label::Label;
use gpui::component::switch::Switch;
use gpui::component::v_flex;
use hxy_plugin_host::PermissionGrants;
use hxy_plugin_host::Permissions;
use hxy_plugin_host::PluginHandler;
use hxy_plugin_host::PluginKey;

use crate::plugins::PluginHandlersGlobal;
use crate::plugins::PluginLoadFailuresGlobal;
use crate::plugins::user_plugins_dir;
use crate::settings::SettingsGlobal;

/// Stable identifier for layout (de)serialization; must never change.
pub const PLUGINS_PANEL_NAME: &str = "PluginsPanel";

/// Emitted when the user asks to download / update the ImHex pattern
/// library. The workspace owns the in-flight fetch task and the library
/// refresh, so the panel delegates rather than driving the download
/// itself (mirrors the egui tab's `RequestPatternsDownload`).
pub struct FetchImhexPatternsRequested;

/// Which of the two shared plugin directories a filesystem section
/// manages. Each has its own on-disk location and reload path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FsSection {
    /// `hxy:vfs` handler components (`<data_dir>/hxy/plugins`).
    VfsHandlers,
    /// Template-runtime components (`<data_dir>/hxy/template-plugins`).
    TemplateRuntimes,
}

impl FsSection {
    fn dir(self) -> Option<PathBuf> {
        match self {
            FsSection::VfsHandlers => user_plugins_dir(),
            FsSection::TemplateRuntimes => hxy_templates::user_template_plugins_dir(),
        }
    }

    /// Rebuild the registry that consumes this section's directory so a
    /// just-installed or -deleted component takes effect immediately.
    fn reload(self, cx: &mut App) {
        match self {
            FsSection::VfsHandlers => crate::plugins::reload_plugins(cx),
            FsSection::TemplateRuntimes => crate::templates::refresh_runtimes(cx),
        }
    }

    fn heading_key(self) -> &'static str {
        match self {
            FsSection::VfsHandlers => "plugin-vfs-handlers",
            FsSection::TemplateRuntimes => "plugin-template-runtimes",
        }
    }

    fn blurb_key(self) -> &'static str {
        match self {
            FsSection::VfsHandlers => "plugin-vfs-handlers-blurb",
            FsSection::TemplateRuntimes => "plugin-template-runtimes-blurb",
        }
    }

    /// Element-id prefix so the two sections' buttons never collide.
    fn id_prefix(self) -> &'static str {
        match self {
            FsSection::VfsHandlers => "plugins-vfs",
            FsSection::TemplateRuntimes => "plugins-tpl",
        }
    }
}

/// Which permission a consent switch edits.
#[derive(Clone, Debug)]
enum PermKind {
    Persist,
    Commands,
    /// One requested `host:port` pattern.
    Network(String),
}

/// Build the grants that result from flipping `kind` to `value`,
/// starting from the plugin's currently-granted set. Mirrors egui's
/// per-checkbox `next` construction (`panels/plugins.rs`).
fn apply_permission(granted: &Permissions, kind: &PermKind, value: bool) -> PermissionGrants {
    let mut next =
        PermissionGrants { persist: granted.persist, commands: granted.commands, network: granted.network.clone() };
    match kind {
        PermKind::Persist => next.persist = value,
        PermKind::Commands => next.commands = value,
        PermKind::Network(pattern) => {
            if value {
                if !next.network.iter().any(|p| p == pattern) {
                    next.network.push(pattern.clone());
                }
            } else {
                next.network.retain(|p| p != pattern);
            }
        }
    }
    next
}

pub struct PluginsPanel {
    focus_handle: FocusHandle,
    _subs: Vec<Subscription>,
}

impl PluginsPanel {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            // A grant toggle or a rescan swaps the handler registry;
            // the pattern fetch records its hash in the settings blob.
            cx.observe_global::<PluginHandlersGlobal>(|_, cx| cx.notify()),
            cx.observe_global::<SettingsGlobal>(|_, cx| cx.notify()),
        ];
        Self { focus_handle: cx.focus_handle(), _subs: subs }
    }

    /// The consent cards, or `None` when no loaded plugin requests any
    /// host capability (egui renders nothing in that case).
    fn consent_section(&self, handlers: &[Arc<PluginHandler>], cx: &mut Context<Self>) -> Option<AnyElement> {
        let to_show: Vec<&Arc<PluginHandler>> =
            handlers.iter().filter(|p| p.manifest().is_some_and(|m| m.permissions != Permissions::default())).collect();
        if to_show.is_empty() {
            return None;
        }
        let mut section = v_flex()
            .gap_2()
            .child(section_heading(hxy_i18n::t("plugin-permissions-header"), cx))
            .child(Label::new(hxy_i18n::t("plugin-permissions-blurb")).text_color(cx.theme().muted_foreground));
        for plugin in to_show {
            section = section.child(self.consent_card(plugin, cx));
        }
        Some(section.into_any_element())
    }

    fn consent_card(&self, plugin: &Arc<PluginHandler>, cx: &mut Context<Self>) -> AnyElement {
        // Guaranteed by the filter in `consent_section`.
        let Some(manifest) = plugin.manifest() else { return div().into_any_element() };
        let key = plugin.key().clone();
        let granted = plugin.granted().clone();
        let requested = manifest.permissions.clone();
        let name = manifest.plugin.name.clone();

        let mut card =
            v_flex().gap_1().p_2().border_1().border_color(cx.theme().border).rounded(cx.theme().radius).child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(name.clone()))
                    .child(
                        Label::new(hxy_i18n::t_args("plugin-version", &[("version", &manifest.plugin.version)]))
                            .text_color(cx.theme().muted_foreground),
                    ),
            );
        if !manifest.plugin.description.is_empty() {
            card = card.child(Label::new(manifest.plugin.description.clone()));
        }

        if requested.persist {
            card = card.child(self.permission_switch(
                (&key, "persist", 0),
                &key,
                &granted,
                PermKind::Persist,
                granted.persist,
                hxy_i18n::t("plugin-perm-persist"),
                cx,
            ));
        }
        if requested.commands {
            card = card.child(self.permission_switch(
                (&key, "commands", 0),
                &key,
                &granted,
                PermKind::Commands,
                granted.commands,
                hxy_i18n::t("plugin-perm-commands"),
                cx,
            ));
        }
        if !requested.network.is_empty() {
            card = card
                .child(Label::new(hxy_i18n::t("plugin-perm-network-header")).text_color(cx.theme().muted_foreground));
            for (index, pattern) in requested.network.iter().enumerate() {
                let checked = granted.network.iter().any(|p| p == pattern);
                card = card.child(self.permission_switch(
                    (&key, "network", index),
                    &key,
                    &granted,
                    PermKind::Network(pattern.clone()),
                    checked,
                    pattern.clone(),
                    cx,
                ));
            }
        }

        // Wiping only makes sense once state can exist (persist granted),
        // mirroring egui's `granted.persist` gate.
        if granted.persist {
            let name = name.clone();
            card = card.child(
                Button::new(perm_id((&key, "wipe", 0)))
                    .label(hxy_i18n::t("plugin-wipe-state"))
                    .danger()
                    .compact()
                    .on_click(cx.listener(move |_this, _, _window, cx| {
                        crate::plugins::wipe_plugin_state(cx, &name);
                        cx.notify();
                    })),
            );
        }
        card.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn permission_switch(
        &self,
        id: (&PluginKey, &'static str, usize),
        key: &PluginKey,
        granted: &Permissions,
        kind: PermKind,
        checked: bool,
        label: String,
        cx: &mut Context<Self>,
    ) -> Switch {
        let key = key.clone();
        let granted = granted.clone();
        Switch::new(perm_id(id)).checked(checked).label(label).on_click(cx.listener(
            move |_this, value: &bool, _window, cx| {
                let next = apply_permission(&granted, &kind, *value);
                crate::plugins::set_grant(cx, key.clone(), next);
                cx.notify();
            },
        ))
    }

    fn patterns_section(&self, installed_hash: Option<String>, cx: &mut Context<Self>) -> AnyElement {
        let status = match &installed_hash {
            Some(hash) => {
                let short: String = hash.chars().take(12).collect();
                hxy_i18n::t_args("patterns-settings-installed", &[("hash", &short)])
            }
            None => hxy_i18n::t("patterns-settings-not-installed"),
        };
        let label = if installed_hash.is_some() {
            hxy_i18n::t("patterns-settings-update")
        } else {
            hxy_i18n::t("patterns-settings-download-now")
        };
        v_flex()
            .gap_1()
            .child(section_heading(hxy_i18n::t("patterns-settings-title"), cx))
            .child(Label::new(status).text_color(cx.theme().muted_foreground))
            .child(Button::new("plugins-fetch-patterns").label(label).compact().on_click(cx.listener(
                |_this, _, _window, cx| {
                    cx.emit(FetchImhexPatternsRequested);
                },
            )))
            .into_any_element()
    }

    fn fs_section(&self, section: FsSection, cx: &mut Context<Self>) -> AnyElement {
        let mut out = v_flex()
            .gap_1()
            .child(section_heading(hxy_i18n::t(section.heading_key()), cx))
            .child(Label::new(hxy_i18n::t(section.blurb_key())).text_color(cx.theme().muted_foreground));

        let Some(dir) = section.dir() else {
            return out.child(Label::new(hxy_i18n::t("plugin-no-data-dir"))).into_any_element();
        };
        let prefix = section.id_prefix();

        let open_dir = dir.clone();
        out = out.child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Label::new(dir.display().to_string()).text_color(cx.theme().muted_foreground))
                .child(
                    Button::new(section_id(prefix, "open"))
                        .label(hxy_i18n::t("plugin-open-in-file-manager"))
                        .compact()
                        .on_click(move |_, _window, _cx| {
                            let _ = open_in_file_manager(&open_dir);
                        }),
                ),
        );

        let install_dir = dir.clone();
        out = out.child(
            h_flex()
                .gap_2()
                .child(
                    Button::new(section_id(prefix, "install")).label(hxy_i18n::t("plugin-install")).compact().on_click(
                        cx.listener(move |_this, _, window, cx| {
                            let dir = install_dir.clone();
                            cx.spawn_in(window, async move |this, cx| {
                                let picked = rfd::AsyncFileDialog::new()
                                    .add_filter(hxy_i18n::t("plugin-wasm-filter"), &["wasm"])
                                    .pick_file()
                                    .await;
                                let Some(handle) = picked else { return };
                                let src = handle.path().to_path_buf();
                                let _ = this.update(cx, |_this, cx| match install_to(&dir, &src) {
                                    Ok(()) => {
                                        section.reload(cx);
                                        cx.notify();
                                    }
                                    Err(err) => tracing::warn!(%err, dir = %dir.display(), "install plugin component"),
                                });
                            })
                            .detach();
                        }),
                    ),
                )
                .child(
                    Button::new(section_id(prefix, "rescan")).label(hxy_i18n::t("plugin-rescan")).compact().on_click(
                        cx.listener(move |_this, _, _window, cx| {
                            section.reload(cx);
                            cx.notify();
                        }),
                    ),
                ),
        );

        let files = list_wasm_files(&dir);
        if files.is_empty() {
            return out
                .child(Label::new(hxy_i18n::t("plugin-none-installed")).text_color(cx.theme().muted_foreground))
                .into_any_element();
        }
        // Only the VFS-handler loader reports per-file failures; the
        // template section has its own loader and no such global.
        let failures = if section == FsSection::VfsHandlers {
            cx.try_global::<PluginLoadFailuresGlobal>().map(|g| g.0.clone()).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mono = cx.theme().mono_font_family.clone();
        for (index, path) in files.into_iter().enumerate() {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            // Only a failure is flagged: a loaded handler carries no
            // source path to match against, so the absence of a failure
            // is not proof of a successful load (a whole-scan abort
            // leaves the failures list empty). No badge means "not known
            // to have failed", never an affirmative "loaded".
            let failure = failures.iter().find(|f| f.path == path).map(|f| f.message.clone());
            let del_path = path.clone();
            let mut header = h_flex()
                .gap_2()
                .items_center()
                .child(div().flex_1().font_family(mono.clone()).child(name));
            if failure.is_some() {
                header = header.child(Label::new(hxy_i18n::t("plugin-load-failed")).text_color(cx.theme().danger));
            }
            header = header.child(
                Button::new(section_row_id(prefix, index))
                    .label(hxy_i18n::t("plugin-delete"))
                    .compact()
                    .danger()
                    .on_click(cx.listener(move |_this, _, _window, cx| {
                        if fs::remove_file(&del_path).is_ok() {
                            section.reload(cx);
                            cx.notify();
                        }
                    })),
            );
            let mut row = v_flex().gap_1().child(header);
            // The plugin's own error detail (e.g. a WIT signature
            // mismatch) is not localized: it is loader-authored text.
            if let Some(message) = failure {
                row = row.child(Label::new(message).text_color(cx.theme().danger).text_xs());
            }
            out = out.child(row);
        }
        out.into_any_element()
    }
}

/// Stable element id for a per-permission control: full plugin identity
/// (name, version, and a content-hash prefix so two components sharing a
/// name and version still get distinct ids) plus a permission tag and
/// index (network patterns share the tag).
fn perm_id(id: (&PluginKey, &'static str, usize)) -> ElementId {
    let (key, tag, index) = id;
    let hash: String = key.sha256.chars().take(12).collect();
    ElementId::from(SharedString::from(format!("plugins-perm-{}-{}-{}-{}-{}", key.name, key.version, hash, tag, index)))
}

fn section_id(prefix: &'static str, tag: &'static str) -> ElementId {
    ElementId::from(SharedString::from(format!("{prefix}-{tag}")))
}

fn section_row_id(prefix: &'static str, index: usize) -> ElementId {
    ElementId::from(SharedString::from(format!("{prefix}-file-{index}")))
}

fn section_heading(text: String, cx: &App) -> impl IntoElement {
    div().mt_2().pb_1().border_b_1().border_color(cx.theme().border).font_weight(gpui::FontWeight::SEMIBOLD).child(text)
}

/// The `.wasm` components in `dir`, sorted. A missing or unreadable
/// directory reads as empty. Ported verbatim from egui's plugins tab.
fn list_wasm_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = read
        .filter_map(|entry| entry.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wasm"))
        .collect();
    out.sort();
    out
}

/// Copy `src` into `dir` under its own filename, creating `dir` if
/// absent. Ported from egui's plugins tab.
fn install_to(dir: &Path, src: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let filename = src
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "source has no filename"))?;
    fs::copy(src, dir.join(filename))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("open").arg(path).status().map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(path).status().map(|_| ())
}

#[cfg(target_os = "windows")]
fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("explorer").arg(path).status().map(|_| ())
}

impl BasePanel for PluginsPanel {
    fn panel_name(&self) -> &'static str {
        PLUGINS_PANEL_NAME
    }
}

impl Panel for PluginsPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t("tab-plugins"))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t("tab-plugins")))
    }
}

impl Focusable for PluginsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PluginsPanel {}
impl EventEmitter<FetchImhexPatternsRequested> for PluginsPanel {}

impl Render for PluginsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handlers = cx.try_global::<PluginHandlersGlobal>().map(|g| g.0.clone()).unwrap_or_default();
        let installed_hash = crate::settings::settings(cx).imhex_patterns.installed_hash.clone();

        let mut root = v_flex()
            .id("plugins-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_2()
            .child(Label::new(hxy_i18n::t("plugin-intro")).text_color(cx.theme().muted_foreground));
        if let Some(consent) = self.consent_section(&handlers, cx) {
            root = root.child(consent);
        }
        root.child(self.patterns_section(installed_hash, cx))
            .child(self.fs_section(FsSection::VfsHandlers, cx))
            .child(self.fs_section(FsSection::TemplateRuntimes, cx))
    }
}

#[cfg(test)]
mod tests {
    use hxy_plugin_host::PermissionGrants;
    use hxy_plugin_host::Permissions;

    use super::*;

    #[test]
    fn toggle_persist_on_builds_grant_with_persist_set() {
        let granted = Permissions { persist: false, commands: true, network: vec![] };
        let next = apply_permission(&granted, &PermKind::Persist, true);
        assert!(next.persist, "persist flips on");
        assert!(next.commands, "other permissions untouched");
    }

    #[test]
    fn toggle_commands_off_clears_only_commands() {
        let granted = Permissions { persist: true, commands: true, network: vec!["a:1".into()] };
        let next = apply_permission(&granted, &PermKind::Commands, false);
        assert!(!next.commands);
        assert!(next.persist);
        assert_eq!(next.network, vec!["a:1".to_string()]);
    }

    #[test]
    fn toggle_network_pattern_adds_and_removes_without_duplicating() {
        let granted = Permissions { persist: false, commands: false, network: vec!["keep:1".into()] };
        let added = apply_permission(&granted, &PermKind::Network("new:2".into()), true);
        assert_eq!(added.network, vec!["keep:1".to_string(), "new:2".to_string()]);

        // Re-adding an already-present pattern is a no-op (no duplicate).
        let granted2 = PermissionGrants { persist: false, commands: false, network: vec!["dup:1".into()] };
        let granted2 =
            Permissions { persist: granted2.persist, commands: granted2.commands, network: granted2.network };
        let re = apply_permission(&granted2, &PermKind::Network("dup:1".into()), true);
        assert_eq!(re.network, vec!["dup:1".to_string()]);

        let removed = apply_permission(&granted, &PermKind::Network("keep:1".into()), false);
        assert!(removed.network.is_empty());
    }
}
