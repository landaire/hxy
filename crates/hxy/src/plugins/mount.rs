//! Workspace + plugin-mount integration: turning a file into a
//! workspace, installing plugin VFS mounts as dock tabs, and the
//! retry-on-failure flow when a plugin mount disconnects.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;

use crate::app::ConsoleSeverity;
use crate::app::HxyApp;
use crate::app::PendingVfsOpen;
use crate::tabs::Tab;

/// Egui temp-data key for the retry queue populated by
/// [`render_failed_mount_placeholder`] and drained by
/// [`drain_pending_mount_retries`].
pub const PENDING_MOUNT_RETRY_KEY: &str = "hxy_pending_mount_retry";

/// Mount the active file's bytes through its detected VFS handler
/// and swap the resulting `Tab::Workspace` in for the original
/// `Tab::File`. When the active tab is already a workspace, just
/// re-show the VFS tree sub-tab if the user closed it.
pub fn mount_active_file(app: &mut HxyApp) {
    if let Some(workspace_id) = crate::app::active_workspace_id(app) {
        ensure_vfs_tree_visible(app, workspace_id);
        return;
    }
    let Some(file_id) = crate::app::active_file_id(app) else { return };
    let Some(file) = app.files.get(&file_id) else { return };
    let Some(handler) = file.detected_handler.clone() else { return };
    let source = file.editor.source().clone();
    let mount = match handler.mount(source) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tracing::warn!(error = %e, handler = handler.name(), "mount vfs");
            return;
        }
    };
    let workspace_id = app.spawn_workspace(file_id, mount);

    if let Some(path) = app.dock.find_tab(&Tab::File(file_id)) {
        let _ = app.dock.remove_tab(path);
    }
    app.dock.push_to_focused_leaf(Tab::Workspace(workspace_id));
    if let Some(path) = app.dock.find_tab(&Tab::Workspace(workspace_id)) {
        let _ = app.dock.set_active_tab(path);
        app.dock.set_focused_node_and_surface(path.node_path());
    }

    if let Some(source) = app.files.get(&file_id).and_then(|f| f.source_kind.clone()) {
        let mut g = app.state.write();
        if let Some(entry) = g.open_tabs.iter_mut().find(|t| t.source == source) {
            entry.as_workspace = true;
        }
    }
}

/// Re-add `WorkspaceTab::VfsTree` to the workspace's inner dock if the
/// user previously closed it. No-op when the tree is already present.
pub fn ensure_vfs_tree_visible(app: &mut HxyApp, workspace_id: crate::files::WorkspaceId) {
    let Some(workspace) = app.workspaces.get_mut(&workspace_id) else { return };
    let already_present = workspace.dock.iter_all_tabs().any(|(_, t)| matches!(t, crate::files::WorkspaceTab::VfsTree));
    if already_present {
        return;
    }
    workspace.dock.main_surface_mut().split_left(
        egui_dock::NodeIndex::root(),
        0.3,
        vec![crate::files::WorkspaceTab::VfsTree],
    );
}

/// Render a `Tab::PluginMount`. The whole tab body is either the
/// VFS tree (when the mount is `MountStatus::Ready`) or a
/// plugin-supplied error placeholder + optional retry button (when
/// `MountStatus::Failed`). Tree clicks queue a
/// `PendingVfsOpen::PluginMount`; retry clicks queue a `MountId`
/// under [`PENDING_MOUNT_RETRY_KEY`].
pub fn render_plugin_mount_tab(
    ui: &mut egui::Ui,
    mount_id: crate::files::MountId,
    plugin: &crate::files::MountedPlugin,
    expanded: &mut Vec<String>,
) {
    let mount = match &plugin.status {
        crate::files::MountStatus::Ready(m) => m,
        crate::files::MountStatus::Failed { message, retry_label } => {
            render_failed_mount_placeholder(ui, mount_id, &plugin.display_name, message, retry_label.as_deref());
            return;
        }
    };
    let scope = egui::Id::new(("hxy-plugin-mount-vfs", mount_id.get()));
    let events = crate::panels::vfs::show(ui, scope, &*mount.fs, expanded);
    let mut to_open: Vec<String> = Vec::new();
    for e in events {
        let crate::panels::vfs::VfsPanelEvent::OpenEntry(path) = e;
        to_open.push(path);
    }
    if !to_open.is_empty() {
        ui.ctx().data_mut(|d| {
            let queue: &mut Vec<PendingVfsOpen> =
                d.get_temp_mut_or_default(egui::Id::new(crate::app::PENDING_VFS_OPEN_KEY));
            for p in to_open {
                queue.push(PendingVfsOpen::PluginMount { mount_id, entry_path: p });
            }
        });
    }
}

/// Body for a `Tab::PluginMount` whose mount isn't established
/// (couldn't connect, plugin returned an error on remount, etc.).
/// Renders the plugin's `message` verbatim and -- when the plugin
/// supplied `retry_label` -- a button that queues a retry under
/// [`PENDING_MOUNT_RETRY_KEY`] for the post-dock drain to pick up.
pub fn render_failed_mount_placeholder(
    ui: &mut egui::Ui,
    mount_id: crate::files::MountId,
    title: &str,
    message: &str,
    retry_label: Option<&str>,
) {
    ui.vertical_centered(|ui| {
        ui.add_space(24.0);
        ui.heading(title);
        ui.add_space(12.0);
        ui.label(message);
        if let Some(label) = retry_label {
            ui.add_space(16.0);
            if ui.button(label).clicked() {
                ui.ctx().data_mut(|d| {
                    let queue: &mut Vec<crate::files::MountId> =
                        d.get_temp_mut_or_default(egui::Id::new(PENDING_MOUNT_RETRY_KEY));
                    queue.push(mount_id);
                });
            }
        }
    });
}

/// Drain any pending retry-mount clicks captured by failed-mount
/// placeholders during the dock pass.
pub fn drain_pending_mount_retries(ctx: &egui::Context, app: &mut HxyApp) {
    let pending: Vec<crate::files::MountId> = ctx
        .data_mut(|d| d.remove_temp::<Vec<crate::files::MountId>>(egui::Id::new(PENDING_MOUNT_RETRY_KEY)))
        .unwrap_or_default();
    for mount_id in pending {
        retry_failed_mount(app, mount_id);
    }
}

fn retry_failed_mount(app: &mut HxyApp, mount_id: crate::files::MountId) {
    let Some(entry) = app.mounts.get(&mount_id) else { return };
    if entry.status.live().is_some() {
        return;
    }
    let plugin_name = entry.plugin_name.clone();
    let token = entry.token.clone();
    let display = entry.display_name.clone();
    let plugin = match app.plugin_handlers.iter().find(|p| p.name() == plugin_name).cloned() {
        Some(p) => p,
        None => {
            app.console_log(ConsoleSeverity::Error, format!("Mount {display}"), "plugin no longer installed");
            return;
        }
    };
    let became_ready = match plugin.mount_by_token(&token) {
        Ok(mount) => {
            app.console_log(ConsoleSeverity::Info, format!("Mount {display}"), "remount succeeded");
            if let Some(entry) = app.mounts.get_mut(&mount_id) {
                entry.status = crate::files::MountStatus::Ready(Arc::new(mount));
            }
            true
        }
        Err(e) => {
            app.console_log(ConsoleSeverity::Warning, format!("Mount {display}"), e.message.clone());
            if let Some(entry) = app.mounts.get_mut(&mount_id) {
                entry.status = crate::files::MountStatus::Failed { message: e.message, retry_label: e.retry_label };
            }
            false
        }
    };
    if !became_ready {
        return;
    }
    let parent_source =
        TabSource::PluginMount { plugin_name: plugin_name.clone(), token: token.clone(), title: display.clone() };
    let to_open: Vec<crate::state::OpenTabState> = app
        .state
        .read()
        .open_tabs
        .iter()
        .filter(|t| matches!(&t.source, TabSource::VfsEntry { parent, .. } if parent.as_ref() == &parent_source))
        .filter(|t| !app.files.values().any(|f| f.source_kind.as_ref() == Some(&t.source)))
        .cloned()
        .collect();
    for tab in to_open {
        if let Err(e) = app.restore_one_tab(&tab, false) {
            tracing::warn!(error = %e, "open vfs entry after mount retry");
        }
    }
}

/// Drive the side effect for whatever outcome a plugin returned
/// from `invoke` or `respond_to_prompt`. Centralized so both
/// initial command activation and prompt answers fan out through
/// the same switch (`Done` -> close, `Cascade` -> sub-palette,
/// `Mount` -> tab, `Prompt` -> argument-style sub-palette that
/// rounds back here on submit).
pub fn dispatch_plugin_outcome(
    ctx: &egui::Context,
    app: &mut HxyApp,
    plugin: Arc<hxy_plugin_host::PluginHandler>,
    plugin_name: &str,
    command_id: &str,
    outcome: Option<hxy_plugin_host::InvokeOutcome>,
) {
    match outcome {
        Some(hxy_plugin_host::InvokeOutcome::Done) => app.palette.close(),
        Some(hxy_plugin_host::InvokeOutcome::Cascade(commands)) => {
            app.palette.enter_plugin_cascade(plugin_name.to_owned(), commands);
        }
        Some(hxy_plugin_host::InvokeOutcome::Mount(req)) => {
            app.palette.close();
            let plugin_name_owned = plugin.name().to_owned();
            let mut ops = std::mem::take(&mut app.pending_plugin_ops);
            crate::plugins::runner::spawn_mount_by_token(
                &mut ops,
                app,
                ctx.clone(),
                plugin,
                plugin_name_owned,
                req.token,
                req.title,
            );
            app.pending_plugin_ops = ops;
        }
        Some(hxy_plugin_host::InvokeOutcome::Prompt(req)) => {
            app.palette.enter_plugin_prompt(
                plugin_name.to_owned(),
                command_id.to_owned(),
                req.title,
                req.default_value,
            );
        }
        None => {
            app.palette.close();
        }
    }
}

/// Install an already-resolved `MountedVfs` as a new `Tab::PluginMount`.
/// The mount itself lives in `app.mounts`; the dock tab carries only
/// the `MountId`. The worker thread that ran `mount-by-token` ends
/// here, and session restoration funnels through here too.
pub fn install_mount_tab(
    app: &mut HxyApp,
    plugin: Arc<hxy_plugin_host::PluginHandler>,
    mount: hxy_vfs::MountedVfs,
    token: String,
    title: String,
) {
    let mount_id = crate::files::MountId::new(app.next_mount_id);
    app.next_mount_id += 1;
    let plugin_name = plugin.name().to_owned();
    let entry = crate::files::MountedPlugin {
        display_name: title.clone(),
        plugin_name: plugin_name.clone(),
        token: token.clone(),
        status: crate::files::MountStatus::Ready(Arc::new(mount)),
    };
    app.mounts.insert(mount_id, entry);

    let source = TabSource::PluginMount { plugin_name: plugin_name.clone(), token, title: title.clone() };
    {
        let mut g = app.state.write();
        if !g.open_tabs.iter().any(|t| t.source == source) {
            g.open_tabs.push(crate::state::OpenTabState {
                source,
                selection: None,
                scroll_offset: 0.0,
                as_workspace: false,
            });
        }
    }

    let tool_leaf = crate::tabs::dock_ops::push_tool_tab(&mut app.dock, Tab::PluginMount(mount_id));
    if let Some(path) = app.dock.find_tab(&Tab::PluginMount(mount_id)) {
        crate::tabs::dock_ops::remove_welcome_from_leaf(&mut app.dock, path.surface, path.node);
        if let Some(fresh_path) = app.dock.find_tab(&Tab::PluginMount(mount_id)) {
            let _ = app.dock.set_active_tab(fresh_path);
        }
    }
    app.dock.set_focused_node_and_surface(tool_leaf);
    tracing::info!(plugin = %plugin_name, title = %title, id = %mount_id.get(), "mount tab installed");
}
