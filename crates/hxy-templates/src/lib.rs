//! Desktop-only template subsystem shared by the hxy frontends:
//! builtin 010/ImHex runtimes, the template library and include
//! sandbox, and pure template-run helpers. Depends on
//! `hxy-plugin-host` (wasmtime), so it cannot build for wasm32.

pub mod breadcrumb;
pub mod builtin;
pub mod color;
pub mod format;
pub mod library;
pub mod run;
pub mod state;

/// Per-user data-directory component. Must match the app's
/// `APP_NAME` so every frontend shares the same plugin and
/// template directories.
pub(crate) const APP_NAME: &str = "hxy";

/// Directory holding user-installed WASM template runtimes
/// (compiled components). Distinct from [`user_templates_dir`],
/// which holds template *sources*. Shared by every frontend so
/// plugins installed once are visible everywhere.
pub fn user_template_plugins_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("template-plugins"))
}

/// Directory for user-authored template sources (`.bt` files). The
/// [`library::TemplateLibrary`] scans this for auto-detection, and
/// the runners sandbox `#include` resolution to it.
pub fn user_templates_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("templates"))
}
