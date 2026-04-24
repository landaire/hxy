//! Validates the per-template WASM workflow end-to-end: the
//! `plugins/png-template` component parses a tiny hand-crafted PNG
//! (signature + IHDR + IEND) with no text template involved —
//! `parse("")` is effectively a no-op since the wasm *is* the template.

use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_plugin_host::TemplateRuntime as _;
use hxy_plugin_host::Value;

fn component_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/png-template/target/png-template.component.wasm")
}

/// Build a minimal valid PNG: 8-byte signature + IHDR (13 bytes of
/// payload) + IEND (0 bytes of payload). CRCs are set to zero — the
/// plugin reads them but doesn't validate.
fn tiny_png() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // IHDR: 13 bytes of payload.
    out.extend_from_slice(&13u32.to_be_bytes()); // length
    out.extend_from_slice(b"IHDR");
    out.extend_from_slice(&320u32.to_be_bytes()); // width
    out.extend_from_slice(&240u32.to_be_bytes()); // height
    out.push(8); // bit depth
    out.push(2); // color type — truecolor
    out.push(0); // compression
    out.push(0); // filter
    out.push(0); // interlace
    out.extend_from_slice(&0u32.to_be_bytes()); // crc (not validated)

    // IEND: 0 bytes of payload.
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"IEND");
    out.extend_from_slice(&0u32.to_be_bytes()); // crc

    out
}

#[test]
fn png_template_parses_signature_ihdr_and_iend() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dir = path.parent().unwrap();
    let mut plugins =
        hxy_plugin_host::load_template_plugins_from_dir(dir).expect("load template plugins");
    let plugin = plugins.pop().expect("at least one plugin");
    assert_eq!(plugin.name(), "png");
    assert_eq!(plugin.extensions(), ["png".to_string()]);

    let data: Arc<dyn HexSource> = Arc::new(MemorySource::new(tiny_png()));
    // The "template source" is empty — this plugin ignores it;
    // its parsing logic is all Rust baked into the wasm.
    let parsed = plugin.parse(data, "").expect("parse");
    let tree = parsed.execute(&[]).expect("execute");

    assert!(tree.diagnostics.is_empty(), "unexpected diagnostics: {:?}", tree.diagnostics);
    // Node 0 = root "PNG", node 1 = signature, node 2 = chunk IHDR.
    assert_eq!(tree.nodes[0].name, "PNG");
    assert_eq!(tree.nodes[1].name, "signature");
    assert!(tree.nodes.iter().any(|n| n.name == "chunk IHDR"));
    assert!(tree.nodes.iter().any(|n| n.name == "chunk IEND"));

    // Width / height / color_type should be present under IHDR.
    let width = tree
        .nodes
        .iter()
        .find(|n| n.name == "width")
        .expect("width node");
    match width.value {
        Some(Value::U32Val(v)) => assert_eq!(v, 320),
        ref other => panic!("expected u32 width, got {other:?}"),
    }
    let height = tree.nodes.iter().find(|n| n.name == "height").expect("height node");
    match height.value {
        Some(Value::U32Val(v)) => assert_eq!(v, 240),
        ref other => panic!("expected u32 height, got {other:?}"),
    }
}
