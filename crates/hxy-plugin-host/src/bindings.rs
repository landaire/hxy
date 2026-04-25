//! Generated wasmtime component bindings for the `hxy:vfs` worlds.
//!
//! Each world gets its own module so the generated `Plugin` /
//! `TemplateRuntime` top-level types don't collide.

pub mod handler_world {
    wasmtime::component::bindgen!({
        world: "plugin",
        path: "wit",
        // Map the WIT `connection` resource to our concrete host
        // type so the generated trait methods take
        // `Resource<TcpConnection>` directly. Without this, bindgen
        // generates its own marker `Connection` type that isn't
        // useful for storing the underlying `TcpStream`.
        // wasmtime bindgen wants `interface.resource` (period before
        // the resource name) rather than `interface/resource`;
        // documented in wasmtime's bindgen!() rustdoc with the
        // example `"wasi:filesystem/types.descriptor"`.
        with: {
            "hxy:vfs/tcp@0.1.0.connection": super::super::host::TcpConnection,
        },
    });
}

pub mod template_world {
    // Reuse the `source` interface types generated for the handler
    // world so the host only implements `SourceHost` once.
    wasmtime::component::bindgen!({
        world: "template-runtime",
        path: "wit",
        with: {
            "hxy:vfs/source@0.1.0": super::handler_world::hxy::vfs::source,
        },
    });
}
