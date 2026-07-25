//! [`HexPane`]: a focusable gpui entity that owns a [`HexEditor`] and
//! renders it through the canvas paint pass in [`crate::paint`].

use std::sync::Arc;

use gpui::App;
use gpui::ClipboardItem;
use gpui::Context;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::KeyDownEvent;
use gpui::ParentElement;
use gpui::Pixels;
use gpui::Render;
use gpui::ScrollDelta;
use gpui::ScrollWheelEvent;
use gpui::Styled;
use gpui::Window;
use gpui::div;
use gpui::px;
use hxy_editor::Disposition;
use hxy_editor::Effect;
use gpui_component::ActiveTheme;
use hxy_core::ColumnCount;
use hxy_core::HexSource;

use crate::GridGeometry;
use crate::paint::GridSnapshot;
use crate::paint::PaintColors;
use crate::paint::hex_canvas;

/// Row height fallback used to convert pixel scroll deltas before the
/// first frame measures the real line height.
const FALLBACK_LINE_H: f32 = 16.0;

/// Geometry and viewport facts latched during paint, consumed by input
/// handlers (hit testing, page-size scrolling).
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    pub geometry: GridGeometry,
    pub content_origin: gpui::Point<Pixels>,
    pub rows_visible: f32,
    pub first_visible_row: u64,
}

/// A hex-editor viewport entity. Holds the editor model, keyboard focus,
/// the column count, and vertical scroll in fractional rows.
pub struct HexPane {
    editor: hxy_editor::HexEditor,
    focus_handle: FocusHandle,
    columns: ColumnCount,
    /// Vertical scroll in fractional rows (row 0 at top when 0.0).
    scroll_rows: f32,
    /// Set during paint, consumed by input handlers.
    pub(crate) last_frame: Option<FrameInfo>,
}

impl HexPane {
    pub fn new(source: Arc<dyn HexSource>, cx: &mut Context<Self>) -> Self {
        Self {
            editor: hxy_editor::HexEditor::new(source),
            focus_handle: cx.focus_handle(),
            columns: ColumnCount::DEFAULT,
            scroll_rows: 0.0,
            last_frame: None,
        }
    }

    pub fn editor(&self) -> &hxy_editor::HexEditor {
        &self.editor
    }

    /// Mutable editor access. Callers that mutate must `cx.notify()` to
    /// schedule a repaint.
    pub fn editor_mut(&mut self) -> &mut hxy_editor::HexEditor {
        &mut self.editor
    }

    pub fn set_source(&mut self, source: Arc<dyn HexSource>, cx: &mut Context<Self>) {
        self.editor = hxy_editor::HexEditor::new(source);
        self.scroll_rows = 0.0;
        self.last_frame = None;
        cx.notify();
    }

    /// Total rows including the trailing EOF-cursor row.
    fn row_count(&self) -> u64 {
        let cols = self.columns.as_u64();
        self.editor.source().len().get().saturating_add(1).div_ceil(cols).max(1)
    }

    fn line_height(&self) -> Pixels {
        self.last_frame.map(|f| f.geometry.metrics.line_h).unwrap_or(px(FALLBACK_LINE_H))
    }

    fn on_scroll_wheel(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let delta_rows = scroll_delta_to_rows(ev.delta, self.line_height());
        let max = self.row_count().saturating_sub(1) as f32;
        // gpui scroll deltas are negative when the wheel moves toward
        // later content, which should advance the top row.
        self.scroll_rows = (self.scroll_rows - delta_rows).clamp(0.0, max);
        cx.notify();
    }

    /// Feed one key-down through the editor. Returns true when the
    /// editor consumed it (callers stop propagation).
    ///
    /// Mirrors the egui adapter's dispatch contract: translate the
    /// keystroke into a key event plus the synthesized text event, feed
    /// both through one input filter, then apply the batch. `apply_input`
    /// runs UNCONDITIONALLY -- even an empty batch advances the editor's
    /// once-per-frame bookkeeping (external-cursor-move detection,
    /// history boundaries).
    ///
    /// One intentional divergence from egui: the egui adapter drains a
    /// whole frame of queued events into a single filter/apply batch,
    /// whereas each gpui key-down is its own single-event batch. The
    /// editor's per-event dispatch and the Insert/Replace Escape latch
    /// only matter within one batch, so single-event batches stay
    /// semantically safe -- Escape still pops the mode; there are simply
    /// no same-batch followers for the latch to drop.
    pub(crate) fn handle_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (key_event, text_event) = crate::input::translate(&event.keystroke);
        let mut filter = self.editor.input_filter();
        let mut consumed = false;
        for event in [key_event, text_event].into_iter().flatten() {
            consumed |= filter.feed(&event) == Disposition::Consumed;
        }
        for effect in self.editor.apply_input(filter.finish()) {
            match effect {
                Effect::CopyText(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            }
        }
        cx.notify();
        consumed
    }

    #[cfg(test)]
    pub(crate) fn scroll_rows(&self) -> f32 {
        self.scroll_rows
    }
}

/// Pure delta-to-rows conversion. `Lines` deltas are rows already;
/// `Pixels` deltas divide by the row height.
fn scroll_delta_to_rows(delta: ScrollDelta, line_h: Pixels) -> f32 {
    match delta {
        ScrollDelta::Lines(p) => p.y,
        ScrollDelta::Pixels(p) => f32::from(p.y) / f32::from(line_h),
    }
}

impl Focusable for HexPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HexPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = GridSnapshot {
            source: self.editor.source().clone(),
            selection: self.editor.selection(),
            active_pane: self.editor.active_pane(),
            nibble: self.editor.nibble(),
            columns: self.columns,
            scroll_rows: self.scroll_rows,
            colors: PaintColors::from_theme(cx.theme()),
            mono_family: cx.theme().mono_font_family.clone(),
            mono_size: cx.theme().mono_font_size,
        };
        let canvas = hex_canvas(snap, cx.entity());

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_key_down(cx.listener(|this, event, window, cx| {
                if this.handle_key_down(event, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(div().size_full().child(canvas))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui::TouchPhase;
    use gpui::point;
    use hxy_core::MemorySource;

    /// 64 full rows of 16 columns.
    fn source_64_rows() -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(vec![0u8; 64 * 16]))
    }

    fn lines_event(dy: f32) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: point(px(20.0), px(20.0)),
            delta: ScrollDelta::Lines(point(0.0, dy)),
            modifiers: Default::default(),
            touch_phase: TouchPhase::Moved,
        }
    }

    #[test]
    fn lines_delta_is_rows_pixels_delta_divides_by_line_height() {
        assert_eq!(scroll_delta_to_rows(ScrollDelta::Lines(point(0.0, -3.0)), px(16.0)), -3.0);
        assert_eq!(scroll_delta_to_rows(ScrollDelta::Pixels(point(px(0.0), px(-32.0))), px(16.0)), -2.0);
    }

    #[gpui::test]
    fn scroll_wheel_moves_and_clamps(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|_window, cx| HexPane::new(source_64_rows(), cx));
        let pane = window.root(cx).unwrap();
        let max = window.update(cx, |p, _, _| p.row_count().saturating_sub(1) as f32).unwrap();

        // Wheel toward later content advances the top row.
        window.update(cx, |p, window, cx| p.on_scroll_wheel(&lines_event(-3.0), window, cx)).unwrap();
        assert_eq!(pane.read_with(cx, |p, _| p.scroll_rows()), 3.0);

        // A large forward scroll clamps back to the top.
        window.update(cx, |p, window, cx| p.on_scroll_wheel(&lines_event(1_000.0), window, cx)).unwrap();
        assert_eq!(pane.read_with(cx, |p, _| p.scroll_rows()), 0.0);

        // A large backward scroll clamps at the last row.
        window.update(cx, |p, window, cx| p.on_scroll_wheel(&lines_event(-1_000.0), window, cx)).unwrap();
        assert_eq!(pane.read_with(cx, |p, _| p.scroll_rows()), max);
    }
}
