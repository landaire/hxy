//! End-to-end test for the xbox-neighborhood plugin.
//!
//! Loads the built `wasm32-wasip2` component (if present), grants
//! it `commands` + the network allowlist its manifest requests, and
//! verifies the prompt-based connect flow surfaces correctly.
//!
//! Build the artifact first:
//!
//! ```sh
//! cd plugins/xbox-neighborhood && cargo build --target wasm32-wasip2 --release
//! ```
//!
//! Skipped automatically when the artifact isn't present so a fresh
//! checkout's `cargo test` stays green.

use std::path::PathBuf;
use std::sync::Arc;

use hxy_plugin_host::InMemoryStateStore;
use hxy_plugin_host::InvokeOutcome;
use hxy_plugin_host::PermissionGrants;
use hxy_plugin_host::PluginGrants;
use hxy_plugin_host::PluginKey;
use hxy_plugin_host::StateStore;
use hxy_vfs::VfsHandler;

fn component_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/xbox-neighborhood/target/wasm32-wasip2/release/hxy_xbox_neighborhood.wasm")
}

fn load_plugin() -> Option<hxy_plugin_host::PluginHandler> {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return None;
    }
    let bytes = std::fs::read(&path).expect("read component");

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("xbox-neighborhood.wasm"), &bytes).expect("write wasm");
    let manifest = r#"
[plugin]
name = "xbox-neighborhood"
version = "0.1.0"

[permissions]
commands = true
network = ["*:730"]
"#;
    std::fs::write(dir.path().join("xbox-neighborhood.hxy.toml"), manifest).expect("write manifest");

    let mut grants = PluginGrants::default();
    let key = PluginKey::from_bytes("xbox-neighborhood", "0.1.0", &bytes);
    grants.set(key, PermissionGrants { persist: false, commands: true, network: vec!["*:730".to_string()] });

    let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
    // tempdir must outlive the load; leak the path lifetime by
    // pulling the bytes back into a fresh load using std::mem::forget
    // would be ugly. Instead, copy the manifest+wasm into a longer-
    // lived dir under the host tempdir lifetime here:
    let handlers = hxy_plugin_host::load_plugins_from_dir(dir.path(), &grants, Some(store)).expect("load plugins");
    // Move the dir guard into the returned handler's closure so the
    // tempdir survives until the test's plugin handle drops. The
    // handler doesn't actually need the on-disk files after load
    // (the bytes are in-memory in the wasmtime Component) so we can
    // safely let `dir` drop here.
    drop(dir);
    handlers.into_iter().find(|p| p.name() == "xbox-neighborhood")
}

#[test]
fn surfaces_connect_command() {
    let Some(plugin) = load_plugin() else { return };

    let commands = plugin.list_commands();
    assert_eq!(commands.len(), 1, "expected single connect command, got {commands:#?}");
    let cmd = &commands[0];
    assert_eq!(cmd.id, "connect");
    assert_eq!(cmd.label, "Connect to Xbox console");
}

#[test]
fn invoke_returns_prompt_for_host_port() {
    let Some(plugin) = load_plugin() else { return };

    let outcome = plugin.invoke_command("connect").expect("invoke connect");
    match outcome {
        InvokeOutcome::Prompt(prompt) => {
            assert_eq!(prompt.title, "Xbox console (host:port)");
            assert_eq!(prompt.default_value.as_deref(), Some("192.168.1.50:730"));
        }
        other => panic!("expected Prompt outcome, got {other:?}"),
    }
}

#[test]
fn respond_to_unreachable_address_yields_error_cascade() {
    let Some(plugin) = load_plugin() else { return };

    // 198.51.100.0/24 is RFC 5737 TEST-NET-2 -- guaranteed
    // unroutable, so the unicast probe times out without an
    // answer and we should see a "no response" cascade entry
    // rather than a panic, a hang, or a permission failure.
    let outcome = plugin.respond_to_prompt("connect", "198.51.100.42:730").expect("respond connect");
    match outcome {
        InvokeOutcome::Cascade(entries) => {
            assert_eq!(entries.len(), 1, "got {entries:#?}");
            let entry = &entries[0];
            assert!(
                entry.id == "noop:probe-error" || entry.id.starts_with("console:"),
                "unexpected entry id {entry:#?}"
            );
        }
        other => panic!("expected Cascade outcome, got {other:?}"),
    }
}
