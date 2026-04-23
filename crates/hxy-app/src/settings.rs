//! User-visible application settings.

use hxy_core::ColumnCount;
use serde::Deserialize;
use serde::Serialize;

/// General application preferences that are safe to persist across sessions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// User's preferred language. `None` means follow the system locale at
    /// startup and don't override it here.
    pub language: Option<String>,

    /// egui zoom factor. 1.0 = native, 1.2 matches `lantia-locator`'s default.
    pub zoom_factor: f32,

    /// Number of hex columns per row in the hex view.
    pub hex_columns: ColumnCount,

    /// Whether the app should check for updates on launch (placeholder —
    /// wired up when we actually implement update checks).
    pub check_for_updates: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { language: None, zoom_factor: 1.2, hex_columns: ColumnCount::DEFAULT, check_for_updates: true }
    }
}
