//! Guest-side bindings for writing hxy WASM plugins.
//!
//! Two worlds are available as sibling modules, each with its own
//! `export_*!` macro:
//!
//! - [`handler`] — VFS format handlers (see [`handler::Guest`]).
//! - [`template`] — template-language runtimes (see
//!   [`template::Guest`]).
//!
//! A given plugin component usually implements exactly one world.

#![no_std]

extern crate alloc;

pub mod handler {
    wit_bindgen::generate!({
        world: "plugin",
        path: "../../wit",
        pub_export_macro: true,
        export_macro_name: "export_handler",
    });

    pub use self::exports::hxy::vfs::handler::FileType;
    pub use self::exports::hxy::vfs::handler::Guest;
    pub use self::exports::hxy::vfs::handler::GuestMount;
    pub use self::exports::hxy::vfs::handler::Metadata;
    pub use self::hxy::vfs::source;
    pub use self::hxy::vfs::state;
    // The plugin world exports `commands` alongside `handler`, so every
    // plugin component implements `GuestCommands` even when its manifest
    // doesn't request the `commands` permission (in which case
    // `list-commands` returns the empty list and `invoke` is unreachable).
    pub use self::exports::hxy::vfs::commands::Command;
    pub use self::exports::hxy::vfs::commands::Guest as GuestCommands;
    pub use self::exports::hxy::vfs::commands::InvokeResult;
    pub use self::exports::hxy::vfs::commands::MountRequest;
    pub use self::exports::hxy::vfs::commands::PromptRequest;
}

pub mod template {
    wit_bindgen::generate!({
        world: "template-runtime",
        path: "../../wit",
        pub_export_macro: true,
        export_macro_name: "export_template_runtime",
    });

    pub use self::exports::hxy::vfs::template::Arg;
    pub use self::exports::hxy::vfs::template::ArgValue;
    pub use self::exports::hxy::vfs::template::DeferredArray;
    pub use self::exports::hxy::vfs::template::Diagnostic;
    pub use self::exports::hxy::vfs::template::DisplayHint;
    pub use self::exports::hxy::vfs::template::Guest;
    pub use self::exports::hxy::vfs::template::GuestParsedTemplate;
    pub use self::exports::hxy::vfs::template::Node;
    pub use self::exports::hxy::vfs::template::NodeType;
    pub use self::exports::hxy::vfs::template::ResultTree;
    pub use self::exports::hxy::vfs::template::ScalarKind;
    pub use self::exports::hxy::vfs::template::Severity;
    pub use self::exports::hxy::vfs::template::Span;
    pub use self::exports::hxy::vfs::template::Value;
    pub use self::hxy::vfs::source;
}
