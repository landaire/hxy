//! Discovery & loading of plugin components from a directory.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use hxy_vfs::HandlerError;
use thiserror::Error;
use wasmtime::Config;
use wasmtime::Engine;
use wasmtime::component::Component;
use wasmtime::component::Linker;

use crate::ManifestError;
use crate::PluginGrants;
use crate::PluginKey;
use crate::PluginManifest;
use crate::StateStore;
use crate::bindings::handler_world::Plugin;
use crate::bindings::template_world::TemplateRuntime as WitTemplateRuntime;
use crate::handler::PluginHandler;
use crate::host::HostState;
use crate::manifest::Permissions;
use crate::template::WasmTemplateRuntime;

#[derive(Debug, Error)]
pub enum PluginLoadError {
    #[error("read plugin directory {path}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read plugin file {path}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configure wasmtime engine")]
    Engine(#[source] wasmtime::Error),
    #[error("compile component {path}")]
    Compile {
        path: PathBuf,
        #[source]
        source: wasmtime::Error,
    },
    #[error("link component {path}")]
    Link {
        path: PathBuf,
        #[source]
        source: wasmtime::Error,
    },
    #[error("instantiate {path} to probe name")]
    Probe {
        path: PathBuf,
        #[source]
        source: HandlerError,
    },
    #[error("read manifest sidecar")]
    Manifest(#[source] ManifestError),
}

/// Load every `*.wasm` component in `dir` into a [`PluginHandler`].
/// Silently tolerates an absent directory (returns empty) -- hosts may
/// call this with a user-config path that doesn't exist yet.
///
/// `grants` supplies per-plugin user consent decisions. `state_store`
/// is the shared on-disk persistence backend handed to plugins that
/// were granted `persist`; pass `None` when the host has no data dir
/// (state interface calls then return `denied`).
///
/// The loader pairs each plugin with its sidecar manifest (when
/// present), computes its [`PluginKey`], and stores the granted
/// permission set on the resulting [`PluginHandler`].
pub fn load_plugins_from_dir(
    dir: &Path,
    grants: &PluginGrants,
    state_store: Option<Arc<StateStore>>,
) -> Result<Vec<PluginHandler>, PluginLoadError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).map_err(PluginLoadError::Engine)?;

    let mut linker: Linker<HostState> = Linker::new(&engine);
    Plugin::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |s: &mut HostState| s)
        .map_err(PluginLoadError::Engine)?;
    let linker = Arc::new(linker);

    let read_dir =
        std::fs::read_dir(dir).map_err(|source| PluginLoadError::ReadDir { path: dir.to_path_buf(), source })?;

    let mut handlers = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|source| PluginLoadError::ReadDir { path: dir.to_path_buf(), source })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wasm") {
            continue;
        }
        let handler = load_single(&engine, linker.clone(), &path, grants, state_store.clone())?;
        handlers.push(handler);
    }
    Ok(handlers)
}

fn load_single(
    engine: &Engine,
    linker: Arc<Linker<HostState>>,
    path: &Path,
    grants: &PluginGrants,
    state_store: Option<Arc<StateStore>>,
) -> Result<PluginHandler, PluginLoadError> {
    let bytes = std::fs::read(path).map_err(|source| PluginLoadError::ReadFile { path: path.to_path_buf(), source })?;
    let manifest = PluginManifest::load_for(path).map_err(PluginLoadError::Manifest)?;

    // Use the manifest's name + version when present so the key
    // survives renames of the .wasm file. Falling back to the file
    // stem for the legacy / no-manifest case keeps a stable handle
    // for plugins that haven't shipped a sidecar yet.
    let (name, version, requested) = match &manifest {
        Some(m) => (m.plugin.name.clone(), m.plugin.version.clone(), m.permissions),
        None => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_owned();
            (stem, "0.0.0".to_owned(), Permissions::default())
        }
    };
    let key = PluginKey::from_bytes(name, version, &bytes);
    let granted = grants.get(&key).intersect(requested);

    let component = Component::new(engine, &bytes)
        .map_err(|source| PluginLoadError::Compile { path: path.to_path_buf(), source })?;
    PluginHandler::new(engine.clone(), component, linker, state_store, manifest, key, granted)
        .map_err(|source| PluginLoadError::Probe { path: path.to_path_buf(), source })
}

/// Load every `*.wasm` component in `dir` that implements the
/// `template-runtime` world. Components that don't match the world
/// are reported as errors rather than silently skipped -- the caller
/// should split template and handler plugin directories.
pub fn load_template_plugins_from_dir(dir: &Path) -> Result<Vec<WasmTemplateRuntime>, PluginLoadError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).map_err(PluginLoadError::Engine)?;

    let mut linker: Linker<HostState> = Linker::new(&engine);
    WitTemplateRuntime::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |s: &mut HostState| s)
        .map_err(PluginLoadError::Engine)?;
    let linker = Arc::new(linker);

    let read_dir =
        std::fs::read_dir(dir).map_err(|source| PluginLoadError::ReadDir { path: dir.to_path_buf(), source })?;

    let mut runtimes = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|source| PluginLoadError::ReadDir { path: dir.to_path_buf(), source })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wasm") {
            continue;
        }
        let runtime = load_template_single(&engine, linker.clone(), &path)?;
        runtimes.push(runtime);
    }
    Ok(runtimes)
}

fn load_template_single(
    engine: &Engine,
    linker: Arc<Linker<HostState>>,
    path: &Path,
) -> Result<WasmTemplateRuntime, PluginLoadError> {
    let bytes = std::fs::read(path).map_err(|source| PluginLoadError::ReadFile { path: path.to_path_buf(), source })?;
    let component = Component::new(engine, &bytes)
        .map_err(|source| PluginLoadError::Compile { path: path.to_path_buf(), source })?;
    WasmTemplateRuntime::new(engine.clone(), component, linker)
        .map_err(|source| PluginLoadError::Probe { path: path.to_path_buf(), source })
}

/// Compile an already-in-memory component into a [`WasmTemplateRuntime`].
/// The `label` is only used for error reporting -- typically a short
/// identifier like `"builtin:010-bt"`.
pub fn load_template_runtime_from_bytes(bytes: &[u8], label: &str) -> Result<WasmTemplateRuntime, PluginLoadError> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).map_err(PluginLoadError::Engine)?;

    let mut linker: Linker<HostState> = Linker::new(&engine);
    WitTemplateRuntime::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |s: &mut HostState| s)
        .map_err(PluginLoadError::Engine)?;
    let linker = Arc::new(linker);

    let label_path = PathBuf::from(label);
    let component = Component::new(&engine, bytes)
        .map_err(|source| PluginLoadError::Compile { path: label_path.clone(), source })?;
    WasmTemplateRuntime::new(engine, component, linker)
        .map_err(|source| PluginLoadError::Probe { path: label_path, source })
}
