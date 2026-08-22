//! Reads the native OS byte-selection color and installs it as the
//! `hxy_view_gpui::OsSelectionColor` global, so the hex selection band
//! matches the platform selection color instead of the theme's own.
//! macOS only; other platforms leave the global cleared, and the theme
//! selection color stands.

use gpui::App;
use gpui::Hsla;
use hxy_view_gpui::OsSelectionColor;

/// Reads the current OS selection color and installs it as the global.
/// Call at startup and on every appearance change: the OS color resolves
/// differently in light and dark mode, so it must be re-read when the
/// system appearance flips. A `None` read clears any prior override.
pub fn sync(cx: &mut App) {
    cx.set_global(OsSelectionColor(read_os_selection_color()));
}

#[cfg(target_os = "macos")]
fn read_os_selection_color() -> Option<Hsla> {
    use gpui::Rgba;
    use objc2_app_kit::NSColor;
    use objc2_app_kit::NSColorSpace;

    // Called on the main thread (startup / appearance observer), where
    // AppKit color access is valid. `selectedTextBackgroundColor` can be
    // a catalog/named color, so convert to sRGB before reading
    // components (a direct component read on a non-RGB color raises).
    let color = NSColor::selectedTextBackgroundColor();
    let srgb = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    Some(Hsla::from(Rgba {
        r: srgb.redComponent() as f32,
        g: srgb.greenComponent() as f32,
        b: srgb.blueComponent() as f32,
        a: srgb.alphaComponent() as f32,
    }))
}

#[cfg(not(target_os = "macos"))]
fn read_os_selection_color() -> Option<Hsla> {
    None
}
