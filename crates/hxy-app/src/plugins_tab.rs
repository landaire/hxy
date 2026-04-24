//! Plugin manager tab: lists `.wasm` components in the two user plugin
//! directories (VFS handlers + template runtimes), and provides install
//! / delete / rescan affordances. The tab reads the filesystem directly
//! to discover files; the actual `VfsRegistry` rebuild is deferred to
//! `HxyApp` via the [`PluginsEvent`] channel.

#![cfg(not(target_arch = "wasm32"))]

use std::fs;
use std::path::PathBuf;

/// Events the tab emits back to the app. The tab itself doesn't own
/// the registry; `HxyApp` drains these after the dock render and
/// reloads / relinks plugins as needed.
pub enum PluginsEvent {
    /// User changed the filesystem (install or delete). The app
    /// should rebuild both the VFS registry and template runtime list.
    Rescan,
}

pub fn show(ui: &mut egui::Ui, handlers_dir: Option<&PathBuf>, templates_dir: Option<&PathBuf>) -> Vec<PluginsEvent> {
    let mut events = Vec::new();
    ui.heading("Plugins");
    ui.label("Drop compiled WASM components into these directories to load them at startup.");
    ui.separator();

    render_section(
        ui,
        "VFS handlers",
        "Parse archive-like byte sources into a browseable tree.",
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
        if ui.button("Install…").clicked()
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
