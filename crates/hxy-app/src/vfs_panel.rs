//! Tree panel that renders a mounted VFS using `egui_ltreeview`.
//!
//! Lives inside the file tab when the tab has an active mount. Emits a
//! [`VfsPanelEvent::OpenEntry`] when the user activates (double-click
//! or Enter) a file entry — the app then opens that entry as a new
//! file tab with its [`TabSource`](hxy_vfs::TabSource) stored as a
//! `VfsEntry` referencing the current tab's source.

use egui_ltreeview::Action;
use egui_ltreeview::NodeBuilder;
use egui_ltreeview::TreeView;
use hxy_vfs::vfs::FileSystem;

/// Events emitted by the panel on a given frame. Returned via
/// [`show`]'s return value; the app translates them into effects.
#[derive(Debug, Clone)]
pub enum VfsPanelEvent {
    /// User activated a file entry. `path` is the VFS path.
    OpenEntry(String),
}

/// Render the panel for `fs` and return any events produced this frame.
/// The `id_seed` should be unique per tab so multiple mounted tabs don't
/// share tree state.
pub fn show(ui: &mut egui::Ui, id_seed: u64, fs: &dyn FileSystem) -> Vec<VfsPanelEvent> {
    // Clip everything painted by the tree to our allocated rect so
    // long entry names don't overflow horizontally into the hex view.
    ui.set_clip_rect(ui.max_rect());
    let mut events = Vec::new();
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let tree_id = egui::Id::new(("hxy_vfs_tree", id_seed));
        let (response, actions) = TreeView::new(tree_id).show(ui, |builder| {
            walk(builder, fs, "");
        });

        // `egui_ltreeview`'s `Action::Activate` (double-click on a
        // leaf) doesn't reliably fire. We synthesise the semantic we
        // actually want: SetSelected updates a remembered "current
        // file selection" in egui memory, and the tree's overall
        // `Response::double_clicked()` flushes it into an open event.
        // Single-clicks only update the memory slot.
        let selection_mem = tree_id.with("current_file");
        for action in actions {
            match action {
                Action::Activate(act) => {
                    for id in act.selected {
                        if let Some(path) = id.strip_prefix("F:") {
                            events.push(VfsPanelEvent::OpenEntry(path.to_string()));
                        }
                    }
                }
                Action::SetSelected(selected) => {
                    let current: Option<String> =
                        selected.iter().find_map(|id| id.strip_prefix("F:").map(str::to_owned));
                    ui.ctx().data_mut(|d| d.insert_temp(selection_mem, current));
                }
                _ => {}
            }
        }
        if response.double_clicked() {
            let pending: Option<Option<String>> = ui.ctx().data_mut(|d| d.get_temp(selection_mem));
            if let Some(Some(path)) = pending {
                events.push(VfsPanelEvent::OpenEntry(path));
            }
        }
    });
    events
}

fn walk(builder: &mut egui_ltreeview::TreeViewBuilder<'_, String>, fs: &dyn FileSystem, path: &str) {
    let Ok(entries) = fs.read_dir(if path.is_empty() { "/" } else { path }) else { return };
    // Collect and sort so directories come first and entries are stable.
    let mut entries: Vec<String> = entries.collect();
    entries.sort();
    let (mut dirs, mut files): (Vec<_>, Vec<_>) = entries.into_iter().partition(|name| {
        let full = join(path, name);
        fs.metadata(&full).map(|m| m.file_type == hxy_vfs::vfs::VfsFileType::Directory).unwrap_or(false)
    });
    dirs.sort();
    files.sort();

    for name in dirs {
        let full = join(path, &name);
        let id = format!("D:{full}");
        let label = format!("{} {}", egui_phosphor::regular::FOLDER, name);
        builder.node(NodeBuilder::dir(id).label(label).default_open(false));
        walk(builder, fs, &full);
        builder.close_dir();
    }
    for name in files {
        let full = join(path, &name);
        let id = format!("F:{full}");
        let label = format!("{} {}", egui_phosphor::regular::FILE, name);
        builder.node(NodeBuilder::leaf(id).label(label));
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { format!("/{name}") } else { format!("{parent}/{name}") }
}
