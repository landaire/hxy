//! Color type used by the template state model. Defined in
//! `hxy-core` (the persisted app state references it on every
//! target, including wasm builds that can't link this crate);
//! re-exported here so template consumers have a local path.

pub use hxy_core::color::Rgba;
