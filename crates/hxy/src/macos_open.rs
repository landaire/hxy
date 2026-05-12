//! macOS Finder right-click "Open in hxy" / "Open With -> hxy"
//! handlers.
//!
//! Two routes feed the same channel:
//!
//! * **Apple Events** -- Launch Services dispatches `kAEOpenDocuments`
//!   to the running NSApp when the user picks hxy from the
//!   "Open With" submenu (or double-clicks a file whose default
//!   handler is hxy). winit installs its own `NSApplicationDelegate`,
//!   so we register an Apple-Event handler on
//!   `NSAppleEventManager` -- a parallel API that doesn't fight
//!   for the delegate slot. The handler runs on the main thread
//!   while winit's event loop is pumping, parses the event's
//!   direct-object descriptor for file URLs, and pushes them onto
//!   the shared inbox the app already drains every frame.
//!
//! * **NSServices** -- the "Open in hxy" entry in Finder's
//!   right-click "Services" submenu (declared in Info.plist's
//!   `NSServices` array) calls `openInHxy:userData:error:` on
//!   whatever object we register via `NSApp.setServicesProvider`.
//!   We read the file URLs from the supplied `NSPasteboard` and
//!   funnel them through the same inbox sender.
//!
//! Cold-start opens (Finder launches the .app with the file as
//! argv) are handled by the existing CLI / IPC plumbing in
//! `main.rs`; this module only covers the warm-start case where
//! hxy is already running.

#![allow(unsafe_code)]

use std::path::PathBuf;
use std::sync::OnceLock;

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

/// Shared sender into the app's "external open" inbox. Set once
/// during install, read by the ObjC handlers (which the runtime
/// calls on the main thread; no synchronization needed beyond the
/// OnceLock's atomic store/load).
static OPEN_SENDER: OnceLock<egui_inbox::UiInboxSender<Vec<PathBuf>>> = OnceLock::new();

/// Install both handlers and return the inbox the app should drain
/// alongside the IPC inbox. Idempotent across the process lifetime
/// -- a second call returns `None` because the first install
/// already owns the sender slot.
///
/// `ctx` is the egui context used to schedule a repaint whenever a
/// batch lands, so the next frame opens the new file without
/// waiting for the user to nudge the window.
pub fn install(ctx: &egui::Context) -> Option<egui_inbox::UiInbox<Vec<PathBuf>>> {
    if OPEN_SENDER.get().is_some() {
        return None;
    }
    let mtm = MainThreadMarker::new()?;
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    if OPEN_SENDER.set(sender).is_err() {
        return None;
    }

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
        let url_class: &AnyClass = class!(NSURL);
        let send_types = NSArray::from_slice(&[NSString::from_str("public.file-url").as_ref()]);
        let _ = url_class;
        app.registerServicesMenuSendTypes_returnTypes(&send_types, &NSArray::<NSString>::from_slice(&[]));
    }

    // Intentional leak: the registrations above hold raw pointers to
    // this object. Dropping it would cause use-after-free on the next
    // event dispatch. The process owns it for its full lifetime.
    Box::leak(Box::new(handler));

    Some(inbox)
}

fn push_paths(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    if let Some(sender) = OPEN_SENDER.get()
        && sender.send(paths).is_err()
    {
        tracing::warn!("macos_open: inbox dropped; cannot forward open request");
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
        /// URL's path, and forward the batch to the inbox.
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
        /// via the inbox no-op, not via the Services framework's
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
                // Down-cast: readObjectsForClasses:[NSURL.class] only
                // ever returns NSURL instances, so this is safe in
                // practice. `downcast_ref` does the runtime class
                // check and gives us a typed `&NSURL`.
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
    fn new() -> Retained<Self> {
        unsafe { msg_send![class!(HxyMacOpenHandler), new] }
    }
}

fn nsurl_to_pathbuf(url: &NSURL) -> Option<PathBuf> {
    let path: Retained<NSString> = url.path()?;
    Some(PathBuf::from(path.to_string()))
}
