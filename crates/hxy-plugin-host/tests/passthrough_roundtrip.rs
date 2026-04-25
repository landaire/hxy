//! End-to-end test for the bidirectional plugin interface. Loads the
//! `plugins/passthrough` sample component, mounts a synthetic byte
//! source, and verifies the plugin correctly calls back into the
//! host's `source.read` to materialise file contents.
//!
//! Build the artifact first:
//!
//! ```sh
//! cd plugins/passthrough && cargo build --target wasm32-wasip2 --release
//! ```
//!
//! `wasm32-wasip2` emits a component directly, so no `wasm-tools
//! component new` step is needed. The test skips itself gracefully
//! if the artifact isn't present so `cargo test` stays green on a
//! fresh checkout.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::VfsHandler;

fn component_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/passthrough/target/wasm32-wasip2/release/hxy_plugin_passthrough.wasm")
}

#[test]
fn passthrough_roundtrips_bytes_through_host_source() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dir = path.parent().unwrap();
    let grants = hxy_plugin_host::PluginGrants::default();
    let handlers = hxy_plugin_host::load_plugins_from_dir(dir, &grants, None).expect("load plugins");
    let handler = handlers
        .into_iter()
        .find(|h| h.name() == "passthrough")
        .expect("passthrough handler present among loaded plugins");

    assert!(handler.matches(&[0u8; 4]), "passthrough should match any head");

    let payload: Vec<u8> = (0u8..=255).collect();
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(payload.clone()));
    let mount = handler.mount(source).expect("mount via plugin");

    let entries: Vec<String> = mount.fs.read_dir("/").expect("read root").collect();
    assert_eq!(entries, vec!["data.bin".to_string()]);

    let meta = mount.fs.metadata("/data.bin").expect("metadata on data.bin");
    assert_eq!(meta.len, payload.len() as u64);

    let mut file = mount.fs.open_file("/data.bin").expect("open data.bin");
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).expect("read");
    assert_eq!(buf, payload);
}
