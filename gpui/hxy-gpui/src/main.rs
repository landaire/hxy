use std::path::PathBuf;
use std::process::ExitCode;

use gpui::App;
use gpui::Bounds;
use gpui::WindowBounds;
use gpui::WindowOptions;
use gpui::prelude::*;
use gpui::px;
use gpui::size;
use gpui::component::Root;
use gpui::component::WindowExt;

mod assets;
mod console;
mod floating;
mod menu;
mod os_color;
mod palette;
mod panels;
mod patches;
mod persist;
mod plugins;
mod settings;
mod status;
mod templates;
mod theme;
mod watch;
mod workspace;

use workspace::Workspace;

/// Turn a platform "open URLs" string (as delivered to gpui's
/// `on_open_urls`) into a filesystem path. macOS hands over `file://`
/// URLs with percent-encoded bytes; a non-file URL has no path to open,
/// so it is skipped. Decoded bytes that are not valid UTF-8 are dropped
/// rather than guessed, since a mangled path can only open the wrong file.
fn parse_file_url(url: &str) -> Option<PathBuf> {
    let encoded = url.strip_prefix("file://")?;
    let decoded = percent_encoding::percent_decode_str(encoded).decode_utf8().ok()?;
    Some(PathBuf::from(decoded.into_owned()))
}

fn main() -> ExitCode {
    // Follow the OS language before any localized string can be emitted,
    // matching the egui app's startup (crates/hxy/src/main.rs). en-US is
    // the only bundled locale today, so runtime text is unchanged; the
    // call is what lets a future locale take effect.
    hxy_i18n::init_from_system_locale();

    // Full file-open UX also covers cmd-o (workspace.rs); every CLI path
    // argument opens the same way at startup (read, dedup, error-toast
    // on failure -- see `Workspace::build_initial`), so all of them open
    // as tabs, not just the first. `resolved_files` canonicalizes against
    // the CWD and drops missing paths so a forwarded path is CWD-neutral.
    let initial = hxy_ipc::cli::Cli::parse_args().resolved_files();

    // Single-instance: if another hxy is already running, hand it the file
    // list and exit WITHOUT opening a second window. An empty list means a
    // bare re-launch, which starts its own window (mirrors the egui app);
    // a failed connect is the normal "we are the first instance" path.
    if !initial.is_empty() && hxy_ipc::socket::try_send_to_running_instance(&initial).is_ok() {
        tracing::info!(count = initial.len(), "forwarded to running instance");
        return ExitCode::SUCCESS;
    }

    // Blocking pre-window load, like egui's `load_persistent_state`:
    // the workspace and its panels read settings at construction, so
    // the global must exist before the window opens.
    let boot = settings::load_blocking();
    let settings_failure = boot.failure;

    // macOS Finder "Open With" delivery. gpui owns the NSApplication and
    // dispatches `application:openURLs:` into this callback, which must be
    // registered on the `Application` before `run` starts AppKit. The
    // callback has no `cx` (it can fire before the window exists), so it
    // forwards parsed paths through a channel the workspace poll loop
    // drains (mirroring the socket receiver).
    let (open_urls_tx, open_urls_receiver) = std::sync::mpsc::channel::<Vec<PathBuf>>();
    let app = gpui::platform::application().with_assets(assets::Assets);
    app.on_open_urls(move |urls| {
        let paths: Vec<PathBuf> = urls.iter().filter_map(|url| parse_file_url(url)).collect();
        if !paths.is_empty() {
            let _ = open_urls_tx.send(paths);
        }
    });
    app.run(move |cx: &mut App| {
        // Bind the single-instance socket and hand the workspace a receiver
        // of forwarded path batches. A bind failure (stale lock, perms)
        // leaves the app running without accepting forwarded opens.
        let ipc_receiver = match hxy_ipc::socket::start_server() {
            Ok(rx) => Some(rx),
            Err(e) => {
                tracing::warn!(error = %e, "ipc: bind socket; forwarded opens disabled");
                None
            }
        };
        gpui::component::init(cx);
        // Must run after gpui::component::init (Theme global) and before
        // the window's sync_system_appearance so the first paint and all
        // later appearance toggles use the hxy theme pair.
        theme::init(cx);
        settings::init(cx, boot);
        // Reads the shared persist handle settings just installed, so
        // it must follow settings::init and precede the window (panels
        // and the palette read the plugin globals).
        plugins::init(cx);
        panels::register(cx);
        cx.set_global(templates::load_runtimes());
        cx.set_global(templates::load_library());
        workspace::init_keybindings(cx);
        menu::init_keybindings(cx);
        menu::init_global_actions(cx);
        cx.set_menus(menu::build_menus());
        let layout_path = persist::layout_path();
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        cx.open_window(
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            move |window, cx| {
                gpui::component::Theme::sync_system_appearance(Some(window), cx);
                os_color::sync(cx);
                let appearance_subscription = window.observe_window_appearance(|window, cx| {
                    gpui::component::Theme::sync_system_appearance(Some(window), cx);
                    os_color::sync(cx);
                });
                // Initial keyboard focus (active pane if a file loaded,
                // otherwise the workspace itself so cmd-o stays
                // reachable) is assigned by `Workspace`'s own first
                // render -- see its `focus_pending` field.
                //
                // gpui-component's Root must be the window's top-level
                // view (it manages the dialog/sheet/notification
                // layers); its render shows only this child, which is
                // all Task 2 needs -- no modal surfaces yet (toasts are
                // Task 6).
                let workspace = cx.new(|cx| Workspace::new(initial, appearance_subscription, layout_path, window, cx));
                // Route forwarded second-instance paths (socket) and macOS
                // "Open With" paths (gpui `on_open_urls`) into this running
                // workspace via an executor poll loop. The socket receiver is
                // `None` when the socket failed to bind; the open-urls
                // receiver is always present.
                workspace.update(cx, |ws, cx| ws.start_ipc(ipc_receiver, open_urls_receiver, window, cx));
                if let Some(failure) = settings_failure {
                    // Deferred like the boot-restore toasts in
                    // `Workspace::build_initial`: the Root notification
                    // layer does not exist until after this closure
                    // returns.
                    window.defer(cx, move |window, cx| {
                        let text = hxy_i18n::t(failure.toast_key());
                        window.push_notification(gpui::component::notification::Notification::warning(text), cx);
                    });
                }
                cx.new(|cx| Root::new(workspace, window, cx))
            },
        )
        .expect("open window");
        cx.activate(true);
    });
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::parse_file_url;
    use std::path::PathBuf;

    #[test]
    fn parses_percent_encoded_file_url() {
        assert_eq!(parse_file_url("file:///tmp/a%20b.bin"), Some(PathBuf::from("/tmp/a b.bin")));
    }

    #[test]
    fn parses_plain_file_url() {
        assert_eq!(parse_file_url("file:///tmp/plain.bin"), Some(PathBuf::from("/tmp/plain.bin")));
    }

    #[test]
    fn skips_non_file_url() {
        assert_eq!(parse_file_url("https://example.com/x.bin"), None);
    }

    /// `main` calls this at startup so the gpui app follows the OS
    /// language like the egui app. Referencing it here means a rename or
    /// signature change breaks the build rather than silently dropping
    /// the language selection; the returned locale is en-US in this build.
    #[test]
    fn system_locale_init_is_wired() {
        let picked = hxy_i18n::init_from_system_locale();
        assert_eq!(picked.language.as_str(), "en");
    }
}
