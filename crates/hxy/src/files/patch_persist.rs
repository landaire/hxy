//! egui-side alias for the unsaved-patch sidecar store. The store
//! itself is framework-agnostic and lives in
//! [`hxy_panels::files::patch_persist`]; this app supplies the edits
//! directory ([`crate::files::save::unsaved_edits_dir`]) at each call
//! site.

#![cfg(not(target_arch = "wasm32"))]

pub use hxy_panels::files::patch_persist::{
    DIGEST_MAX_BYTES, PatchSidecar, RestoreIntegrity, discard, load, sidecar_filename, sidecar_path, snapshot, store,
};
