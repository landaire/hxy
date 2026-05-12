//! macOS Finder right-click "Open in hxy" / "Open With -> hxy"
//! handlers.
//!
//! Two routes feed the same buffer:
//!
//! * **Apple Events** -- Launch Services dispatches `kAEOpenDocuments`
//!   to the running NSApp when the user picks hxy from the
//!   "Open With" submenu (or double-clicks a file whose default
//!   handler is hxy). winit installs its own `NSApplicationDelegate`,
//!   so we register an Apple-Event handler on
//!   `NSAppleEventManager` -- a parallel API that doesn't fight
//!   for the delegate slot. The handler runs on the main thread
//!   while AppKit / winit are pumping the run loop.
//!
//! * **NSServices** -- the "Open in hxy" entry in Finder's
//!   right-click "Services" submenu (declared in Info.plist's
//!   `NSServices` array) calls `openInHxy:userData:error:` on
//!   whatever object we register via `NSApp.setServicesProvider`.
//!
//! Both routes push paths into a process-global buffer the app
//! drains every frame via [`drain_pending_paths`]. The buffer model
//! (instead of an `egui_inbox`) lets [`install`] run before
//! `eframe::run_native` -- crucial, because if we registered the
//! Apple Event handler during the eframe creator (which runs inside
//! winit's `applicationDidFinishLaunching`), we'd be replacing
//! AppKit's in-flight `_handleAEOpenEvent:` handler mid-dispatch on
//! cold-start launches with a document, which crashes winit's
//! ApplicationDelegate state machine.
//!
//! Cold-start opens still also flow through argv -> IPC when macOS
//! happens to pass the file via argv; this module covers the
//! AppleEvent path (which is how Launch Services delivers most
//! "Open With" invocations to a bundled .app).

#![allow(unsafe_code)]

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use objc2::ClassType;
use objc2::class;
use objc2::define_class;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::NSApplication;
use objc2_app_kit::NSPasteboard;
use objc2_foundation::MainThreadMarker;
use objc2_foundation::NSAppleEventDescriptor;
use objc2_foundation::NSAppleEventManager;
use objc2_foundation::NSArray;
use objc2_foundation::NSObject;
use objc2_foundation::NSObjectProtocol;
use objc2_foundation::NSString;
use objc2_foundation::NSURL;

/// Four-char OSType packed big-endian, the way CoreServices does
/// it. `osc(b"aevt") == kCoreEventClass`, etc.
const fn osc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const K_CORE_EVENT_CLASS: u32 = osc(b"aevt");
const K_AE_OPEN_DOCUMENTS: u32 = osc(b"odoc");
const KEY_DIRECT_OBJECT: u32 = osc(b"----");

/// Buffer of batched file paths the handlers have produced. The app
/// drains this every frame in `drain_external_open_requests`. Behind
/// a `Mutex` so we don't need to think about whether AppleEvent
/// callbacks could ever fire off the main thread (they don't today,
/// but the buffer also has to be readable from the app's update
/// loop without `RefCell` requiring `!Sync`).
static PENDING_PATHS: Mutex<Vec<Vec<PathBuf>>> = Mutex::new(Vec::new());

/// Set once, when the egui `Context` is first available, so the
/// AppleEvent / NSServices handlers can request a repaint after
/// pushing paths. Stays `None` until the eframe creator runs.
static REPAINT_CTX: OnceLock<egui::Context> = OnceLock::new();

/// Whether the handler ObjC object is already installed. Idempotent
/// across the process lifetime; a second [`install`] call is a no-op.
static INSTALLED: OnceLock<()> = OnceLock::new();

/// Register the Apple Event and NSServices handlers. MUST be called
/// before `eframe::run_native` so the handlers are in place before
/// `NSApplication::run` dispatches any cold-start Apple Events.
/// Returns `false` if not on macOS's main thread (`MainThreadMarker`
/// unavailable) or if a previous install already ran.
pub fn install() -> bool {
    if INSTALLED.get().is_some() {
        return false;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };

    let handler = HxyMacOpenHandler::new();
    let handler_obj: &AnyObject = handler.as_ref();

    // Apple Event handler: `kCoreEventClass` / `kAEOpenDocuments`.
    let mgr = NSAppleEventManager::sharedAppleEventManager();
    unsafe {
        mgr.setEventHandler_andSelector_forEventClass_andEventID(
            handler_obj,
            sel!(handleAppleEvent:withReplyEvent:),
            K_CORE_EVENT_CLASS,
            K_AE_OPEN_DOCUMENTS,
        );
    }

    // NSServices provider for the "Open in hxy" right-click entry.
    let app = NSApplication::sharedApplication(mtm);
    unsafe {
        app.setServicesProvider(Some(handler_obj));
        let send_types = NSArray::from_slice(&[NSString::from_str("public.file-url").as_ref()]);
        app.registerServicesMenuSendTypes_returnTypes(&send_types, &NSArray::<NSString>::from_slice(&[]));
    }

    // Intentional leak: the registrations above hold raw pointers to
    // this object. Dropping it would cause use-after-free on the next
    // event dispatch. The process owns it for its full lifetime.
    Box::leak(Box::new(handler));

    let _ = INSTALLED.set(());
    true
}

/// Plumb the egui [`Context`] in once it's available so subsequent
/// handler firings can request a repaint. Safe to call multiple times
/// -- only the first call takes effect.
pub fn wire_repaint_ctx(ctx: &egui::Context) {
    let _ = REPAINT_CTX.set(ctx.clone());
}

/// Drain whatever batches the handlers have accumulated since the
/// last call. Returns an empty `Vec` when nothing is pending.
/// Called from the per-frame open-request drain on macOS.
pub fn drain_pending_paths() -> Vec<Vec<PathBuf>> {
    PENDING_PATHS.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
}

fn push_paths(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    if let Ok(mut buf) = PENDING_PATHS.lock() {
        buf.push(paths);
    }
    if let Some(ctx) = REPAINT_CTX.get() {
        ctx.request_repaint();
    }
}

define_class!(
    /// ObjC class hosting both the Apple Event selector and the
    /// NSServices `openInHxy` selector. One class for both routes
    /// keeps the registration site colocated; the two methods don't
    /// share state.
    ///
    /// SAFETY: NSObject has no subclassing requirements; this type
    /// does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "HxyMacOpenHandler"]
    struct HxyMacOpenHandler;

    unsafe impl NSObjectProtocol for HxyMacOpenHandler {}

    impl HxyMacOpenHandler {
        /// NSAppleEventManager calls this with a populated event
        /// descriptor whose direct-object parameter is a list of
        /// file URL descriptors. We iterate the list, pull each
        /// URL's path, and forward the batch to the buffer.
        ///
        /// SAFETY: matches the AppleEvent handler signature
        /// `-(void)handleAppleEvent:withReplyEvent:` exactly.
        #[unsafe(method(handleAppleEvent:withReplyEvent:))]
        fn handle_apple_event(
            &self,
            event: &NSAppleEventDescriptor,
            _reply: &NSAppleEventDescriptor,
        ) {
            let Some(list) = event.paramDescriptorForKeyword(KEY_DIRECT_OBJECT) else {
                return;
            };
            let mut paths: Vec<PathBuf> = Vec::new();
            let count = list.numberOfItems();
            // NSAppleEventDescriptor lists are 1-indexed.
            for i in 1..=count {
                let Some(item) = list.descriptorAtIndex(i) else { continue };
                let Some(url) = item.fileURLValue() else { continue };
                if let Some(path) = nsurl_to_pathbuf(&url) {
                    paths.push(path);
                }
            }
            push_paths(paths);
        }

        /// NSServices entry point. The pasteboard carries one or
        /// more file URLs (we declared `public.file-url` as the
        /// only NSSendType in Info.plist). `userData` and the error
        /// out-param go unused -- failures here surface to the user
        /// via the buffer no-op, not via the Services framework's
        /// error reporting.
        ///
        /// SAFETY: matches the NSServices provider signature
        /// `-(void)openInHxy:userData:error:`.
        #[unsafe(method(openInHxy:userData:error:))]
        fn open_in_hxy(
            &self,
            pboard: &NSPasteboard,
            _user_data: *const NSString,
            _error: *mut *mut NSString,
        ) {
            let url_class: &AnyClass = class!(NSURL);
            let classes = NSArray::from_slice(&[url_class]);
            let Some(items) = (unsafe { pboard.readObjectsForClasses_options(&classes, None) })
            else {
                return;
            };
            let mut paths: Vec<PathBuf> = Vec::new();
            let count = items.count();
            for i in 0..count {
                let obj: Retained<AnyObject> = items.objectAtIndex(i);
                let Some(url) = obj.downcast_ref::<NSURL>() else { continue };
                if let Some(path) = nsurl_to_pathbuf(url) {
                    paths.push(path);
                }
            }
            push_paths(paths);
        }
    }
);

impl HxyMacOpenHandler {
    /// `+[HxyMacOpenHandler new]` allocates and initialises in one
    /// shot. Returns a `Retained<Self>` ready to register with
    /// NSAppleEventManager and NSApp.servicesProvider.
    ///
    /// Goes through `Self::class()` (not `class!()`) so the
    /// `define_class!`-generated class registration runs even when
    /// this is the first reference to the class in the process --
    /// `class!()` only does a runtime lookup and panics on miss.
    fn new() -> Retained<Self> {
        let cls = <Self as ClassType>::class();
        unsafe { msg_send![cls, new] }
    }
}

fn nsurl_to_pathbuf(url: &NSURL) -> Option<PathBuf> {
    let path: Retained<NSString> = url.path()?;
    Some(PathBuf::from(path.to_string()))
}
