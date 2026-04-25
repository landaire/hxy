//! End-to-end test for the commands + state interfaces.
//!
//! Loads `plugins/test-statecmd` against a tempdir + an
//! `InMemoryStateStore`, with both `persist` and `commands`
//! granted. Verifies:
//!
//! 1. `list_commands` returns the entries the plugin declares.
//! 2. `invoke` round-trips each [`InvokeOutcome`] variant.
//! 3. The state import survives across invocations -- the
//!    plugin bumps a u32 counter on every call and we observe
//!    the new value in the next `list_commands` subtitle.
//! 4. The token round-trips through `mount-by-token` and the
//!    resulting VFS exposes the expected synthesized file.
//!
//! The component binary is produced by:
//!
//! ```sh
//! cd plugins/test-statecmd
//! cargo build --target wasm32-unknown-unknown --release
//! wasm-tools component new \
//!     target/wasm32-unknown-unknown/release/hxy_plugin_test_statecmd.wasm \
//!     -o target/test-statecmd.component.wasm
//! ```
//!
//! The test skips itself if that artifact is absent so a fresh
//! checkout's `cargo test` stays green.

use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use hxy_plugin_host::InMemoryStateStore;
use hxy_plugin_host::InvokeOutcome;
use hxy_plugin_host::PermissionGrants;
use hxy_plugin_host::PluginGrants;
use hxy_plugin_host::PluginKey;
use hxy_plugin_host::StateStore;
use hxy_vfs::VfsHandler;

fn component_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/test-statecmd/target/test-statecmd.component.wasm")
}

#[test]
fn commands_invoke_state_roundtrip() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read component");

    // Stage the .wasm + sidecar into a tempdir so the loader can
    // discover both the binary and a manifest declaring the
    // permissions the plugin needs.
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm_path = dir.path().join("test-statecmd.wasm");
    std::fs::write(&wasm_path, &bytes).expect("write wasm");
    let manifest = r#"
[plugin]
name = "test-statecmd"
version = "0.1.0"
description = "Test fixture: state + commands."

[permissions]
persist = true
commands = true
"#;
    std::fs::write(dir.path().join("test-statecmd.hxy.toml"), manifest).expect("write manifest");

    // Pre-grant everything the manifest asks for, otherwise the
    // host-side intersect would clamp the actual grants to empty
    // and the commands list would come back empty.
    let mut grants = PluginGrants::default();
    let key = PluginKey::from_bytes("test-statecmd", "0.1.0", &bytes);
    grants.set(key, PermissionGrants { persist: true, commands: true, network: vec![] });

    let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
    let handlers = hxy_plugin_host::load_plugins_from_dir(dir.path(), &grants, Some(store.clone()))
        .expect("load plugins");
    let plugin = handlers
        .into_iter()
        .find(|p| p.name() == "test-statecmd")
        .expect("test-statecmd handler present");

    // 1. Plugin's declared commands surface verbatim.
    let cmds = plugin.list_commands();
    let ids: Vec<&str> = cmds.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["done", "cascade", "mount", "prompt", "network"]);

    // The plugin's `Done outcome` subtitle reads the persisted
    // counter; with a fresh InMemoryStateStore it should start at 0.
    let done_subtitle = cmds
        .iter()
        .find(|c| c.id == "done")
        .and_then(|c| c.subtitle.as_deref())
        .expect("done subtitle present");
    assert_eq!(done_subtitle, "counter = 0");

    // 2. invoke returns the variant matching the command id and
    //    bumps the counter (visible to the host on the next list).
    let done = plugin.invoke_command("done").expect("invoke done");
    assert!(matches!(done, InvokeOutcome::Done));

    let cascade = plugin.invoke_command("cascade").expect("invoke cascade");
    let cascade_kids = match cascade {
        InvokeOutcome::Cascade(k) => k,
        other => panic!("expected cascade, got {other:?}"),
    };
    assert_eq!(cascade_kids.len(), 2);
    // The plugin embeds the post-bump counter into child labels
    // so we can check the bump happened mid-invoke.
    assert!(cascade_kids[0].label.contains("counter = 2"), "got {:?}", cascade_kids[0].label);

    let mount = plugin.invoke_command("mount").expect("invoke mount");
    let req = match mount {
        InvokeOutcome::Mount(r) => r,
        other => panic!("expected mount, got {other:?}"),
    };
    assert_eq!(req.token, "token-3");
    assert_eq!(req.title, "Test mount #3");

    // 3. The persisted counter is now visible to subsequent calls.
    let cmds2 = plugin.list_commands();
    let after_subtitle = cmds2
        .iter()
        .find(|c| c.id == "done")
        .and_then(|c| c.subtitle.as_deref())
        .expect("done subtitle present");
    assert_eq!(after_subtitle, "counter = 3");

    // 4. mount-by-token returns a VFS that exposes the token-named
    //    file. Drives the same path the app's `Mount` outcome
    //    uses to open a tab.
    let mount = plugin.mount_by_token(&req.token).expect("mount-by-token");
    let entries: Vec<String> = mount.fs.read_dir("/").expect("read root").collect();
    assert_eq!(entries, vec![format!("{}.txt", req.token)]);
    let meta = mount.fs.metadata(&format!("/{}.txt", req.token)).expect("metadata");
    assert_eq!(meta.len, req.token.len() as u64);

    // 5. invoke "prompt" returns a Prompt outcome carrying the
    //    plugin's title + default. Pretend the user typed
    //    "xenon-dev" and dispatch via respond_to_prompt; the
    //    plugin turns the answer into a Mount outcome whose
    //    token embeds the answer verbatim.
    let prompt_outcome = plugin.invoke_command("prompt").expect("invoke prompt");
    let prompt = match prompt_outcome {
        InvokeOutcome::Prompt(p) => p,
        other => panic!("expected prompt, got {other:?}"),
    };
    assert_eq!(prompt.title, "Token name");
    assert!(prompt.default_value.is_some(), "prompt should pre-fill a default");

    let respond_outcome = plugin.respond_to_prompt("prompt", "xenon-dev").expect("respond");
    let mount_req = match respond_outcome {
        InvokeOutcome::Mount(m) => m,
        other => panic!("expected mount after respond, got {other:?}"),
    };
    assert_eq!(mount_req.token, "from-prompt:xenon-dev");
    assert_eq!(mount_req.title, "Prompt answered: xenon-dev");
}

#[test]
fn network_grant_lets_plugin_open_tcp_connection() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read component");

    // Spin up a one-shot TCP echo server on a random local port
    // before we set up the plugin so the plugin's connect call
    // succeeds. The accept loop runs once and replies "pong"
    // when it sees "ping" -- the plugin sends "ping" verbatim.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).expect("read ping");
        assert_eq!(&buf, b"ping");
        stream.write_all(b"pong").expect("write pong");
        // Drop closes; the plugin's `read` returns the pending
        // bytes and then EOF.
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let wasm_path = dir.path().join("test-statecmd.wasm");
    std::fs::write(&wasm_path, &bytes).expect("write wasm");
    // Manifest declares the wildcard pattern + a tighter one
    // limited to the listener's localhost. Both are listed so
    // the test can show that the user only needs to grant the
    // narrower pattern for the connect to succeed.
    let manifest = format!(
        r#"
[plugin]
name = "test-statecmd"
version = "0.1.0"

[permissions]
persist = true
commands = true
network = ["127.0.0.1:{port}", "*:443"]
"#
    );
    std::fs::write(dir.path().join("test-statecmd.hxy.toml"), manifest).expect("write manifest");

    let mut grants = PluginGrants::default();
    let key = PluginKey::from_bytes("test-statecmd", "0.1.0", &bytes);
    // User grants only the specific localhost pattern -- not
    // the broader `*:443`. The host's intersect drops anything
    // the user didn't approve, so connecting to a port other
    // than `port` will be denied even though the manifest asked
    // for `*:443`.
    grants.set(
        key,
        PermissionGrants {
            persist: true,
            commands: true,
            network: vec![format!("127.0.0.1:{port}")],
        },
    );

    let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
    let handlers = hxy_plugin_host::load_plugins_from_dir(dir.path(), &grants, Some(store.clone()))
        .expect("load plugins");
    let plugin = handlers
        .into_iter()
        .find(|p| p.name() == "test-statecmd")
        .expect("test-statecmd handler present");

    // The "network" command emits a Prompt that asks for the
    // host:port. respond drives the connect / write / read on
    // the host side and stores the echo bytes in the state blob
    // so we can pull them out via the in-memory store.
    let prompt = match plugin.invoke_command("network").expect("invoke") {
        InvokeOutcome::Prompt(p) => p,
        other => panic!("expected prompt, got {other:?}"),
    };
    assert_eq!(prompt.title, "host:port to connect to");

    let answer = format!("127.0.0.1:{port}");
    let outcome = plugin.respond_to_prompt("network", &answer).expect("respond");
    assert!(matches!(outcome, InvokeOutcome::Done), "got {outcome:?}");

    let echoed = store.load("test-statecmd").unwrap().expect("plugin saved its echo");
    assert_eq!(&echoed[..], b"pong", "echo blob = {:?}", String::from_utf8_lossy(&echoed));
}

#[test]
fn network_pattern_outside_allowlist_is_denied() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read component");

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("test-statecmd.wasm"), &bytes).expect("write wasm");
    // Manifest only allows :443; user grants the same. The
    // plugin then tries to connect to :9999 which doesn't match
    // any granted pattern, so the host returns a denial that
    // the plugin saves as "ERR: ...".
    let manifest = r#"
[plugin]
name = "test-statecmd"
version = "0.1.0"

[permissions]
persist = true
commands = true
network = ["*:443"]
"#;
    std::fs::write(dir.path().join("test-statecmd.hxy.toml"), manifest).expect("write manifest");

    let mut grants = PluginGrants::default();
    let key = PluginKey::from_bytes("test-statecmd", "0.1.0", &bytes);
    grants.set(
        key,
        PermissionGrants {
            persist: true,
            commands: true,
            network: vec!["*:443".to_string()],
        },
    );

    let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
    let handlers = hxy_plugin_host::load_plugins_from_dir(dir.path(), &grants, Some(store.clone()))
        .expect("load plugins");
    let plugin = handlers
        .into_iter()
        .find(|p| p.name() == "test-statecmd")
        .expect("test-statecmd handler present");

    let _ = plugin.invoke_command("network").expect("invoke");
    let outcome = plugin.respond_to_prompt("network", "127.0.0.1:9999").expect("respond");
    assert!(matches!(outcome, InvokeOutcome::Done));

    let saved = store.load("test-statecmd").unwrap().expect("plugin saved its error");
    let saved_str = String::from_utf8_lossy(&saved);
    assert!(
        saved_str.starts_with("ERR: network permission denied"),
        "expected denial for out-of-allowlist port, got {saved_str:?}"
    );
}

#[test]
fn network_denied_returns_error_string() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read component");

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("test-statecmd.wasm"), &bytes).expect("write wasm");
    // Manifest declares network -- but grants withhold it. The
    // plugin's tcp.connect should return a "permission denied"
    // string; the test plugin saves "ERR: ..." into state, which
    // we verify here.
    let manifest = r#"
[plugin]
name = "test-statecmd"
version = "0.1.0"

[permissions]
persist = true
commands = true
network = ["*:*"]
"#;
    std::fs::write(dir.path().join("test-statecmd.hxy.toml"), manifest).expect("write manifest");

    let mut grants = PluginGrants::default();
    let key = PluginKey::from_bytes("test-statecmd", "0.1.0", &bytes);
    // Explicitly grant persist + commands but NOT network. The
    // empty network grant means no patterns are approved even
    // though the manifest asked for the wildcard.
    grants.set(key, PermissionGrants { persist: true, commands: true, network: vec![] });

    let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
    let handlers = hxy_plugin_host::load_plugins_from_dir(dir.path(), &grants, Some(store.clone()))
        .expect("load plugins");
    let plugin = handlers
        .into_iter()
        .find(|p| p.name() == "test-statecmd")
        .expect("test-statecmd handler present");

    // Drive the same prompt -> respond cycle. With network
    // denied, the plugin's tcp_roundtrip helper hits
    // `tcp::connect` which returns `Err("network permission
    // denied for ...")`; the plugin formats that as an "ERR:"
    // blob.
    let _ = plugin.invoke_command("network").expect("invoke");
    let outcome = plugin.respond_to_prompt("network", "127.0.0.1:9").expect("respond");
    assert!(matches!(outcome, InvokeOutcome::Done));

    let saved = store.load("test-statecmd").unwrap().expect("plugin saved its error");
    let saved_str = String::from_utf8_lossy(&saved);
    assert!(
        saved_str.starts_with("ERR: network permission denied"),
        "expected denial error, got {saved_str:?}"
    );
}

#[test]
fn denied_permissions_yield_empty_commands_and_denied_state() {
    let path = component_path();
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read component");

    let dir = tempfile::tempdir().expect("tempdir");
    let wasm_path = dir.path().join("test-statecmd.wasm");
    std::fs::write(&wasm_path, &bytes).expect("write wasm");
    let manifest = r#"
[plugin]
name = "test-statecmd"
version = "0.1.0"

[permissions]
persist = true
commands = true
"#;
    std::fs::write(dir.path().join("test-statecmd.hxy.toml"), manifest).expect("write manifest");

    // Empty grants -- nothing requested by the manifest gets through.
    let grants = PluginGrants::default();
    let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
    let handlers = hxy_plugin_host::load_plugins_from_dir(dir.path(), &grants, Some(store.clone()))
        .expect("load plugins");
    let plugin = handlers
        .into_iter()
        .find(|p| p.name() == "test-statecmd")
        .expect("test-statecmd handler present");

    // commands grant denied -> host short-circuits before calling
    // into the plugin at all.
    let cmds = plugin.list_commands();
    assert!(cmds.is_empty(), "expected empty commands list, got {cmds:?}");
    let invoke = plugin.invoke_command("done");
    assert!(invoke.is_none(), "expected None for ungated invoke, got {invoke:?}");

    // The InMemoryStateStore stays empty too -- the plugin never
    // got a chance to write because invoke was short-circuited,
    // and even if we'd called list_commands, the persist denial
    // from the host's StateHost impl would have made `state::save`
    // return `denied` from the plugin's perspective.
    assert_eq!(store.load("test-statecmd").unwrap(), None);
}
