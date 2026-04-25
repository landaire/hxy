//! Tree panel that renders a mounted VFS using `egui_ltreeview`.
//!
//! Lives inside the file tab when the tab has an active mount. Emits a
//! [`VfsPanelEvent::OpenEntry`] when the user activates (double-click
//! or Enter) a file entry -- the app then opens that entry as a new
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
    let mut totals = Totals::default();

    // Footer first so its height is reserved before the scroll area claims
    // the remaining vertical space.
    let footer_text = egui::Id::new(("hxy_vfs_footer", id_seed));
    egui::Panel::bottom(egui::Id::new(("hxy_vfs_footer_panel", id_seed))).resizable(false).show_inside(ui, |ui| {
        let text: String = ui.ctx().data(|d| d.get_temp(footer_text)).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.weak(text);
        });
    });

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let tree_id = egui::Id::new(("hxy_vfs_tree", id_seed));
        let (response, actions) = TreeView::new(tree_id).show(ui, |builder| {
            walk(builder, fs, "", &mut totals);
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

    let text = format!(
        "{} file{}, {} folder{}",
        totals.files,
        if totals.files == 1 { "" } else { "s" },
        totals.dirs,
        if totals.dirs == 1 { "" } else { "s" },
    );
    ui.ctx().data_mut(|d| d.insert_temp(footer_text, text));

    events
}

#[derive(Default)]
struct Totals {
    files: usize,
    dirs: usize,
}

fn walk(
    builder: &mut egui_ltreeview::TreeViewBuilder<'_, String>,
    fs: &dyn FileSystem,
    path: &str,
    totals: &mut Totals,
) {
    let Ok(entries) = fs.read_dir(if path.is_empty() { "/" } else { path }) else { return };
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();
    for name in entries {
        let full = join(path, &name);
        match fs.metadata(&full) {
            Ok(m) if m.file_type == hxy_vfs::vfs::VfsFileType::Directory => dirs.push(name),
            Ok(m) => files.push((name, m.len)),
            Err(_) => files.push((name, 0)),
        }
    }
    dirs.sort();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for name in dirs {
        let full = join(path, &name);
        let id = format!("D:{full}");
        let label = format!("{} {}", egui_phosphor::regular::FOLDER, name);
        let is_open = builder.node(NodeBuilder::dir(id).label(label).default_open(false));
        totals.dirs += 1;
        // Lazy descent: only recurse into children when the user has
        // actually expanded this node. For in-memory VFS handlers
        // (zip, etc.) the eager walk was cheap; for a TCP-backed
        // mount like xbox-neighborhood it pulls the entire remote
        // filesystem on the UI thread and freezes the frame loop.
        if is_open {
            walk(builder, fs, &full, totals);
        }
        builder.close_dir();
    }
    for (name, size) in files {
        let full = join(path, &name);
        let id = format!("F:{full}");
        let size_text = format_size(size);
        let label = format!("{} {}", egui_phosphor::regular::FILE, name);
        builder.node(NodeBuilder::leaf(id).label_ui(move |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(&label).selectable(false).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(&size_text).weak()).selectable(false));
                });
            });
        }));
        totals.files += 1;
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { format!("/{name}") } else { format!("{parent}/{name}") }
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} {}", UNITS[0]) } else { format!("{value:.1} {}", UNITS[unit]) }
}
