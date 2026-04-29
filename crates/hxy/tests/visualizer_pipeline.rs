//! End-to-end check that ImHex `[[hex::visualize(...)]]` flows
//! through the runtime adapter into the host's visualizer dispatch:
//! the canonical `hxy_visualize` attribute lands on the right node,
//! [`hxy_lib::visualizers::VisualizerSpec::parse`] decodes it back
//! into a kind + args list, and the `image` decoder turns a real
//! PNG into a non-empty texture-ready buffer.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_lib::visualizers::VisualizerKind;
use hxy_lib::visualizers::VisualizerSpec;
use hxy_plugin_host::TemplateRuntime;
use hxy_plugin_host::VISUALIZE_ATTR;

fn imhex_runtime() -> Arc<dyn TemplateRuntime> {
    hxy_lib::templates::builtin::builtins()
        .into_iter()
        .find(|r| r.extensions().iter().any(|e| e == "hexpat"))
        .expect("imhex runtime registered in builtins()")
}

#[test]
fn visualize_attribute_arrives_at_host_in_canonical_form() {
    // `data` is decorated with `hex::visualize("bitmap", "RGBA8", 1, 1)`.
    // The interp canonicalizes the key and packs the args; the host
    // splits the value back into a typed VisualizerKind + Vec<String>.
    let template = r#"
        u8 data[4] [[hex::visualize("bitmap", "RGBA8", 1, 1)]];
    "#;
    let bytes: Vec<u8> = vec![0xff, 0x80, 0x40, 0xff];
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes.clone()));
    let runtime = imhex_runtime();
    let parsed = runtime.parse(source, template).expect("parse");
    let tree = parsed.execute(&[]).expect("execute");

    let data_node = tree
        .nodes
        .iter()
        .find(|n| n.name == "data")
        .expect("data node");
    let raw = data_node
        .attributes
        .iter()
        .find_map(|(k, v)| (k == VISUALIZE_ATTR).then_some(v.as_str()))
        .expect("data has hxy_visualize attribute");

    let spec = VisualizerSpec::parse(raw).expect("parses");
    assert_eq!(spec.kind, VisualizerKind::Bitmap);
    assert_eq!(spec.args, vec!["RGBA8", "1", "1"]);
}

#[test]
fn image_visualizer_decodes_a_real_png() {
    // We don't ship the ImHex pattern corpus, but we do have a PNG
    // checked into the repo. Round-trip it through the `image` crate
    // (the same call the visualizer makes) and assert we get a valid
    // RGBA buffer back -- if this stops working the GUI image
    // visualizer would also stop working.
    let png_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("img/command_palette.png");
    assert!(png_path.is_file(), "test fixture missing: {}", png_path.display());
    let bytes = std::fs::read(&png_path).expect("read png");
    let img = image::load_from_memory(&bytes).expect("decode png");
    let (w, h) = img.to_rgba8().dimensions();
    assert!(w > 0 && h > 0, "decoded image had zero dimension: {w}x{h}");
}

#[test]
fn unknown_visualizer_kind_is_surfaced_not_dropped() {
    // A typo (or a future visualizer the host doesn't know about
    // yet) shouldn't disappear silently. The spec parser preserves
    // the raw name in `Unknown(...)` so the panel can render a
    // clear "not registered" message.
    let raw = "this_visualizer_does_not_exist";
    let spec = VisualizerSpec::parse(raw).expect("parses");
    assert!(matches!(spec.kind, VisualizerKind::Unknown(ref n) if n == raw));
}
