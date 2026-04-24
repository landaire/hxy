//! Template runtime abstraction. [`TemplateRuntime`] and
//! [`ParsedTemplate`] are traits so the app can mix native in-process
//! runtimes (no sandboxing — we trust our own code) with
//! user-installed WASM component plugins (sandboxed via wasmtime).
//!
//! The WASM impls ([`WasmTemplateRuntime`] / [`WasmParsedTemplate`])
//! live here. Native impls live in whichever crate owns the language
//! implementation.

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

pub use crate::bindings::template_world::exports::hxy::vfs::template::Arg;
pub use crate::bindings::template_world::exports::hxy::vfs::template::ArgValue;
pub use crate::bindings::template_world::exports::hxy::vfs::template::DeferredArray;
pub use crate::bindings::template_world::exports::hxy::vfs::template::Diagnostic;
pub use crate::bindings::template_world::exports::hxy::vfs::template::DisplayHint;
pub use crate::bindings::template_world::exports::hxy::vfs::template::Node;
pub use crate::bindings::template_world::exports::hxy::vfs::template::NodeType;
pub use crate::bindings::template_world::exports::hxy::vfs::template::ResultTree;
pub use crate::bindings::template_world::exports::hxy::vfs::template::ScalarKind;
pub use crate::bindings::template_world::exports::hxy::vfs::template::Severity;
pub use crate::bindings::template_world::exports::hxy::vfs::template::Span;
pub use crate::bindings::template_world::exports::hxy::vfs::template::Value;

/// Canonical display name for a [`ScalarKind`] — matches 010 Editor
/// spelling (uchar / uint32 / float) since that's the template language
/// most hxy users will have typed for themselves. Plugins emit the
/// typed enum; this decides what shows up in the UI column.
pub fn scalar_kind_name(k: ScalarKind) -> &'static str {
    match k {
        ScalarKind::U8K => "uchar",
        ScalarKind::U16K => "ushort",
        ScalarKind::U32K => "uint32",
        ScalarKind::U64K => "uint64",
        ScalarKind::S8K => "char",
        ScalarKind::S16K => "int16",
        ScalarKind::S32K => "int32",
        ScalarKind::S64K => "int64",
        ScalarKind::F32K => "float",
        ScalarKind::F64K => "double",
        ScalarKind::BoolK => "bool",
        ScalarKind::BytesK => "bytes",
        ScalarKind::StringK => "string",
    }
}

/// Format a [`NodeType`] for UI display. Scalars use [`scalar_kind_name`];
/// user-defined kinds print their declared name; arrays append `[count]`.
pub fn node_type_label(ty: &NodeType) -> String {
    match ty {
        NodeType::Scalar(k) => scalar_kind_name(*k).to_owned(),
        NodeType::ScalarArray((k, n)) => format!("{}[{n}]", scalar_kind_name(*k)),
        NodeType::StructType(name) => name.clone(),
        NodeType::StructArray((name, n)) => format!("{name}[{n}]"),
        NodeType::EnumType(name) => name.clone(),
        NodeType::EnumArray((name, n)) => format!("{name}[{n}]"),
        NodeType::Unknown(name) => name.clone(),
    }
}

/// A template-language runtime. Callers don't care whether the impl
/// is native Rust or a sandboxed WASM plugin — both answer the same
/// tokenize + parse + execute lifecycle.
pub trait TemplateRuntime: Send + Sync {
    /// Short identifier for logs / UI (e.g. `"010-bt"`).
    fn name(&self) -> &str;

    /// File extensions this runtime claims (no leading dot). Used by
    /// the app to route a template file to the right runtime.
    fn extensions(&self) -> &[String];

    /// Parse `template_source` and bind it to `source` (the data
    /// file the template reads from). Repeat `execute` / `expand_array`
    /// calls happen on the returned handle.
    fn parse(&self, source: Arc<dyn HexSource>, template_source: &str)
    -> Result<Arc<dyn ParsedTemplate>, HandlerError>;
}

/// A parsed template bound to a byte source.
pub trait ParsedTemplate: Send + Sync {
    /// Walk the parsed template against the bound source, emitting a
    /// [`ResultTree`] of nodes + diagnostics. Safe to call repeatedly;
    /// each call produces a fresh tree.
    fn execute(&self, args: &[Arg]) -> Result<ResultTree, HandlerError>;

    /// Materialise elements `[start, end)` of a deferred array. Native
    /// runtimes that don't emit deferred arrays return
    /// [`HandlerError::Unsupported`].
    fn expand_array(&self, array_id: u64, start: u64, end: u64) -> Result<Vec<Node>, HandlerError>;
}

/// WASM-component-backed runtime — the sandboxed path for user-installed
/// template plugins loaded off disk.
pub struct WasmTemplateRuntime {
    name: String,
    extensions: Vec<String>,
    engine: Engine,
    component: Component,
    linker: Arc<Linker<HostState>>,
}

impl WasmTemplateRuntime {
    pub fn new(engine: Engine, component: Component, linker: Arc<Linker<HostState>>) -> Result<Self, HandlerError> {
        let placeholder: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        let mut store = Store::new(&engine, HostState::new(placeholder));
        let runtime = WitTemplateRuntime::instantiate(&mut store, &component, &linker)
            .map_err(|e| HandlerError::Internal(format!("instantiate template-runtime: {e}")))?;
        let iface = runtime.hxy_vfs_template();
        let name = iface.call_name(&mut store).map_err(|e| HandlerError::Internal(format!("call name: {e}")))?;
        let extensions =
            iface.call_extensions(&mut store).map_err(|e| HandlerError::Internal(format!("call extensions: {e}")))?;
        Ok(Self { name, extensions, engine, component, linker })
    }
}

impl TemplateRuntime for WasmTemplateRuntime {
    fn name(&self) -> &str {
        &self.name
    }

    fn extensions(&self) -> &[String] {
        &self.extensions
    }

    fn parse(
        &self,
        source: Arc<dyn HexSource>,
        template_source: &str,
    ) -> Result<Arc<dyn ParsedTemplate>, HandlerError> {
        let mut store = Store::new(&self.engine, HostState::new(source));
        let runtime = WitTemplateRuntime::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| HandlerError::Internal(format!("instantiate template-runtime: {e}")))?;
        let resource = runtime
            .hxy_vfs_template()
            .parsed_template()
            .call_constructor(&mut store, template_source)
            .map_err(|e| HandlerError::Internal(format!("call parsed-template constructor: {e}")))?;
        Ok(Arc::new(WasmParsedTemplate { inner: Mutex::new(ParsedInner { store, runtime, resource }) }))
    }
}

/// Live parsed-template resource on the WASM side. Held behind a
/// [`Mutex`] because wasmtime stores need `&mut` access while the
/// trait takes `&self`.
pub(crate) struct WasmParsedTemplate {
    inner: Mutex<ParsedInner>,
}

struct ParsedInner {
    store: Store<HostState>,
    runtime: WitTemplateRuntime,
    resource: ResourceAny,
}

impl ParsedTemplate for WasmParsedTemplate {
    fn execute(&self, args: &[Arg]) -> Result<ResultTree, HandlerError> {
        let mut g = self.inner.lock().map_err(|_| HandlerError::Internal("template mutex poisoned".into()))?;
        let g = &mut *g;
        g.runtime
            .hxy_vfs_template()
            .parsed_template()
            .call_execute(&mut g.store, g.resource, args)
            .map_err(|e| HandlerError::Internal(format!("call execute: {e}")))
    }

    fn expand_array(&self, array_id: u64, start: u64, end: u64) -> Result<Vec<Node>, HandlerError> {
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
