//! Toast / non-blocking prompt center for the desktop GUI.
//!
//! Wraps `egui_toast` for the simple info / success / warning bubbles
//! and adds a template-prompt path on top: when the user opens a
//! file we recognise, we surface a "Run X.bt" button as a sticky
//! toast. Several templates can match a single open; when the user
//! accepts one we close every sibling toast in the same group so
//! the screen doesn't stay cluttered with unanswered alternatives.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use egui::Align2;
use egui::Color32;
use egui::Frame;
use egui::Id;
use egui::Order;
use egui::Stroke;
use egui::vec2;
use egui_toast::Toast;
use egui_toast::ToastKind;
use egui_toast::ToastOptions;
use egui_toast::Toasts;

use crate::files::FileId;

/// One on-screen template prompt. Lives on the [`ToastCenter`] until
/// the user accepts or dismisses it (or its sibling does, in which
/// case the whole group goes away together).
#[derive(Clone, Debug)]
pub struct TemplatePrompt {
    /// Group key shared by every prompt produced for the same open.
    /// Today the file id, but kept opaque so cross-source prompts
    /// (drag-and-drop, plugin-mounted, ...) can share a group later.
    pub group: u64,
    /// File this prompt was raised for. Stays valid for the prompt's
    /// lifetime because the toast can only run while the file is
    /// still open -- closing the tab dismisses every prompt that
    /// targets it.
    pub file_id: FileId,
    /// Source path of the template that would run if the user accepts.
    /// Resolved against [`crate::templates::library::TemplateLibrary`]
    /// at draw time so a freshly downloaded corpus can populate
    /// new entries without restarting the app.
    pub template_path: PathBuf,
    /// Display label, e.g. `Run WAV.bt`. Pre-formatted at push time
    /// so the i18n lookup doesn't fire every frame.
    pub label: String,
    /// Tracks whether the prompt has been hovered yet. We keep a
    /// long inactivity TTL so unattended toasts eventually clear,
    /// matching the rest of egui_toast's behaviour.
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

pub struct ToastCenter {
    /// egui_toast handle for the simple text bubbles. Anchored at
    /// the top-right of the central area to mirror the previous
    /// egui-notify default and stay out of the dock-tab strip.
    inner: Toasts,
    /// Active template prompts. Re-rendered every frame as our own
    /// stacked Area widgets so we keep direct control over their
    /// lifecycle (group dismissal, accept-on-click).
    prompts: Vec<TemplatePrompt>,
    /// Group ids whose prompts should disappear at the next render.
    /// Filled when the user accepts one prompt in a multi-template
    /// match so the others (which are now stale) collapse.
    dismissed_groups: HashSet<u64>,
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
            dismissed_groups: HashSet::new(),
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

    /// Queue a "Run this template?" prompt for an open file. Multiple
    /// calls with the same `group` are siblings -- accepting one
    /// dismisses the rest. Duplicates (same group + same template
    /// path) are coalesced into a single entry.
    pub fn push_template_prompt(&mut self, group: u64, file_id: FileId, template_path: PathBuf, label: String) {
        if self.prompts.iter().any(|p| p.group == group && p.template_path == template_path) {
            return;
        }
        self.prompts.push(TemplatePrompt { group, file_id, template_path, label, remaining: PROMPT_TTL_SECONDS });
    }

    /// Drop every prompt targeting `file_id`. Called when a tab
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

        // TTL bookkeeping and group-dismiss housekeeping happens here
        // even though the actual prompt widgets are drawn elsewhere:
        // we want a prompt to expire even if its targeted tab isn't
        // currently focused (otherwise it'd survive forever waiting
        // to be rendered).
        if !self.dismissed_groups.is_empty() {
            let dismissed = std::mem::take(&mut self.dismissed_groups);
            self.prompts.retain(|p| !dismissed.contains(&p.group));
        }
        self.prompts.retain(|p| p.remaining > 0.0);
    }

    /// Render every template prompt targeting `file_id`, anchored to
    /// the top-right of `tab_rect`. Drains accepted prompts into
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
        let mut accepted_groups: HashSet<u64> = HashSet::new();
        let mut y_offset = 12.0_f32;
        for (idx, prompt) in self.prompts.iter_mut().enumerate() {
            if prompt.file_id != file_id {
                continue;
            }
            // Anchor each prompt individually with a screen-relative
            // fixed_pos derived from the tab rect, so the prompts
            // ride along when the dock layout shifts and stack
            // O(1) per row instead of reflowing the whole list.
            let area_id = Id::new("hxy_template_prompt").with((file_id.get(), idx));
            let pos = egui::pos2(tab_rect.right() - 12.0, tab_rect.top() + y_offset);
            let response = egui::Area::new(area_id)
                .order(Order::Foreground)
                .pivot(Align2::RIGHT_TOP)
                .fixed_pos(pos)
                .constrain_to(tab_rect)
                .show(ctx, |ui| {
                    Frame::window(ui.style()).stroke(Stroke::new(1.0, Color32::from_gray(80))).show(ui, |ui| {
                        ui.set_max_width(280.0);
                        ui.horizontal(|ui| {
                            ui.label(egui_phosphor::regular::PUZZLE_PIECE);
                            ui.label(&prompt.label);
                        });
                        ui.horizontal(|ui| {
                            if ui.button(hxy_i18n::t("toast-template-run")).clicked() {
                                pending_runs.push(PendingTemplateRun {
                                    file_id: prompt.file_id,
                                    template_path: prompt.template_path.clone(),
                                });
                                accepted_groups.insert(prompt.group);
                            }
                            if ui.button(hxy_i18n::t("toast-template-dismiss")).clicked() {
                                accepted_groups.insert(prompt.group);
                            }
                        });
                    });
                })
                .response;
            // Hover pauses the inactivity timer; matches egui_toast's
            // own dwell behaviour.
            if !response.hovered() {
                prompt.remaining -= dt;
            }
            y_offset += response.rect.height() + 6.0;
        }
        if !accepted_groups.is_empty() {
            self.prompts.retain(|p| !accepted_groups.contains(&p.group));
        }
    }
}
