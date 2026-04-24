//! End-to-end test for the 010 Editor template runtime plugin.
//!
//! Loads `plugins/bt-runtime/target/bt-runtime.component.wasm` (if
//! built), parses a small hand-written template, runs it against a
//! synthetic byte buffer, and verifies the emitted tree.

use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_plugin_host::TemplateRuntime as _;
use hxy_plugin_host::Value;

fn component_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/bt-runtime/target/bt-runtime.component.wasm")
}

#[test]
fn executes_basic_template() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dir = path.parent().unwrap();
    let mut runtimes =
        hxy_plugin_host::load_template_runtimes_from_dir(dir).expect("load template runtimes");
    let runtime = runtimes.pop().expect("at least one runtime");
    assert_eq!(runtime.name(), "010-bt");
    assert_eq!(runtime.extensions(), ["bt".to_string()]);

    let template_source = r#"
LittleEndian();
uint32 magic;
uint16 count;
"#;

    let mut data = Vec::new();
    data.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
    data.extend_from_slice(&0x0102u16.to_le_bytes());
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(data));

    let parsed = runtime.parse(source, template_source).expect("parse template");
    let tree = parsed.execute(&[]).expect("execute");
    assert!(
        tree.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        tree.diagnostics
    );
    assert_eq!(tree.nodes.len(), 2, "expected two field nodes");

    // First node: magic u32 = 0xDEADBEEF
    assert_eq!(tree.nodes[0].name, "magic");
    match tree.nodes[0].value {
        Some(Value::U32Val(v)) => assert_eq!(v, 0xDEADBEEF),
        ref other => panic!("expected u32 magic, got {other:?}"),
    }
    // Second: count u16 = 0x0102
    assert_eq!(tree.nodes[1].name, "count");
    match tree.nodes[1].value {
        Some(Value::U16Val(v)) => assert_eq!(v, 0x0102),
        ref other => panic!("expected u16 count, got {other:?}"),
    }
}

#[test]
fn executes_typedef_struct_template() {
    let path = component_path();
    if !path.exists() {
        return;
    }
    let dir = path.parent().unwrap();
    let mut runtimes =
        hxy_plugin_host::load_template_runtimes_from_dir(dir).expect("load template runtimes");
    let runtime = runtimes.pop().expect("runtime");

    let template_source = r#"
LittleEndian();
typedef struct {
    uchar version;
    ushort flags;
    uint32 crc;
} HEADER;
HEADER header;
"#;

    let bytes = vec![0x01, 0x02, 0x00, 0xEF, 0xBE, 0xAD, 0xDE];
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    let parsed = runtime.parse(source, template_source).expect("parse");
    let tree = parsed.execute(&[]).expect("execute");

    assert!(tree.diagnostics.is_empty(), "diagnostics: {:?}", tree.diagnostics);
    // One parent HEADER node + three field children.
    assert_eq!(tree.nodes.len(), 4);
    assert_eq!(tree.nodes[0].name, "header");
    assert_eq!(tree.nodes[0].type_name, "HEADER");
    assert_eq!(tree.nodes[0].parent, None);
    for child in &tree.nodes[1..] {
        assert_eq!(child.parent, Some(0));
    }
}
