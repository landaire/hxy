//! Guest-side bindings for hxy plugins.
//!
//! Each submodule exposes one wit-bindgen-generated world:
//!
//! * [`handler`] — `hxy:vfs/plugin` — exports the [`handler`] interface
//!   (VFS format detection + mount) and imports [`source`](handler::source).
//! * [`template`] — `hxy:vfs/template-runtime` — exports the [`template`]
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
}

pub mod template {
    wit_bindgen::generate!({
        world: "template-runtime",
        path: "wit",
        export_macro_name: "export_template_runtime",
        pub_export_macro: true,
    });
}
