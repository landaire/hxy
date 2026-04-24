//! Generated wasmtime component bindings for the `hxy:vfs` worlds.
//!
//! Each world gets its own module so the generated `Plugin` /
//! `PluginRich` / `TemplateRuntime` top-level types don't collide.

pub mod handler_world {
    wasmtime::component::bindgen!({
        world: "plugin",
        path: "wit",
    });
}

pub mod rich_world {
    // Reuse the source interface from handler_world so the host
    // implements `SourceHost` once. State and commands are bound
    // here because they only exist in the rich world.
    wasmtime::component::bindgen!({
        world: "plugin-rich",
        path: "wit",
        with: {
            "hxy:vfs/source@0.1.0": super::handler_world::hxy::vfs::source,
            "hxy:vfs/handler@0.1.0": super::handler_world::exports::hxy::vfs::handler,
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
