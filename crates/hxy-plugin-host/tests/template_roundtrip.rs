//! End-to-end test for the template-runtime pipeline. Loads the
//! fixture runtime, parses a dummy template source, runs `execute`
//! against a small synthetic data buffer, then exercises
//! `expand-array` on the deferred array the runtime returned.

use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_plugin_host::Value;

fn component_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/fixture-template/target/fixture-template.component.wasm")
}

#[test]
fn fixture_template_execute_and_expand() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dir = path.parent().unwrap();
    let mut runtimes =
        hxy_plugin_host::load_template_runtimes_from_dir(dir).expect("load template runtimes");
    let runtime = runtimes.pop().expect("at least one runtime");
    assert_eq!(runtime.name(), "fixture");
    assert_eq!(runtime.extensions(), ["fixture".to_string()]);

    // Build a data buffer: magic=0xDEADBEEF, count=3, data = [0x01, 0x02, 0x03]
    let mut data = Vec::new();
    data.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
    data.extend_from_slice(&3u64.to_le_bytes());
    for v in [0x01u32, 0x02, 0x03] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(data));

    let parsed = runtime.parse(source, "").expect("parse template");
    let tree = parsed.execute(&[]).expect("execute");
    assert!(tree.diagnostics.is_empty(), "unexpected diagnostics: {:?}", tree.diagnostics);
    assert_eq!(tree.nodes.len(), 4);
    // Root has no parent; children 1..=3 point to root.
    assert_eq!(tree.nodes[0].parent, None);
    for child in &tree.nodes[1..] {
        assert_eq!(child.parent, Some(0));
    }
    // magic is u32 at offset 0
    match tree.nodes[1].value {
        Some(Value::U32Val(v)) => assert_eq!(v, 0xDEADBEEF),
        ref other => panic!("expected u32-val, got {other:?}"),
    }
    // count is u64 at offset 4
    match tree.nodes[2].value {
        Some(Value::U64Val(v)) => assert_eq!(v, 3),
        ref other => panic!("expected u64-val, got {other:?}"),
    }
    // data is a deferred array of 3 u32s
    let arr = tree.nodes[3].array.as_ref().expect("deferred array on data");
    assert_eq!(arr.count, 3);
    assert_eq!(arr.stride, 4);

    let elements = parsed.expand_array(arr.id, 0, 3).expect("expand array");
    assert_eq!(elements.len(), 3);
    let values: Vec<u32> = elements
        .iter()
        .map(|n| match n.value {
            Some(Value::U32Val(v)) => v,
            _ => panic!("element missing u32 value"),
        })
        .collect();
    assert_eq!(values, vec![0x01, 0x02, 0x03]);
}
