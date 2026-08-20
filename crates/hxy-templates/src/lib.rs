//! Desktop-only template subsystem shared by the hxy frontends:
//! builtin 010/ImHex runtimes, the template library and include
//! sandbox, and pure template-run helpers. Depends on
//! `hxy-plugin-host` (wasmtime), so it cannot build for wasm32.

pub mod builtin;
pub mod library;
pub mod run;

/// Per-user data-directory component. Must match the app's
/// `APP_NAME` so every frontend shares the same plugin and
/// template directories.
pub(crate) const APP_NAME: &str = "hxy";
