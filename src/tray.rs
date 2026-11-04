//! Menu bar icon (NSStatusItem) and the Dock-vs-menu-bar activation policy.
//! Left click shows/hides the app; right (or ctrl) click opens the Open / Quit menu.
//! Showing happens here directly (egui runs no frames while hidden); Quit sets a flag that
//! `App::handle_window` turns into a normal close.

use std::{
    cell::RefCell,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
};

use eframe::egui;
use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained, runtime::AnyObject,
    runtime::NSObject, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSColor, NSEventMask, NSEventModifierFlags,
    NSEventType, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::NSString;

static QUIT: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

thread_local! {
    // The live status item and its right-click menu, for the click handler.
    static ITEM: RefCell<Option<(Retained<NSStatusItem>, Retained<NSMenu>)>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RecordrMenuTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(clicked:))]
        fn clicked(&self, _sender: Option<&AnyObject>) {
            let app = NSApplication::sharedApplication(self.mtm());
            let right = app.currentEvent().is_some_and(|e| {
                e.r#type() == NSEventType::RightMouseUp
                    || e.modifierFlags().contains(NSEventModifierFlags::Control)
            });
            if right {
                // Attach the menu just for this click, so a left click stays a plain action.
                ITEM.with_borrow(|i| {
                    if let Some((item, menu)) = i {
                        item.setMenu(Some(menu));
                        if let Some(b) = item.button(self.mtm()) {
                            unsafe { b.performClick(None) };
                        }
                        item.setMenu(None);
                    }
                });
            } else if app.isActive() && app.windows().iter().any(|w| w.isVisible()) {
                hide();
            } else {
                show();
            }
        }

        #[unsafe(method(open:))]
        fn open(&self, _sender: Option<&AnyObject>) {
            // egui doesn't run frames while hidden, so show it from here.
            show();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            // Show the window so egui runs a frame, sees the flag, and closes (saving state).
            show();
            QUIT.store(true, Relaxed);
            wake();
        }
    }
);

/// Unhide the app and bring its window to the front.
pub fn show() {
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
        if let Some(b) = item.button(mtm) {
            unsafe {
                b.setTarget(Some(&target));
                b.setAction(Some(sel!(clicked:)));
            }
            b.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
        }
        ITEM.set(Some((item.clone(), menu)));
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
        ITEM.set(None);
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
/// `hide:` does nothing for an Accessory app (menu bar only), so there the windows are ordered
/// out instead. Not with a Dock icon: a Dock click unhides the app but wouldn't reorder them.
pub fn hide() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.hide(None);
    if app.activationPolicy() == NSApplicationActivationPolicy::Accessory {
        for w in app.windows() {
            w.orderOut(None);
        }
    }
}

/// Bring the app to the front (needed in Accessory mode, where it isn't activated by the Dock).
pub fn activate() {
    let mtm = MainThreadMarker::new().expect("main thread");
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}
