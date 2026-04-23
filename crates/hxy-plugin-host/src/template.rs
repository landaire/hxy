//! [`TemplateRuntime`] — a loaded template-language runtime component.
//! Mirrors [`PluginHandler`] but exposes the `template` interface
//! instead of `handler`. Each runtime handles one language (identified
//! by file extensions); the app routes template files by extension.

use std::sync::Arc;
use std::sync::Mutex;

use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::HandlerError;
use wasmtime::Engine;
use wasmtime::Store;
use wasmtime::component::Component;
use wasmtime::component::Linker;
use wasmtime::component::ResourceAny;

use crate::bindings::template_world::TemplateRuntime as WitTemplateRuntime;
use crate::host::HostState;

pub use crate::bindings::template_world::exports::hxy::vfs::template::{
    Arg, ArgValue, DeferredArray, Diagnostic, DisplayHint, Node, ResultTree, Severity, Span, Value,
};

/// A parsed template bound to a live byte source. Held behind a
/// [`Mutex`] because wasmtime stores need `&mut` access while the
/// consumer API is `&self`.
pub struct ParsedTemplate {
    inner: Mutex<ParsedInner>,
}

struct ParsedInner {
    store: Store<HostState>,
    runtime: WitTemplateRuntime,
    resource: ResourceAny,
}

impl ParsedTemplate {
    pub fn execute(&self, args: &[Arg]) -> Result<ResultTree, HandlerError> {
        let mut g = self.inner.lock().map_err(|_| HandlerError::Internal("template mutex poisoned".into()))?;
        let g = &mut *g;
        g.runtime
            .hxy_vfs_template()
            .parsed_template()
            .call_execute(&mut g.store, g.resource, args)
            .map_err(|e| HandlerError::Internal(format!("call execute: {e}")))
    }

    pub fn expand_array(&self, array_id: u64, start: u64, end: u64) -> Result<Vec<Node>, HandlerError> {
        let mut g = self.inner.lock().map_err(|_| HandlerError::Internal("template mutex poisoned".into()))?;
        let g = &mut *g;
        g.runtime
            .hxy_vfs_template()
            .parsed_template()
            .call_expand_array(&mut g.store, g.resource, array_id, start, end)
            .map_err(|e| HandlerError::Internal(format!("call expand-array: {e}")))?
            .map_err(|d| HandlerError::Malformed(format!("{}: {}", severity_label(d.severity), d.message)))
    }
}

fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

pub struct TemplateRuntime {
    name: String,
    extensions: Vec<String>,
    engine: Engine,
    component: Component,
    linker: Arc<Linker<HostState>>,
}

impl TemplateRuntime {
    pub fn new(
        engine: Engine,
        component: Component,
        linker: Arc<Linker<HostState>>,
    ) -> Result<Self, HandlerError> {
        let placeholder: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        let mut store = Store::new(&engine, HostState::new(placeholder));
        let runtime = WitTemplateRuntime::instantiate(&mut store, &component, &linker)
            .map_err(|e| HandlerError::Internal(format!("instantiate template-runtime: {e}")))?;
        let iface = runtime.hxy_vfs_template();
        let name =
            iface.call_name(&mut store).map_err(|e| HandlerError::Internal(format!("call name: {e}")))?;
        let extensions = iface
            .call_extensions(&mut store)
            .map_err(|e| HandlerError::Internal(format!("call extensions: {e}")))?;
        Ok(Self { name, extensions, engine, component, linker })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }

    /// Instantiate the runtime against `source` and parse
    /// `template_source`. Returns a handle the caller can execute
    /// repeatedly and use to expand deferred arrays.
    pub fn parse(&self, source: Arc<dyn HexSource>, template_source: &str) -> Result<ParsedTemplate, HandlerError> {
        let mut store = Store::new(&self.engine, HostState::new(source));
        let runtime = WitTemplateRuntime::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| HandlerError::Internal(format!("instantiate template-runtime: {e}")))?;
        let resource = runtime
            .hxy_vfs_template()
            .parsed_template()
            .call_constructor(&mut store, template_source)
            .map_err(|e| HandlerError::Internal(format!("call parsed-template constructor: {e}")))?;
        Ok(ParsedTemplate { inner: Mutex::new(ParsedInner { store, runtime, resource }) })
    }
}
