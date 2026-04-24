//! Guest-side bindings for hxy plugins.
//!
//! Each submodule exposes one wit-bindgen-generated world:
//!
//! * [`handler`] -- `hxy:vfs/plugin` -- exports the [`handler`] interface
//!   (VFS format detection + mount) and imports [`source`](handler::source).
//! * [`template`] -- `hxy:vfs/template-runtime` -- exports the [`template`]
//!   interface (parse + execute .bt / .hexpat-style templates) and imports
//!   the same [`source`](template::source).
//!
//! The two worlds share the same `source` interface in WIT but generate
//! independent Rust modules; a plugin will typically depend on only one.

#![no_std]

pub mod handler {
    wit_bindgen::generate!({
        world: "plugin",
        path: "wit",
        export_macro_name: "export_handler",
        pub_export_macro: true,
    });

    // Lift the exported handler interface's types and the imported
    // `source` / `state` interfaces up to the module root so plugins
    // can write `hxy_plugin_api::handler::FileType` instead of the
    // generated `...::exports::hxy::vfs::handler::FileType` path.
    pub use self::hxy::vfs::source;
    pub use self::hxy::vfs::state;
    pub use exports::hxy::vfs::handler::FileType;
    pub use exports::hxy::vfs::handler::Guest;
    pub use exports::hxy::vfs::handler::GuestMount;
    pub use exports::hxy::vfs::handler::Metadata;
    // Commands interface re-exports. `GuestCommands` is the trait
    // every plugin must implement (return empty list / unreachable
    // invoke when commands are not declared in the manifest).
    pub use exports::hxy::vfs::commands::Command;
    pub use exports::hxy::vfs::commands::Guest as GuestCommands;
    pub use exports::hxy::vfs::commands::InvokeResult;
    pub use exports::hxy::vfs::commands::MountRequest;
}

pub mod template {
    wit_bindgen::generate!({
        world: "template-runtime",
        path: "wit",
        export_macro_name: "export_template_runtime",
        pub_export_macro: true,
    });

    pub use self::hxy::vfs::source;
    pub use exports::hxy::vfs::template::Arg;
    pub use exports::hxy::vfs::template::ArgValue;
    pub use exports::hxy::vfs::template::DeferredArray;
    pub use exports::hxy::vfs::template::Diagnostic;
    pub use exports::hxy::vfs::template::DisplayHint;
    pub use exports::hxy::vfs::template::Guest;
    pub use exports::hxy::vfs::template::GuestParsedTemplate;
    pub use exports::hxy::vfs::template::Node;
    pub use exports::hxy::vfs::template::NodeType;
    pub use exports::hxy::vfs::template::ResultTree;
    pub use exports::hxy::vfs::template::ScalarKind;
    pub use exports::hxy::vfs::template::Severity;
    pub use exports::hxy::vfs::template::Span;
    pub use exports::hxy::vfs::template::Value;
}
