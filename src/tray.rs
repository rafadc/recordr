//! Menu bar icon (NSStatusItem) with Open / Quit, and the Dock-vs-menu-bar activation policy.
//! Open unhides the app directly; Quit sets a flag that `App::ui` turns into a normal close.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering::Relaxed},
};

use eframe::egui;
use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained, runtime::AnyObject,
    runtime::NSObject, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSColor, NSImage, NSMenu, NSMenuItem,
    NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::NSString;

static QUIT: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RecordrMenuTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(open:))]
        fn open(&self, _sender: Option<&AnyObject>) {
            // egui doesn't run frames while hidden, so show it from here.
            show_windows();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            // Show the window so egui runs a frame, sees the flag, and closes (saving state).
            show_windows();
            QUIT.store(true, Relaxed);
            wake();
        }
    }
);

fn show_windows() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.unhide(None);
    for w in app.windows() {
        w.makeKeyAndOrderFront(None);
    }
    activate();
}

fn wake() {
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

pub fn take_quit() -> bool {
    QUIT.swap(false, Relaxed)
}

pub struct Tray {
    item: Retained<NSStatusItem>,
    _target: Retained<Target>, // menu items hold their target weakly
    recording: Option<bool>,
}

impl Tray {
    pub fn new(ctx: &egui::Context) -> Self {
        let _ = CTX.set(ctx.clone());
        let mtm = MainThreadMarker::new().expect("main thread");
        let target: Retained<Target> = unsafe { msg_send![Target::alloc(mtm), init] };
        let menu = NSMenu::new(mtm);
        for (title, action) in [("Open recordr", sel!(open:)), ("Quit recordr", sel!(quit:))] {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(action),
                    &NSString::new(),
                )
            };
            unsafe { item.setTarget(Some(&target)) };
            menu.addItem(&item);
        }
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        item.setMenu(Some(&menu));
        let mut tray = Tray {
            item,
            _target: target,
            recording: None,
        };
        tray.set_recording(false);
        tray
    }

    /// Waveform normally; a red record dot while any device records.
    pub fn set_recording(&mut self, on: bool) {
        if self.recording == Some(on) {
            return;
        }
        self.recording = Some(on);
        let mtm = MainThreadMarker::new().expect("main thread");
        let Some(button) = self.item.button(mtm) else {
            return;
        };
        let (symbol, label) = if on {
            ("record.circle.fill", "recordr: recording")
        } else {
            ("waveform", "recordr")
        };
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            Some(&NSString::from_str(label)),
        );
        button.setImage(image.as_deref());
        let red = NSColor::systemRedColor();
        button.setContentTintColor(on.then_some(&*red));
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

/// With a Dock icon (Regular) or menu bar only (Accessory).
pub fn set_dock(visible: bool) {
    let mtm = MainThreadMarker::new().expect("main thread");
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(policy);
}

/// Hide the whole app (like ⌘H): a Dock click or Open in the menu brings the window back.
pub fn hide() {
    let mtm = MainThreadMarker::new().expect("main thread");
    NSApplication::sharedApplication(mtm).hide(None);
}

/// Bring the app to the front (needed in Accessory mode, where it isn't activated by the Dock).
pub fn activate() {
    let mtm = MainThreadMarker::new().expect("main thread");
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}
