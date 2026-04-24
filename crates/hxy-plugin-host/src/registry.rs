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

use crate::bindings::handler_world::Plugin;
use crate::bindings::template_world::TemplateRuntime as WitTemplateRuntime;
use crate::handler::PluginHandler;
use crate::host::HostState;
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
}

/// Load every `*.wasm` component in `dir` into a [`PluginHandler`].
/// Silently tolerates an absent directory (returns empty) -- hosts may
/// call this with a user-config path that doesn't exist yet.
pub fn load_plugins_from_dir(dir: &Path) -> Result<Vec<PluginHandler>, PluginLoadError> {
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
        let handler = load_single(&engine, linker.clone(), &path)?;
        handlers.push(handler);
    }
    Ok(handlers)
}

fn load_single(engine: &Engine, linker: Arc<Linker<HostState>>, path: &Path) -> Result<PluginHandler, PluginLoadError> {
    let bytes = std::fs::read(path).map_err(|source| PluginLoadError::ReadFile { path: path.to_path_buf(), source })?;
    let component = Component::new(engine, &bytes)
        .map_err(|source| PluginLoadError::Compile { path: path.to_path_buf(), source })?;
    PluginHandler::new(engine.clone(), component, linker)
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
