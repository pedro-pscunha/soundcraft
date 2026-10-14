//! The plugin editor window: an `NSWindow` subclass (`SoundCraftPluginWindow`) that hands the key
//! events its plugin view does not take to SoundCraft's own window. See
//! [`crate::plugin_window`] for the behaviour.

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, NSObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSEvent, NSFloatingWindowLevel, NSResponder, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::cell::Cell;

/// Largest window side, in points.
const MAX_SIDE: f64 = 16_384.0;

/// The content size of a window made for a view that reports none.
pub const DEFAULT_CONTENT: NSSize = NSSize::new(400.0, 300.0);

define_class!(
    // SAFETY: `NSWindow` may be subclassed. The subclass has no instance variables and no `Drop`,
    // and overrides `keyDown:`, `keyUp:`, `flagsChanged:` and `canBecomeMainWindow` with the
    // signatures AppKit declares for them.
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SoundCraftPluginWindow"]
    struct HostWindow;

    impl HostWindow {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !forward(self.mtm(), event, Key::Down) {
                // SAFETY: NSWindow's own `keyDown:`, with the event AppKit passed in.
                let () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            if !forward(self.mtm(), event, Key::Up) {
                // SAFETY: as in `keyDown:`.
                let () = unsafe { msg_send![super(self), keyUp: event] };
            }
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            if !forward(self.mtm(), event, Key::Flags) {
                // SAFETY: as in `keyDown:`.
                let () = unsafe { msg_send![super(self), flagsChanged: event] };
            }
        }

        /// Never main: SoundCraft's own window stays the main window (and the key target) while
        /// a plugin window is key, as with an inspector panel.
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }
    }
);

#[derive(Clone, Copy)]
enum Key {
    Down,
    Up,
    Flags,
}

thread_local! {
    /// A forwarded event is being handled (a target that sends it back is not followed).
    static FORWARDING: Cell<bool> = const { Cell::new(false) };
}

/// Clears [`FORWARDING`] however the forwarded call ends.
struct Forwarding;

impl Drop for Forwarding {
    fn drop(&mut self) {
        FORWARDING.with(|f| f.set(false));
    }
}

/// Hands a key event that reached a plugin window (no view of the plugin took it) to the first
/// responder of SoundCraft's own window. False when there is none, or while a forwarded event is
/// being handled; the plugin window's own handling runs then.
fn forward(mtm: MainThreadMarker, event: &NSEvent, key: Key) -> bool {
    // A panic must never unwind into AppKit (and `catch_unwind` aborts on an Objective-C
    // exception, so the target's code runs inside `exception::catch`).
    let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if FORWARDING.with(Cell::get) {
            return false;
        }
        let Some(target) = target_window(mtm).and_then(|w| w.firstResponder()) else { return false };
        FORWARDING.with(|f| f.set(true));
        let _clear = Forwarding;
        let handled = objc2::exception::catch(std::panic::AssertUnwindSafe(|| match key {
            Key::Down => target.keyDown(event),
            Key::Up => target.keyUp(event),
            Key::Flags => target.flagsChanged(event),
        }));
        if let Err(e) = handled {
            log::error!("plugin window: a forwarded key threw an Objective-C exception: {}", e.map_or_else(|| "nil".to_string(), |e| e.to_string()));
        }
        true
    }));
    sent.unwrap_or_else(|_| {
        log::error!("plugin window: a panic while forwarding a key was stopped");
        false
    })
}

/// SoundCraft's window that gets the keys plugin windows pass on (see [`choose_target`]).
fn target_window(mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
    #[cfg(feature = "testing")]
    if let Some(w) = testing::target() {
        return Some(w);
    }
    let app = NSApplication::sharedApplication(mtm);
    choose_target(app.mainWindow(), app.orderedWindows(), |w| w.isVisible())
}

/// The window the keys a plugin window passes on go to, given the app's main window and its
/// windows front to back: the main window when it is SoundCraft's own (plugin windows never become
/// main), else the frontmost of SoundCraft's own windows that is `visible` and can be key; `None`
/// without one (AppKit's own handling runs then: a beep). Never a plugin's window: neither a
/// plugin window nor one a plugin opened itself (a CLAP plugin's floating window, a plugin's own
/// panel), which may be main or frontmost.
fn choose_target(
    main: Option<Retained<NSWindow>>,
    ordered: impl IntoIterator<Item = Retained<NSWindow>>,
    visible: impl Fn(&NSWindow) -> bool,
) -> Option<Retained<NSWindow>> {
    main.filter(|w| is_app_window(w)).or_else(|| ordered.into_iter().find(|w| is_app_window(w) && visible(w) && w.canBecomeKeyWindow()))
}

/// The class of winit's windows: SoundCraft's own (every egui viewport is one).
const APP_WINDOW_CLASS: &std::ffi::CStr = c"WinitWindow";

/// One of SoundCraft's own windows.
fn is_app_window(w: &NSWindow) -> bool {
    AnyClass::get(APP_WINDOW_CLASS).is_some_and(|c| w.isKindOfClass(c))
}

/// The content size a plugin window gives a view of size `s`: rounded, each side at most 16 384
/// points; `None` when `s` is no size (not finite, or a side under one point), as a view may
/// report.
pub fn content_size(s: NSSize) -> Option<NSSize> {
    (s.width.is_finite() && s.height.is_finite() && s.width >= 1.0 && s.height >= 1.0)
        .then(|| NSSize::new(s.width.min(MAX_SIDE).round(), s.height.min(MAX_SIDE).round()))
}

/// Resizes `window`'s content to `content`, keeping the window's top-left corner where it is
/// (no-op when the size is already that, or `content` is no size).
pub fn fit_content(window: &NSWindow, content: NSSize) {
    let Some(size) = content_size(content) else { return };
    autoreleasepool(|_| {
        let now = window.contentRectForFrameRect(window.frame());
        if (now.size.width - size.width).abs() < 0.5 && (now.size.height - size.height).abs() < 0.5 {
            return;
        }
        let top = now.origin.y + now.size.height;
        let new = NSRect::new(NSPoint::new(now.origin.x, top - size.height), size);
        window.setFrame_display(window.frameRectForContentRect(new), true);
    });
}

/// A plugin editor window (see [`crate::plugin_window`]). Holds one retain of the window.
pub struct PluginWindow {
    /// Never null; released in `Drop`.
    window: *mut NSWindow,
    /// Ordered front at least once (never, for a window kept hidden).
    shown: bool,
}

// SAFETY: the window is only touched on the main thread: every method takes a `MainThreadMarker`,
// and `Drop` releases it only there (elsewhere the retain is leaked). The struct merely carries
// the retain between threads' locks.
unsafe impl Send for PluginWindow {}

impl PluginWindow {
    /// A new window with `content` as its content size ([`content_size`]; [`DEFAULT_CONTENT`]
    /// when it is no size), not on screen until [`PluginWindow::show`].
    pub fn new(mtm: MainThreadMarker, title: &str, content: NSSize) -> PluginWindow {
        let size = content_size(content).unwrap_or(DEFAULT_CONTENT);
        let rect = NSRect::new(NSPoint::new(200.0, 200.0), size);
        // Plugin editors are transient windows; in particular not minimizable (a minimized window
        // becomes a Dock thumbnail, which made every editor visited look like another app).
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        // In a pool: what AppKit autoreleases while setting the window up (references to it
        // among them) goes now, not whenever the caller's pool drains.
        let window = autoreleasepool(|_| {
            let this = HostWindow::alloc(mtm).set_ivars(());
            // SAFETY: NSWindow's designated initializer on a fresh instance of the subclass, main
            // thread; a valid rect, style and backing.
            let window: Retained<HostWindow> =
                unsafe { msg_send![super(this), initWithContentRect: rect, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false] };
            let window: Retained<NSWindow> = Retained::into_super(window);
            // SAFETY: we keep our own retain and release it in `Drop`, so AppKit must not release
            // the window when the user closes it.
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str(title));
            window.setLevel(NSFloatingWindowLevel);
            window.setHidesOnDeactivate(true);
            window.setExcludedFromWindowsMenu(true);
            window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle);
            window.center();
            window
        });
        PluginWindow { window: Retained::into_raw(window), shown: false }
    }

    /// The window.
    pub fn window(&self, _mtm: MainThreadMarker) -> &NSWindow {
        // SAFETY: we hold a retain of this (non-null) window until `Drop`.
        unsafe { &*self.window }
    }

    /// The content view a plugin view goes in.
    pub fn content_view(&self, mtm: MainThreadMarker) -> Option<Retained<NSView>> {
        self.window(mtm).contentView()
    }

    /// Orders the window front and makes it key. AppKit runs the plugin view's own code meanwhile
    /// (it draws, and may become first responder): an Objective-C exception thrown there is caught
    /// and logged, never left to abort SoundCraft. The window counts as shown either way: if it
    /// did not reach the screen, it is hidden and closes as one the user closed.
    pub fn show(&mut self, mtm: MainThreadMarker) {
        let window = self.window(mtm);
        if let Err(e) = objc2::exception::catch(std::panic::AssertUnwindSafe(|| order_front(window))) {
            log::error!("plugin window: showing it threw an Objective-C exception: {}", e.map_or_else(|| "nil".to_string(), |e| e.to_string()));
        }
        self.shown = true;
    }

    /// The user closed the window. It also hides while SoundCraft is in the background
    /// (`hidesOnDeactivate`) and comes back with it, so hidden counts as closed only while the
    /// app is active; a window never shown was not closed.
    pub fn closed_by_user(&self, mtm: MainThreadMarker) -> bool {
        self.shown && !self.window(mtm).isVisible() && app_active(mtm)
    }

    /// Fits the window to a content size (see [`fit_content`]).
    pub fn fit(&self, mtm: MainThreadMarker, content: NSSize) {
        fit_content(self.window(mtm), content);
    }

    /// Takes the window off screen, closes and releases it.
    pub fn close(self, _mtm: MainThreadMarker) {
        drop(self);
    }
}

impl Drop for PluginWindow {
    fn drop(&mut self) {
        if MainThreadMarker::new().is_none() {
            log::warn!("plugin window: dropped off the main thread; the window is leaked");
            return;
        }
        autoreleasepool(|_| {
            // SAFETY: the retain taken in `new`, released exactly once here; main thread.
            if let Some(w) = unsafe { Retained::from_raw(self.window) } {
                w.orderOut(None);
                w.close();
            }
        });
    }
}

/// SoundCraft is the active app.
fn app_active(mtm: MainThreadMarker) -> bool {
    #[cfg(feature = "testing")]
    if let Some(active) = testing::app_active() {
        return active;
    }
    NSApplication::sharedApplication(mtm).isActive()
}

/// Puts `window` on screen and makes it key (`makeKeyAndOrderFront:`).
fn order_front(window: &NSWindow) {
    #[cfg(feature = "testing")]
    if let Some(f) = testing::order_front() {
        return f(window);
    }
    window.makeKeyAndOrderFront(None);
}

/// Seams for the main-thread tests, which never put a window on screen (and a hidden window is
/// never main or ordered front).
#[cfg(feature = "testing")]
pub mod testing {
    use objc2::rc::{Retained, Weak};
    use objc2_app_kit::NSWindow;
    use std::cell::{Cell, RefCell};

    thread_local! {
        static TARGET: RefCell<Option<Weak<NSWindow>>> = const { RefCell::new(None) };
        static ORDER_FRONT: Cell<Option<fn(&NSWindow)>> = const { Cell::new(None) };
        static ACTIVE: Cell<Option<bool>> = const { Cell::new(None) };
    }

    /// Whether plugin windows take SoundCraft for the active app (`None`: ask AppKit), which
    /// decides whether a hidden window was closed by the user or hid with the app.
    pub fn set_app_active(active: Option<bool>) {
        ACTIVE.with(|a| a.set(active));
    }

    pub(super) fn app_active() -> Option<bool> {
        ACTIVE.with(Cell::get)
    }

    /// Runs `f` where [`PluginWindow::show`](super::PluginWindow::show) would order the window on
    /// screen, inside the same guard (`None`: AppKit's `makeKeyAndOrderFront:` again). With a
    /// no-op, `show` marks a hidden window as shown.
    pub fn set_order_front(f: Option<fn(&NSWindow)>) {
        ORDER_FRONT.with(|o| o.set(f));
    }

    pub(super) fn order_front() -> Option<fn(&NSWindow)> {
        ORDER_FRONT.with(Cell::get)
    }

    /// Sends the keys plugin windows pass on to `window` (`None`: back to SoundCraft's window).
    pub fn set_key_target(window: Option<&NSWindow>) {
        TARGET.with(|t| *t.borrow_mut() = window.map(Weak::from));
    }

    pub(super) fn target() -> Option<Retained<NSWindow>> {
        TARGET.with(|t| t.borrow().as_ref().and_then(Weak::load))
    }

    /// The window the keys a plugin window passes on would go to, given the main window, the
    /// windows front to back and which of them count as visible (tests: hidden windows). SoundCraft's
    /// own windows are winit's (class `WinitWindow`).
    pub fn choose_target(main: Option<&NSWindow>, ordered: &[&NSWindow], visible: impl Fn(&NSWindow) -> bool) -> Option<Retained<NSWindow>> {
        super::choose_target(main.map(objc2::Message::retain), ordered.iter().map(|w| objc2::Message::retain(*w)), visible)
    }
}
