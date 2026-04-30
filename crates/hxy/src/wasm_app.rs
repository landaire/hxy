//! Browser build of `HxyApp`.
//!
//! TEMPORARY: this is a slimmed-down `HxyApp` carrying only the
//! features that already compile cleanly under `wasm32-unknown-unknown`.
//! The desktop build's `HxyApp` lives in [`crate::app`]; both are
//! re-exported as `HxyApp` from the crate root so call sites
//! (`main.rs`, `wasm.rs`, anything that holds an `HxyApp`) refer
//! to one symbolic type.
//!
//! The end state collapses these two implementations into one --
//! the desktop modules need cfg-gating pushed inward (most fields
//! and methods don't fundamentally need a target gate, just the
//! ones touching plugin host / filesystem / IPC). Until that
//! refactor lands, this module exists as a stepping stone so we
//! can ship a working browser build incrementally rather than
//! blocking on the whole codebase.
//!
//! Features carried over so far: hex view of an in-memory buffer,
//! file open via `rfd::AsyncFileDialog`, multi-tab dock, basic
//! local search bar. Everything else (panels, palette, plugins,
//! templates, visualizers) follows in subsequent commits.

// `lib.rs` already gates this module to `target_arch = "wasm32"`;
// no inner `#![cfg]` -- clippy flags it as a duplicated attribute.

use std::collections::HashMap;
use std::sync::Arc;

use egui_dock::DockArea;
use egui_dock::DockState;
use egui_dock::TabViewer;
use hxy_core::ByteCache;
use hxy_core::CacheLimit;
use hxy_core::HexSource;
use hxy_core::MemorySource;

use crate::files::FileId;
use crate::files::OpenFile;
use crate::state::SharedPersistedState;
use crate::tabs::Tab;

pub struct HxyApp {
    dock: DockState<Tab>,
    files: HashMap<FileId, OpenFile>,
    next_file_id: u64,
    state: SharedPersistedState,
    byte_cache: Arc<ByteCache>,
    last_active_file: Option<FileId>,
    applied_zoom: f32,
}

impl HxyApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: SharedPersistedState) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.set_global_style(crate::style::hxy_style());
        let initial_zoom = state.read().app.zoom_factor;
        cc.egui_ctx.set_zoom_factor(initial_zoom);
        let limit = CacheLimit::from_mib(state.read().app.byte_cache_limit_mib);
        Self {
            dock: DockState::new(vec![Tab::Welcome]),
            files: HashMap::new(),
            next_file_id: 1,
            state,
            byte_cache: ByteCache::new(limit),
            last_active_file: None,
            applied_zoom: initial_zoom,
        }
    }

    /// Snapshot the active file's display name and byte source for
    /// the wasm save flow. Reads the entire source through the
    /// editor (so any in-memory patches are included). Returns
    /// `None` when no file is focused or the source can't be read.
    /// Bytes are buffered up front because the `rfd::save_file`
    /// future runs on a separate `spawn_local` task and can't
    /// re-borrow `&self.files`.
    fn active_file_bytes(&self) -> Option<(String, Vec<u8>)> {
        let id = self.last_active_file?;
        let file = self.files.get(&id)?;
        let len = file.editor.source().len().get();
        let bytes = if len == 0 {
            Vec::new()
        } else {
            let range = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)).ok()?;
            file.editor.source().read(range).ok()?
        };
        Some((file.display_name.clone(), bytes))
    }

    /// Open an in-memory byte buffer as a fresh file tab. Used by
    /// the rfd file picker and any future drag-and-drop / paste
    /// path. Returns the new tab's id.
    pub fn open_bytes(&mut self, name: String, bytes: Vec<u8>) -> FileId {
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let file = OpenFile::from_source(id, name, None, source, &self.byte_cache);
        self.files.insert(id, file);
        self.dock.push_to_focused_leaf(Tab::File(id));
        if let Some(path) = self.dock.find_tab(&Tab::Welcome) {
            let _ = self.dock.remove_tab(path);
        }
        self.last_active_file = Some(id);
        id
    }
}

impl eframe::App for HxyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Apply any zoom change the settings panel pushed (no
        // settings panel yet on wasm, but the field is wired
        // through PersistedState so a future palette command
        // can drive it).
        let target_zoom = self.state.read().app.zoom_factor;
        if (target_zoom - self.applied_zoom).abs() > f32::EPSILON {
            ctx.set_zoom_factor(target_zoom);
            self.applied_zoom = target_zoom;
        }
        // Drag-and-drop file open. egui's `dropped_files` carries
        // the file name + bytes for each file the user dropped on
        // the canvas this frame; on wasm `bytes` is always
        // populated (browser file API), `path` is `None`.
        let dropped: Vec<egui::DroppedFile> = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            let bytes = match f.bytes {
                Some(b) => b.to_vec(),
                None => continue,
            };
            let name = if f.name.is_empty() { "dropped".to_owned() } else { f.name };
            self.open_bytes(name, bytes);
        }
        egui::Panel::top("hxy_top_bar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open file...").clicked() {
                    let ctx_clone = ctx.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
                            return;
                        };
                        let bytes = handle.read().await;
                        let name = handle.file_name();
                        push_open_request(name, bytes);
                        ctx_clone.request_repaint();
                    });
                }
                // Save the active file's current bytes (post any
                // in-memory edits) as a browser download. rfd's
                // wasm save backend doesn't pop a save-as dialog
                // -- it returns a writable handle whose `.write`
                // call triggers the download with the suggested
                // filename baked in.
                let snapshot = self.active_file_bytes();
                ui.add_enabled_ui(snapshot.is_some(), |ui| {
                    if ui.button("Save as...").clicked()
                        && let Some((name, bytes)) = snapshot
                    {
                        wasm_bindgen_futures::spawn_local(async move {
                            let Some(handle) = rfd::AsyncFileDialog::new().set_file_name(&name).save_file().await
                            else {
                                return;
                            };
                            if let Err(e) = handle.write(&bytes).await {
                                tracing::warn!(error = %e, "wasm save");
                            }
                        });
                    }
                });
                ui.label(crate::APP_NAME);
            });
        });
        // Drain any rfd-driven open requests posted from the
        // async picker callback. Done here (UI thread) so the
        // file insertion sees a `&mut self`.
        for (name, bytes) in drain_open_requests() {
            self.open_bytes(name, bytes);
        }
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let style = crate::style::hxy_dock_style(ui.style());
            DockArea::new(&mut self.dock).style(style).show_inside(
                ui,
                &mut WasmTabViewer { files: &mut self.files, last_active_file: &mut self.last_active_file },
            );
        });
    }
}

/// Per-frame TabViewer for the wasm build. The desktop build's
/// `HxyTabViewer` carries enough refs to render every panel kind;
/// here we just paint the hex view (or a Welcome placeholder) and
/// leave the other Tab variants as no-ops for now.
struct WasmTabViewer<'a> {
    files: &'a mut HashMap<FileId, OpenFile>,
    last_active_file: &'a mut Option<FileId>,
}

impl TabViewer for WasmTabViewer<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            Tab::Welcome => "Welcome".into(),
            Tab::Settings => "Settings".into(),
            Tab::Console => "Console".into(),
            Tab::Inspector => "Inspector".into(),
            Tab::Plugins => "Plugins".into(),
            Tab::Memory => "Memory".into(),
            Tab::File(id) => match self.files.get(id) {
                Some(f) => f.display_name.clone().into(),
                None => format!("file-{}", id.get()).into(),
            },
            Tab::Workspace(id) => format!("workspace-{}", id.get()).into(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Welcome => {
                ui.heading("hxy");
                ui.label("Open a file from the toolbar to get started.");
            }
            Tab::File(id) => {
                let id = *id;
                if let Some(file) = self.files.get_mut(&id) {
                    *self.last_active_file = Some(id);
                    let len = file.editor.source().len().get();
                    if len > 0 {
                        let columns = hxy_core::ColumnCount::DEFAULT;
                        let response = file.editor.view().columns(columns).show(ui);
                        file.editor.on_response(&response, columns);
                    } else {
                        ui.label("(empty buffer)");
                    }
                } else {
                    ui.colored_label(egui::Color32::RED, format!("missing file {id:?}"));
                }
            }
            other => {
                ui.label(format!("{other:?} (not yet wired on wasm)"));
            }
        }
    }
}

// rfd's `pick_file` future runs in a separate spawned task and
// can't capture `&mut HxyApp`. We use a thread-local mailbox to
// post the picked bytes back; the per-frame `update` drains it
// so insertion happens on the same thread that owns the app.
// thread-local is fine here because wasm is single-threaded
// without explicit worker setup.
type OpenRequest = (String, Vec<u8>);

thread_local! {
    static OPEN_INBOX: std::cell::RefCell<Vec<OpenRequest>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn push_open_request(name: String, bytes: Vec<u8>) {
    OPEN_INBOX.with(|q| q.borrow_mut().push((name, bytes)));
}

fn drain_open_requests() -> Vec<OpenRequest> {
    OPEN_INBOX.with(|q| std::mem::take(&mut *q.borrow_mut()))
}
