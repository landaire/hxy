//! Plugin manager tab: lists `.wasm` components in the two user plugin
//! directories (VFS handlers + template runtimes), exposes per-plugin
//! consent toggles, and provides install / delete / rescan
//! affordances. The tab reads the filesystem directly to discover
//! files; the actual `VfsRegistry` rebuild and consent persistence
//! is deferred to `HxyApp` via the [`PluginsEvent`] channel.

#![cfg(not(target_arch = "wasm32"))]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use hxy_plugin_host::PermissionGrants;
use hxy_plugin_host::PluginHandler;
use hxy_plugin_host::PluginKey;

/// Events the tab emits back to the app. The tab itself doesn't own
/// the registry; `HxyApp` drains these after the dock render and
/// reloads / relinks plugins as needed.
pub enum PluginsEvent {
    /// User changed the filesystem (install or delete). The app
    /// should rebuild both the VFS registry and template runtime list.
    Rescan,
    /// User toggled a permission for a specific plugin. The app
    /// updates `PersistedState::plugin_grants`, persists, and
    /// reloads plugins so the linker reflects the new grant set.
    SetGrant { key: PluginKey, grants: PermissionGrants },
    /// User asked to wipe the plugin's persisted state blob. The
    /// app calls `clear` on the configured `StateStore`.
    WipeState { plugin_name: String },
}

pub fn show(
    ui: &mut egui::Ui,
    handlers_dir: Option<&PathBuf>,
    templates_dir: Option<&PathBuf>,
    plugin_handlers: &[Arc<PluginHandler>],
) -> Vec<PluginsEvent> {
    let mut events = Vec::new();
    ui.heading("Plugins");
    ui.label("Drop compiled WASM components into these directories to load them at startup.");
    ui.separator();

    render_consent_section(ui, plugin_handlers, &mut events);
    ui.add_space(12.0);
    render_section(
        ui,
        "VFS handlers",
        "Mount byte sources as a browseable VFS tree.",
        handlers_dir,
        &mut events,
    );
    ui.add_space(12.0);
    render_section(
        ui,
        "Template runtimes",
        "Execute binary templates (e.g. 010 Editor `.bt`) against a data source.",
        templates_dir,
        &mut events,
    );
    events
}

fn render_consent_section(
    ui: &mut egui::Ui,
    plugin_handlers: &[Arc<PluginHandler>],
    events: &mut Vec<PluginsEvent>,
) {
    // Only surface plugins that ship a manifest with at least one
    // declared permission. Manifest-less plugins, and plugins that
    // request nothing, have nothing to grant.
    let to_show: Vec<&Arc<PluginHandler>> = plugin_handlers
        .iter()
        .filter(|p| p.manifest().is_some_and(|m| m.permissions != hxy_plugin_host::Permissions::default()))
        .collect();
    if to_show.is_empty() {
        return;
    }
    ui.heading("Permissions");
    ui.weak("Plugins request the host capabilities they need; you grant or revoke them here.");
    for plugin in to_show {
        render_consent_card(ui, plugin, events);
        ui.add_space(8.0);
    }
}

fn render_consent_card(
    ui: &mut egui::Ui,
    plugin: &Arc<PluginHandler>,
    events: &mut Vec<PluginsEvent>,
) {
    // Manifest is guaranteed by the filter in render_consent_section;
    // unwrap-via-let-else here keeps the flow linear.
    let Some(manifest) = plugin.manifest() else { return };
    let key = plugin.key().clone();
    let granted = plugin.granted();
    let requested = &manifest.permissions;

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.strong(&manifest.plugin.name);
            ui.weak(format!("v{}", manifest.plugin.version));
        });
        if !manifest.plugin.description.is_empty() {
            ui.label(&manifest.plugin.description);
        }

        // Snapshot the grant set so toggles can collectively diff
        // against the original. We emit an event only if something
        // actually changed -- avoids saving + reloading the world
        // on every frame.
        let mut next = PermissionGrants {
            persist: granted.persist,
            commands: granted.commands,
            network: granted.network.clone(),
        };
        let mut changed = false;
        if requested.persist
            && ui.checkbox(&mut next.persist, "Persist (remember per-plugin state across sessions)").changed()
        {
            changed = true;
        }
        if requested.commands
            && ui
                .checkbox(&mut next.commands, "Commands (contribute entries to the command palette)")
                .changed()
        {
            changed = true;
        }
        if !requested.network.is_empty() {
            ui.label("Network: outbound TCP allowed for these patterns:");
            // One checkbox per requested pattern. The grant Vec
            // is a subset of the requested list, so toggling
            // adds / removes the pattern from `next.network`.
            for pattern in &requested.network {
                let mut checked = next.network.iter().any(|p| p == pattern);
                if ui.checkbox(&mut checked, pattern).changed() {
                    if checked {
                        next.network.push(pattern.clone());
                    } else {
                        next.network.retain(|p| p != pattern);
                    }
                    changed = true;
                }
            }
        }
        if changed {
            events.push(PluginsEvent::SetGrant { key: key.clone(), grants: next });
        }

        if granted.persist
            && ui.button("Wipe stored state").clicked()
        {
            events.push(PluginsEvent::WipeState { plugin_name: manifest.plugin.name.clone() });
        }
    });
}

fn render_section(
    ui: &mut egui::Ui,
    heading: &str,
    blurb: &str,
    dir: Option<&PathBuf>,
    events: &mut Vec<PluginsEvent>,
) {
    ui.heading(heading);
    ui.weak(blurb);

    let Some(dir) = dir else {
        ui.weak("Could not resolve the user data directory on this system.");
        return;
    };
    ui.horizontal(|ui| {
        ui.label(format!("{}", dir.display()));
        if ui.small_button("Open").clicked() {
            let _ = open_in_file_manager(dir);
        }
    });

    ui.horizontal(|ui| {
        if ui.button("Install...").clicked()
            && let Some(picked) = rfd::FileDialog::new().add_filter("WASM component", &["wasm"]).pick_file()
            && install_to(dir, &picked).is_ok()
        {
            events.push(PluginsEvent::Rescan);
        }
        if ui.button("Rescan").clicked() {
            events.push(PluginsEvent::Rescan);
        }
    });

    let files = list_wasm_files(dir);
    if files.is_empty() {
        ui.weak("No plugins installed.");
        return;
    }
    egui::Grid::new(("hxy-plugins-grid", heading)).num_columns(2).striped(true).show(ui, |ui| {
        for path in files {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            ui.label(egui::RichText::new(name).monospace());
            if ui.small_button("Delete").clicked() && fs::remove_file(&path).is_ok() {
                events.push(PluginsEvent::Rescan);
            }
            ui.end_row();
        }
    });
}

fn list_wasm_files(dir: &PathBuf) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = read
        .filter_map(|entry| entry.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wasm"))
        .collect();
    out.sort();
    out
}

fn install_to(dir: &PathBuf, src: &PathBuf) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let filename = src
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "source has no filename"))?;
    let dest = dir.join(filename);
    fs::copy(src, &dest)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_in_file_manager(path: &PathBuf) -> std::io::Result<()> {
    std::process::Command::new("open").arg(path).status().map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_in_file_manager(path: &PathBuf) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(path).status().map(|_| ())
}

#[cfg(target_os = "windows")]
fn open_in_file_manager(path: &PathBuf) -> std::io::Result<()> {
    std::process::Command::new("explorer").arg(path).status().map(|_| ())
}
