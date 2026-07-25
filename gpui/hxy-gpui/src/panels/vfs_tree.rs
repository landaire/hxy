//! [`VfsTreePanel`]: a lazily-expanded tree over a mounted VFS, living
//! in the left dock of a [`WorkspaceHostPanel`](super::workspace_host)'s
//! inner dock. Mirrors the egui app's `panels/vfs.rs`: directories are
//! only descended into when the user has actually expanded them
//! (`visible_rows`), so a remote / streaming mount is never walked
//! eagerly. Activating a file row emits [`VfsTreeEvent::OpenEntry`]; the
//! host turns that into an editor tab in the inner dock.
//!
//! Not registered with the global `PanelRegistry`: it needs the live
//! [`MountedVfs`] to render, which a registry closure cannot supply, so
//! the host rebuilds it by hand on restore (injecting the mount and the
//! persisted expansion set).

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::App;
use gpui::Context;
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
use gpui::UniformListScrollHandle;
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui::prelude::FluentBuilder;
use gpui::px;
use gpui::uniform_list;
use gpui_component::ActiveTheme;
use gpui_component::Icon;
use gpui_component::IconName;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use hxy_vfs::MountedVfs;
use hxy_vfs::vfs::FileSystem;
use hxy_vfs::vfs::VfsFileType;

/// Stable identifier for layout (de)serialization; must never change.
pub const VFS_TREE_PANEL_NAME: &str = "VfsTreePanel";

/// One visible row of the tree, in top-to-bottom render order. Produced
/// by [`visible_rows`], which only walks expanded directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsRow {
    /// VFS path with a leading slash (e.g. `/dir/file.bin`).
    pub path: String,
    /// Leaf name shown in the row.
    pub name: String,
    /// Indentation depth; root entries are depth 0.
    pub depth: usize,
    pub kind: VfsRowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsRowKind {
    Dir { open: bool },
    File { size: u64 },
}

/// Events the panel emits for the host to act on.
#[derive(Debug, Clone)]
pub enum VfsTreeEvent {
    /// The user activated a file entry at this VFS path.
    OpenEntry(String),
}

/// Flatten `fs` into the rows currently visible given `expanded`.
///
/// Lazy by construction: a directory's children are read only when its
/// path is in `expanded`. An unexpanded directory contributes exactly
/// one row and triggers no `read_dir` beneath it -- the property that
/// keeps a remote mount from being pulled in full (mirrors
/// `crates/hxy/src/panels/vfs.rs`'s `walk`).
pub fn visible_rows(fs: &dyn FileSystem, expanded: &BTreeSet<String>) -> Vec<VfsRow> {
    let mut out = Vec::new();
    walk(fs, "", 0, expanded, &mut out);
    out
}

fn walk(fs: &dyn FileSystem, parent: &str, depth: usize, expanded: &BTreeSet<String>, out: &mut Vec<VfsRow>) {
    let dir = if parent.is_empty() { "/" } else { parent };
    let Ok(entries) = fs.read_dir(dir) else { return };
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();
    for name in entries {
        let full = join(parent, &name);
        match fs.metadata(&full) {
            Ok(m) if m.file_type == VfsFileType::Directory => dirs.push(name),
            Ok(m) => files.push((name, m.len)),
            // A stat failure is not a reason to hide the entry; show it
            // as a zero-length file so the user still sees it exists.
            Err(_) => files.push((name, 0)),
        }
    }
    dirs.sort();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for name in dirs {
        let full = join(parent, &name);
        let open = expanded.contains(&full);
        out.push(VfsRow { path: full.clone(), name, depth, kind: VfsRowKind::Dir { open } });
        if open {
            walk(fs, &full, depth + 1, expanded, out);
        }
    }
    for (name, size) in files {
        let full = join(parent, &name);
        out.push(VfsRow { path: full, name, depth, kind: VfsRowKind::File { size } });
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { format!("/{name}") } else { format!("{parent}/{name}") }
}

/// Human-readable byte size. Mirrors `crates/hxy/src/panels/vfs.rs`'s
/// `format_size` so the two front-ends render identical size columns.
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} {}", UNITS[0]) } else { format!("{value:.1} {}", UNITS[unit]) }
}

pub struct VfsTreePanel {
    focus_handle: FocusHandle,
    mount: Arc<MountedVfs>,
    /// Directory paths (leading slash) the user has expanded. Persisted
    /// in the host's dump so the tree comes back open where it was.
    expanded: BTreeSet<String>,
    scroll: UniformListScrollHandle,
}

impl VfsTreePanel {
    pub fn new(mount: Arc<MountedVfs>, expanded: BTreeSet<String>, cx: &mut Context<Self>) -> Self {
        Self { focus_handle: cx.focus_handle(), mount, expanded, scroll: UniformListScrollHandle::new() }
    }

    /// The persisted expansion set, for the host's dump.
    pub fn expanded(&self) -> &BTreeSet<String> {
        &self.expanded
    }

    /// The rows currently visible, walking only expanded directories.
    pub fn rows(&self) -> Vec<VfsRow> {
        visible_rows(self.mount.fs.as_ref(), &self.expanded)
    }

    /// Toggle a directory's expansion. Collapsing drops the entry;
    /// expanding adds it (its children are walked lazily on the next
    /// render, never before).
    pub fn toggle_dir(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        cx.notify();
    }

    /// Emit an open request for a file entry.
    pub fn activate_file(&mut self, path: String, cx: &mut Context<Self>) {
        cx.emit(VfsTreeEvent::OpenEntry(path));
    }
}

impl EventEmitter<VfsTreeEvent> for VfsTreePanel {}
impl EventEmitter<PanelEvent> for VfsTreePanel {}

impl Panel for VfsTreePanel {
    fn panel_name(&self) -> &'static str {
        VFS_TREE_PANEL_NAME
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t("gpui-vfs-tree-title"))
    }
}

impl Focusable for VfsTreePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for VfsTreePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows();
        let count = rows.len();
        let weak = cx.entity().downgrade();
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().accent;
        let list = uniform_list("vfs-tree-rows", count, move |range, _window, _cx| {
            range.map(|ix| render_row(rows[ix].clone(), weak.clone(), muted, accent)).collect::<Vec<_>>()
        })
        .track_scroll(self.scroll.clone())
        .size_full();

        div().track_focus(&self.focus_handle).size_full().bg(cx.theme().background).child(list)
    }
}

fn render_row(row: VfsRow, weak: WeakEntity<VfsTreePanel>, muted: gpui::Hsla, accent: gpui::Hsla) -> impl IntoElement {
    let indent = px(8.0 + row.depth as f32 * 14.0);
    let (leading_icon, size_text, is_dir) = match &row.kind {
        VfsRowKind::Dir { open } => {
            let icon = if *open { IconName::FolderOpen } else { IconName::Folder };
            (icon, None, true)
        }
        VfsRowKind::File { size } => (IconName::File, Some(format_size(*size)), false),
    };
    let chevron = match &row.kind {
        VfsRowKind::Dir { open } => {
            Some(Icon::new(if *open { IconName::ChevronDown } else { IconName::ChevronRight }).size_4())
        }
        VfsRowKind::File { .. } => None,
    };

    let path = row.path.clone();
    let mut label = div().flex().flex_1().min_w_0().items_center().gap_1().child(Icon::new(leading_icon).size_4());
    label = label.child(div().flex_1().min_w_0().truncate().child(SharedString::from(row.name)));
    if let Some(size_text) = size_text {
        label = label.child(div().text_color(muted).child(SharedString::from(size_text)));
    }

    div()
        .id(SharedString::from(row.path.clone()))
        .flex()
        .items_center()
        .gap_1()
        .pl(indent)
        .pr_2()
        .py_0p5()
        .w_full()
        .hover(move |s| s.bg(accent))
        .when_some(chevron, |el, chevron| el.child(chevron))
        .child(label)
        .on_click(move |_event, _window, cx| {
            let _ = weak.update(cx, |this, cx| {
                if is_dir {
                    this.toggle_dir(path.clone(), cx);
                } else {
                    this.activate_file(path.clone(), cx);
                }
            });
        })
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Cursor;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use hxy_core::HexSource;
    use hxy_core::MemorySource;
    use hxy_vfs::MountedVfs;
    use hxy_vfs::VfsRegistry;
    use hxy_vfs::handlers::ZipHandler;
    use hxy_vfs::vfs::FileSystem;
    use hxy_vfs::vfs::SeekAndRead;
    use hxy_vfs::vfs::SeekAndWrite;
    use hxy_vfs::vfs::VfsMetadata;
    use hxy_vfs::vfs::VfsResult;

    /// Build an in-memory zip with a small nested layout:
    /// `/top.txt`, `/dir/nested.bin`, `/dir/sub/deep.dat`. Returned as
    /// raw bytes so tests can mount it or write it to disk.
    pub fn fixture_zip_bytes() -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let opts: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zip.start_file("top.txt", opts).unwrap();
            zip.write_all(b"hello top").unwrap();
            zip.start_file("dir/nested.bin", opts).unwrap();
            zip.write_all(b"nested bytes here").unwrap();
            zip.start_file("dir/sub/deep.dat", opts).unwrap();
            zip.write_all(b"deep").unwrap();
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    /// Mount [`fixture_zip_bytes`] through the real [`ZipHandler`].
    pub fn mount_fixture() -> Arc<MountedVfs> {
        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        let bytes = fixture_zip_bytes();
        let handler = registry.detect(&bytes).expect("zip handler matches fixture");
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        Arc::new(handler.mount(source).expect("mount fixture zip"))
    }

    /// A [`FileSystem`] decorator that counts `read_dir` calls per path,
    /// so a test can prove the tree walk is lazy (unexpanded directories
    /// are never read).
    #[derive(Debug)]
    pub struct CountingFs {
        inner: Box<dyn FileSystem>,
        reads: std::sync::Mutex<Vec<String>>,
        total: AtomicUsize,
    }

    impl CountingFs {
        pub fn new(inner: Box<dyn FileSystem>) -> Self {
            Self { inner, reads: std::sync::Mutex::new(Vec::new()), total: AtomicUsize::new(0) }
        }

        pub fn total_read_dirs(&self) -> usize {
            self.total.load(Ordering::SeqCst)
        }

        pub fn read_dir_paths(&self) -> Vec<String> {
            self.reads.lock().unwrap().clone()
        }
    }

    impl FileSystem for CountingFs {
        fn read_dir(&self, path: &str) -> VfsResult<Box<dyn Iterator<Item = String> + Send>> {
            self.total.fetch_add(1, Ordering::SeqCst);
            self.reads.lock().unwrap().push(path.to_string());
            self.inner.read_dir(path)
        }
        fn create_dir(&self, path: &str) -> VfsResult<()> {
            self.inner.create_dir(path)
        }
        fn open_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndRead + Send>> {
            self.inner.open_file(path)
        }
        fn create_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
            self.inner.create_file(path)
        }
        fn append_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
            self.inner.append_file(path)
        }
        fn metadata(&self, path: &str) -> VfsResult<VfsMetadata> {
            self.inner.metadata(path)
        }
        fn exists(&self, path: &str) -> VfsResult<bool> {
            self.inner.exists(path)
        }
        fn remove_file(&self, path: &str) -> VfsResult<()> {
            self.inner.remove_file(path)
        }
        fn remove_dir(&self, path: &str) -> VfsResult<()> {
            self.inner.remove_dir(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::test_support::CountingFs;
    use super::test_support::mount_fixture;
    use super::*;

    fn names(rows: &[VfsRow]) -> Vec<String> {
        rows.iter().map(|r| r.path.clone()).collect()
    }

    /// With nothing expanded, only the root level is listed and only the
    /// root directory is ever read: the nested `/dir` is shown but its
    /// children (and `/dir/sub`'s) are never walked. This is the test
    /// that fails if the walk is eager.
    #[test]
    fn root_listing_reads_only_the_root_directory() {
        let mount = mount_fixture();
        let fs = CountingFs::new(mount_fixture_fs());
        let expanded = BTreeSet::new();

        let rows = visible_rows(&fs, &expanded);

        assert_eq!(names(&rows), vec!["/dir".to_string(), "/top.txt".to_string()]);
        assert_eq!(fs.total_read_dirs(), 1, "only the root dir is read when nothing is expanded");
        assert_eq!(fs.read_dir_paths(), vec!["/".to_string()]);
        // The mount itself is otherwise unused here; keep it alive so the
        // fixture's byte backing does not drop mid-borrow.
        drop(mount);
    }

    /// Expanding `/dir` reveals its immediate children and reads exactly
    /// `/dir`, but the still-collapsed `/dir/sub` is never descended.
    #[test]
    fn expanding_one_dir_reads_only_that_dir() {
        let fs = CountingFs::new(mount_fixture_fs());
        let mut expanded = BTreeSet::new();
        expanded.insert("/dir".to_string());

        let rows = visible_rows(&fs, &expanded);

        assert_eq!(
            names(&rows),
            vec!["/dir".to_string(), "/dir/sub".to_string(), "/dir/nested.bin".to_string(), "/top.txt".to_string()],
        );
        assert_eq!(fs.total_read_dirs(), 2, "root plus the one expanded dir");
        assert!(!fs.read_dir_paths().contains(&"/dir/sub".to_string()), "collapsed /dir/sub is never walked");
    }

    /// A file row carries its byte size, formatted like the egui panel.
    #[test]
    fn file_rows_carry_sizes() {
        let fs = mount_fixture_fs();
        let rows = visible_rows(fs.as_ref(), &BTreeSet::new());
        let top = rows.iter().find(|r| r.path == "/top.txt").unwrap();
        assert_eq!(top.kind, VfsRowKind::File { size: 9 });
        assert_eq!(format_size(9), "9 B");
        assert_eq!(format_size(2048), "2.0 KB");
    }

    fn mount_fixture_fs() -> Box<dyn hxy_vfs::vfs::FileSystem> {
        use std::sync::Arc;

        use hxy_core::HexSource;
        use hxy_core::MemorySource;
        use hxy_vfs::VfsRegistry;
        use hxy_vfs::handlers::ZipHandler;

        use super::test_support::fixture_zip_bytes;

        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        let bytes = fixture_zip_bytes();
        let handler = registry.detect(&bytes).unwrap();
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        handler.mount(source).unwrap().fs
    }
}
