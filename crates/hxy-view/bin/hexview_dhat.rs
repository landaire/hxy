//! Synthetic frame-paint loop under dhat.
//!
//! Drives a read-only `HexView` against a 64 MiB in-memory source
//! through `egui_kittest::Harness` for `HXY_DHAT_FRAMES` (default
//! 200) frames, then drops the dhat profiler so its `dhat-heap.json`
//! (or `HXY_DHAT_OUTPUT`) gets flushed.
//!
//! Run with:
//! ```sh
//! cargo run -p hxy-view --example hexview_dhat --features dhat-bench --release
//! ```
//! Then load the resulting `dhat-heap.json` at
//! <https://nnethercote.github.io/dh_view/dh_view.html>.

use std::sync::Arc;

use egui_kittest::Harness;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_view::HexView;

#[global_allocator]
static GLOBAL: dhat::Alloc = dhat::Alloc;

struct State {
    source: Arc<dyn HexSource>,
    selection: Option<Selection>,
}

fn main() {
    let path = std::env::var("HXY_DHAT_OUTPUT").unwrap_or_else(|_| "dhat-heap.json".to_owned());
    eprintln!("dhat: writing heap profile to {path}");
    let _profiler = dhat::Profiler::builder().file_name(&path).build();

    let frames: u32 = std::env::var("HXY_DHAT_FRAMES").ok().and_then(|s| s.parse().ok()).unwrap_or(200);

    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0xABu8; 64 * 1024 * 1024]));
    let state = State { source, selection: None };

    let mut harness: Harness<'_, State> =
        Harness::builder().with_size(egui::Vec2::new(1200.0, 800.0)).with_pixels_per_point(1.0).build_ui_state(
            |ui, st: &mut State| {
                // Byte-value highlighting on so the tint-batching path is
                // exercised; default-off would emit zero `rect_filled`s
                // and hide the optimization we're measuring.
                HexView::new(st.source.as_ref(), &mut st.selection)
                    .value_highlight(Some(hxy_view::ValueHighlight::Background))
                    .show(ui);
            },
            state,
        );

    eprintln!("hexview_dhat: running {frames} frames over a 64 MiB MemorySource");
    for _ in 0..frames {
        harness.run();
    }
}
