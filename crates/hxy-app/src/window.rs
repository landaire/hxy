//! Persisted window geometry, modelled after `egui_winit::WindowSettings`
//! and adapted from lantia-locator. Multiplies by the current zoom factor
//! when capturing so we store "native logical size at zoom = 1.0"; without
//! this the window shrinks on every launch because eframe applies the zoom
//! factor *again* when constructing the window from `ViewportBuilder`.

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    pub inner_size_points: Option<[f32; 2]>,
    pub outer_position_pixels: Option<[f32; 2]>,
    pub fullscreen: bool,
    pub maximized: bool,
}

impl WindowSettings {
    pub fn from_viewport_info(info: &egui::ViewportInfo, zoom_factor: f32) -> Self {
        Self {
            inner_size_points: info.inner_rect.map(|r| [r.width() * zoom_factor, r.height() * zoom_factor]),
            outer_position_pixels: info.outer_rect.map(|r| [r.left(), r.top()]),
            fullscreen: info.fullscreen.unwrap_or(false),
            maximized: info.maximized.unwrap_or(false),
        }
    }

    pub fn apply_to_builder(&self, builder: egui::ViewportBuilder, default_size: [f32; 2]) -> egui::ViewportBuilder {
        let size = self.inner_size_points.unwrap_or(default_size);
        let mut builder = builder.with_inner_size(size);
        if let Some(pos) = self.outer_position_pixels {
            builder = builder.with_position(egui::pos2(pos[0], pos[1]));
        }
        if self.fullscreen {
            builder.with_fullscreen(true)
        } else if self.maximized {
            builder.with_maximized(true)
        } else {
            builder
        }
    }
}
