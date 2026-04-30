//! Toast / non-blocking prompt center for the desktop GUI.
//!
//! Wraps `egui_toast` for the simple info / success / warning bubbles
//! and adds a template-prompt path on top: when the user opens a
//! file we recognise, we surface the matching templates as a single
//! sticky panel anchored to the file tab. Each row is one runnable
//! template -- accepting one closes the panel; descriptions are
//! truncated and revealed in full on hover.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use egui::Align2;
use egui::Color32;
use egui::Frame;
use egui::Id;
use egui::Order;
use egui::RichText;
use egui::Stroke;
use egui::vec2;
use egui_toast::Toast;
use egui_toast::ToastKind;
use egui_toast::ToastOptions;
use egui_toast::Toasts;

use crate::files::FileId;

/// One runnable template inside a [`TemplatePromptGroup`]. Holds the
/// data needed to render a row and dispatch a run if accepted.
#[derive(Clone, Debug)]
pub struct TemplatePromptEntry {
    /// Source path of the template that would run if accepted.
    /// Resolved against [`crate::templates::library::TemplateLibrary`]
    /// at draw time so a freshly downloaded corpus can populate
    /// new entries without restarting the app.
    pub template_path: PathBuf,
    /// Display name (`png.hexpat`). Shown verbatim in the row.
    pub name: String,
    /// Optional one-liner pulled from the template header. Rendered
    /// truncated next to the name and revealed in full on hover.
    pub description: Option<String>,
}

/// A coalesced "run a template?" prompt for one open file. All
/// matching templates appear as rows inside a single anchored panel
/// so the corner doesn't fill with redundant toasts. Accepting any
/// row dispatches that template's run and dismisses the whole group;
/// a single Dismiss button hides the group without running anything.
#[derive(Clone, Debug)]
pub struct TemplatePromptGroup {
    /// Group key shared by every entry produced for the same open.
    /// Today the file id, but kept opaque so cross-source prompts
    /// (drag-and-drop, plugin-mounted, ...) can share a group later.
    pub group: u64,
    /// File this prompt was raised for. Stays valid for the prompt's
    /// lifetime because the panel can only run while the file is
    /// still open -- closing the tab dismisses the group.
    pub file_id: FileId,
    /// Candidate templates, in the order returned by the ranker.
    pub entries: Vec<TemplatePromptEntry>,
    /// Tracks remaining inactivity time. Hover pauses decay so an
    /// engaged user isn't punished for reading.
    pub remaining: f32,
}

/// Thing the host loop should do in response to a clicked prompt.
/// Drained by `app.update()` and dispatched the same way the
/// command palette's `Run Template` action goes.
#[derive(Clone, Debug)]
pub struct PendingTemplateRun {
    pub file_id: FileId,
    pub template_path: PathBuf,
}

/// Default time a prompt stays on screen before fading. Long enough
/// for a deliberate scan of the page; short enough that an unwanted
/// suggestion doesn't follow you around.
const PROMPT_TTL_SECONDS: f32 = 30.0;

/// Upper bound on the name column. A pathologically long template
/// filename truncates rather than ballooning the panel.
const NAME_COL_MAX: f32 = 220.0;
/// Upper bound on the description column. Beyond this the description
/// truncates and the full text is shown on hover, so a multi-sentence
/// description can't span half the screen.
const DESC_COL_MAX: f32 = 320.0;
/// Per-row height. Matches the default interactable height so the
/// Run button doesn't grow taller than the labels next to it.
const ROW_HEIGHT: f32 = 22.0;
/// Width reserved for the per-row Run button. Hardcoded so the
/// containing panel width is deterministic and the Dismiss row's
/// right-alignment sits flush with the rows above.
const RUN_BUTTON_W: f32 = 50.0;

pub struct ToastCenter {
    /// egui_toast handle for the simple text bubbles. Anchored at
    /// the top-right of the central area to mirror the previous
    /// egui-notify default and stay out of the dock-tab strip.
    inner: Toasts,
    /// Active grouped template prompts. Re-rendered every frame as
    /// our own anchored Area widgets so we keep direct control over
    /// lifecycle (whole-group dismissal, accept-on-click).
    prompts: Vec<TemplatePromptGroup>,
}

impl Default for ToastCenter {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastCenter {
    pub fn new() -> Self {
        Self {
            inner: Toasts::new().anchor(Align2::RIGHT_TOP, (-12.0, 12.0)).direction(egui::Direction::TopDown),
            prompts: Vec::new(),
        }
    }

    pub fn info(&mut self, text: &str) {
        self.inner.add(Self::transient_toast(text, ToastKind::Info));
    }

    pub fn success(&mut self, text: &str) {
        self.inner.add(Self::transient_toast(text, ToastKind::Success));
    }

    pub fn warning(&mut self, text: &str) {
        self.inner.add(Self::transient_toast(text, ToastKind::Warning));
    }

    pub fn error(&mut self, text: &str) {
        self.inner.add(Self::transient_toast(text, ToastKind::Error));
    }

    fn transient_toast(text: &str, kind: ToastKind) -> Toast {
        Toast {
            kind,
            text: text.into(),
            options: ToastOptions::default().duration(Duration::from_secs(4)).show_progress(true),
            style: Default::default(),
        }
    }

    /// Replace the prompt group keyed by `(group, file_id)` with the
    /// supplied entries, or create a new group if none exists.
    /// Refreshes the inactivity timer so a freshly recomputed match
    /// list doesn't expire on a stale clock. Called once per
    /// detection pass; the caller is responsible for deciding how
    /// many candidates to surface.
    pub fn set_template_prompt(&mut self, group: u64, file_id: FileId, entries: Vec<TemplatePromptEntry>) {
        if entries.is_empty() {
            self.prompts.retain(|g| g.group != group);
            return;
        }
        if let Some(existing) = self.prompts.iter_mut().find(|g| g.group == group) {
            existing.file_id = file_id;
            existing.entries = entries;
            existing.remaining = PROMPT_TTL_SECONDS;
            return;
        }
        self.prompts.push(TemplatePromptGroup { group, file_id, entries, remaining: PROMPT_TTL_SECONDS });
    }

    /// Drop every prompt group targeting `file_id`. Called when a tab
    /// closes so abandoned suggestions don't hang around.
    pub fn dismiss_for_file(&mut self, file_id: FileId) {
        self.prompts.retain(|p| p.file_id != file_id);
    }

    /// Render the simple info / success / warning / error bubbles
    /// (the egui_toast cluster). Called once per frame at the app
    /// level so these stay app-global; template prompts are routed
    /// separately through [`Self::show_template_prompts_for`] so each
    /// prompt can render scoped to the file tab it targets.
    pub fn show_toasts(&mut self, ctx: &egui::Context) {
        // egui_toast wants a `&mut Ui`; we don't have one in this
        // late stage of the frame, so spin up a transparent
        // foreground Area as a host. The library's own Areas anchor
        // independently, so this outer rect just needs to exist.
        let host_id = Id::new("hxy_toast_host");
        egui::Area::new(host_id)
            .order(Order::Foreground)
            .interactable(false)
            .anchor(Align2::RIGHT_TOP, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                self.inner.show(ui);
            });

        // TTL bookkeeping happens here even though the actual prompt
        // widgets are drawn elsewhere: we want a prompt to expire
        // even if its targeted tab isn't currently focused.
        self.prompts.retain(|p| p.remaining > 0.0);
    }

    /// Render the template-prompt panel for `file_id`, anchored to
    /// the top-right of `tab_rect`. Drains accepted runs into
    /// `pending_runs`; the host loop then routes those through the
    /// same code path the palette's `Run Template` action uses.
    ///
    /// Called from inside the file tab's body so the prompt visually
    /// lives in the tab's space rather than the app-global corner.
    /// `tab_rect` should be the tab's content area in screen
    /// coordinates (typically `ui.max_rect()` from the tab body).
    pub fn show_template_prompts_for(
        &mut self,
        ctx: &egui::Context,
        tab_rect: egui::Rect,
        file_id: FileId,
        pending_runs: &mut Vec<PendingTemplateRun>,
    ) {
        let dt = ctx.input(|i| i.unstable_dt);
        let mut dismissed_groups: HashSet<u64> = HashSet::new();
        for prompt in self.prompts.iter_mut() {
            if prompt.file_id != file_id {
                continue;
            }
            let area_id = Id::new("hxy_template_prompt").with(prompt.group);
            let pos = egui::pos2(tab_rect.right() - 12.0, tab_rect.top() + 12.0);
            let response = egui::Area::new(area_id)
                .order(Order::Foreground)
                .pivot(Align2::RIGHT_TOP)
                .fixed_pos(pos)
                .constrain_to(tab_rect)
                .show(ctx, |ui| {
                    Frame::window(ui.style()).stroke(Stroke::new(1.0, Color32::from_gray(80))).show(ui, |ui| {
                        // Measure the widest name and description in
                        // this group so rows align across entries
                        // while the panel stays content-sized: short
                        // descriptions don't pad it out, long ones
                        // truncate at DESC_COL_MAX.
                        let name_font = egui::TextStyle::Monospace.resolve(ui.style());
                        let body_font = egui::TextStyle::Body.resolve(ui.style());
                        let text_color = ui.visuals().text_color();
                        let measured_name_w = prompt
                            .entries
                            .iter()
                            .map(|e| {
                                ui.fonts_mut(|f| {
                                    f.layout_no_wrap(e.name.clone(), name_font.clone(), text_color).size().x
                                })
                            })
                            .fold(0.0_f32, f32::max);
                        let measured_desc_w = prompt
                            .entries
                            .iter()
                            .filter_map(|e| e.description.as_deref())
                            .map(|d| {
                                ui.fonts_mut(|f| f.layout_no_wrap(d.to_owned(), body_font.clone(), text_color).size().x)
                            })
                            .fold(0.0_f32, f32::max);
                        let name_col_w = measured_name_w.min(NAME_COL_MAX);
                        let desc_col_w = measured_desc_w.min(DESC_COL_MAX);
                        // Compute the panel's content width up front
                        // so the Dismiss row's right-alignment sits
                        // flush with the row above instead of
                        // sprawling across the parent Area's
                        // available_width.
                        let gap = ui.spacing().item_spacing.x;
                        let mut panel_w = name_col_w + gap + RUN_BUTTON_W;
                        if desc_col_w > 0.0 {
                            panel_w += desc_col_w + gap;
                        }
                        ui.set_max_width(panel_w);
                        let mut accepted = false;
                        ui.horizontal(|ui| {
                            ui.label(egui_phosphor::regular::PUZZLE_PIECE);
                            ui.label(RichText::new(hxy_i18n::t("toast-template-group-title")).strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                // Dim the icon at rest so the theme's
                                // brighter hovered fg_stroke (TEXT_BRIGHT)
                                // reads as a clear "this is interactive
                                // and you're aiming at it" cue. Scoped
                                // so the override doesn't leak.
                                ui.scope(|ui| {
                                    let weak = ui.visuals().weak_text_color();
                                    ui.style_mut().visuals.widgets.inactive.fg_stroke.color = weak;
                                    if ui
                                        .add(egui::Button::new(egui_phosphor::regular::X).frame(false))
                                        .on_hover_text(hxy_i18n::t("toast-template-dismiss"))
                                        .clicked()
                                    {
                                        accepted = true;
                                    }
                                });
                            });
                        });
                        ui.add_space(6.0);
                        for (idx, entry) in prompt.entries.iter().enumerate() {
                            ui.push_id(idx, |ui| {
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [name_col_w, ROW_HEIGHT],
                                        egui::Label::new(RichText::new(&entry.name).monospace())
                                            .selectable(false)
                                            .truncate(),
                                    );
                                    if desc_col_w > 0.0 {
                                        let desc = entry.description.as_deref().unwrap_or("");
                                        let desc_resp = ui.add_sized(
                                            [desc_col_w, ROW_HEIGHT],
                                            egui::Label::new(RichText::new(desc).weak()).selectable(false).truncate(),
                                        );
                                        if let Some(full) = entry.description.as_deref()
                                            && !full.is_empty()
                                            && measured_desc_w > DESC_COL_MAX
                                        {
                                            desc_resp.on_hover_text(full);
                                        }
                                    }
                                    if ui
                                        .add_sized(
                                            [RUN_BUTTON_W, ROW_HEIGHT],
                                            egui::Button::new(hxy_i18n::t("toast-template-run")),
                                        )
                                        .clicked()
                                    {
                                        pending_runs.push(PendingTemplateRun {
                                            file_id: prompt.file_id,
                                            template_path: entry.template_path.clone(),
                                        });
                                        accepted = true;
                                    }
                                });
                            });
                        }
                        accepted
                    })
                });
            if response.inner.inner {
                dismissed_groups.insert(prompt.group);
            }
            // Hover pauses the inactivity timer so a user reading
            // descriptions doesn't watch the panel disappear under
            // their cursor.
            if !response.response.hovered() {
                prompt.remaining -= dt;
            }
        }
        if !dismissed_groups.is_empty() {
            self.prompts.retain(|p| !dismissed_groups.contains(&p.group));
        }
    }
}
