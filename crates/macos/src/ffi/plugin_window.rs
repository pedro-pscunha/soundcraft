//! The plugin editor window: an `NSWindow` subclass (`SoundCraftPluginWindow`) that hands the key
//! events its plugin view does not take to SoundCraft's own window. See
//! [`crate::plugin_window`] for the behaviour.

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, NSObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSFloatingWindowLevel, NSMenuItem, NSResponder, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSSize, NSString};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};

/// Largest window side, in points.
const MAX_SIDE: f64 = 16_384.0;

/// The content size of a window made for a view that reports none.
pub const DEFAULT_CONTENT: NSSize = NSSize::new(400.0, 300.0);

/// What a plugin window remembers of the keys it handed on, to let go of them (see
/// `HostWindow::let_go`). Main thread only, like the window.
#[derive(Default)]
struct Handed {
    /// Key-downs handed on whose key-up was not (one per key code).
    down: RefCell<Vec<Retained<NSEvent>>>,
    /// A modifier change was handed on since the window last let go.
    flags: Cell<bool>,
}

define_class!(
    // SAFETY: `NSWindow` may be subclassed. The instance variables (`Handed`) are Rust values
    // dropped with the window, and touched only on the main thread (the class is main-thread
    // only). The subclass overrides `keyDown:`, `keyUp:`, `flagsChanged:`, `performKeyEquivalent:`,
    // `resignKeyWindow`, `close` and `canBecomeMainWindow` with the signatures AppKit declares for
    // them, and calls NSWindow's own implementation where it hands nothing on.
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SoundCraftPluginWindow"]
    #[ivars = Handed]
    struct HostWindow;

    impl HostWindow {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            // ⌘W that the plugin did not take closes this editor (handed on, it would close
            // SoundCraft's window).
            if is_close_window(event) {
                guarded("closing it with ⌘W", || self.performClose(None));
                return;
            }
            // Tab and ⇧Tab that a focused plugin control passed up move the focus to the plugin's
            // next (previous) control: AppKit's key-view loop. When there is none to move to (no
            // view focused, or a view outside any loop, as a plugin's root view usually is) they
            // are handed on like any key.
            if is_tab(event) {
                let back = event.modifierFlags().contains(NSEventModifierFlags::Shift);
                // The controls' own code runs as AppKit asks them whether they take the focus, and
                // as it moves; if it throws, the key counts as taken.
                let moved = guarded("moving the focus with Tab", || {
                    let moves = self.key_view_after_focus(back).is_some();
                    if moves && back {
                        self.selectPreviousKeyView(None);
                    } else if moves {
                        self.selectNextKeyView(None);
                    }
                    moves
                });
                if moved != Some(false) {
                    return;
                }
            }
            // While SoundCraft has a text field focused, a key handed on would type into it unseen.
            if APP_WANTS_KEYBOARD.load(Ordering::Relaxed) || !forward(self.mtm(), event, Key::Down) {
                // SAFETY: NSWindow's own `keyDown:`, with the event AppKit passed in.
                let () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            }
            let mut down = self.ivars().down.borrow_mut();
            down.retain(|d| d.keyCode() != event.keyCode());
            down.push(event.retain());
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            if !forward(self.mtm(), event, Key::Up) {
                // SAFETY: as in `keyDown:`.
                let () = unsafe { msg_send![super(self), keyUp: event] };
                return;
            }
            self.ivars().down.borrow_mut().retain(|d| d.keyCode() != event.keyCode());
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            if !forward(self.mtm(), event, Key::Flags) {
                // SAFETY: as in `keyDown:`.
                let () = unsafe { msg_send![super(self), flagsChanged: event] };
                return;
            }
            self.ivars().flags.set(true);
        }

        /// A ⌘-key, before it arrives as `keyDown:`: the plugin's views first (NSWindow's own
        /// handling), then the Edit keys for the plugin's focused control (see `perform_edit`).
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let taken = guarded("offering a ⌘-key to the plugin", || {
                // SAFETY: NSWindow's own `performKeyEquivalent:`, with the event AppKit passed in.
                let taken: bool = unsafe { msg_send![super(self), performKeyEquivalent: event] };
                taken
            });
            // A plugin's code threw: the key counts as taken, so none of its code runs again for it.
            taken.unwrap_or(true) || self.perform_edit(event)
        }

        /// No longer key: the key-ups and modifier changes of the keys still held go elsewhere.
        #[unsafe(method(resignKeyWindow))]
        fn resign_key_window(&self) {
            // SAFETY: NSWindow's own `resignKeyWindow`.
            let () = unsafe { msg_send![super(self), resignKeyWindow] };
            self.let_go();
        }

        /// Closed (by the user, ⌘W, or its editor going): as when it resigns key.
        #[unsafe(method(close))]
        fn close(&self) {
            self.let_go();
            // SAFETY: NSWindow's own `close`.
            let () = unsafe { msg_send![super(self), close] };
        }

        /// Never main: SoundCraft's own window stays the main window (and the key target) while
        /// a plugin window is key, as with an inspector panel.
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }
    }
);

impl HostWindow {
    /// Hands SoundCraft a key-up for each key-down it was handed and whose key-up it was not, then
    /// the modifier state as it is now: once the window is no longer key (or gone), those key-ups
    /// and modifier changes go to another window or app, and SoundCraft would see the keys held
    /// down until they were pressed again.
    fn let_go(&self) {
        let down = std::mem::take(&mut *self.ivars().down.borrow_mut());
        if !self.ivars().flags.replace(false) && down.is_empty() {
            return;
        }
        let mtm = self.mtm();
        autoreleasepool(|_| {
            let now = NSEvent::modifierFlags_class();
            let time = NSProcessInfo::processInfo().systemUptime();
            for d in down {
                let (chars, plain) = (d.characters().unwrap_or_default(), d.charactersIgnoringModifiers().unwrap_or_default());
                if let Some(up) = key_event(NSEventType::KeyUp, now, time, d.windowNumber(), &chars, &plain, d.keyCode()) {
                    forward(mtm, &up, Key::Up);
                }
            }
            // Key code 0: no key, only the modifier state (winit reads it so).
            let none = NSString::new();
            if let Some(flags) = key_event(NSEventType::FlagsChanged, now, time, self.windowNumber(), &none, &none, 0) {
                forward(mtm, &flags, Key::Flags);
            }
        });
    }

    /// The view Tab (⇧Tab: `back`) would move the focus to: the focused view's next (previous)
    /// valid key view, when it has one other than itself. `None` with no view focused (the window
    /// itself is first responder) or a view outside any key-view loop (a plugin's root view
    /// usually is: VSTGUI's, JUCE's), where `selectNextKeyView:` would do nothing.
    fn key_view_after_focus(&self, back: bool) -> Option<Retained<NSView>> {
        let focused = self.firstResponder()?.downcast::<NSView>().ok()?;
        // SAFETY: the getters retain what they return.
        let next = unsafe { if back { focused.previousValidKeyView() } else { focused.nextValidKeyView() } }?;
        (!std::ptr::addr_eq(Retained::as_ptr(&next), Retained::as_ptr(&focused))).then_some(next)
    }

    /// ⌘C, ⌘V, ⌘X, ⌘Z, ⇧⌘Z and ⌘A for the plugin's focused control, as another app's Edit menu
    /// sends them (SoundCraft's menu bar has no key equivalents): the action goes to the first of
    /// the focused control and the responders after it, up to this window, that implements it, if
    /// it is enabled there. True when it was sent. False (the key goes on to `keyDown:`, and so
    /// to SoundCraft, as before) for a key aimed at another window, with no control focused (the
    /// window itself first responder), or when no responder implements the action or it is
    /// disabled (nothing selected to copy, nothing to undo).
    fn perform_edit(&self, event: &NSEvent) -> bool {
        let Some(action) = edit_action(event) else { return false };
        if event.windowNumber() != self.windowNumber() {
            return false;
        }
        let window: &NSWindow = self;
        let mtm = self.mtm();
        // The focused control's own code runs here (`respondsToSelector:`, validation, the action).
        guarded("an Edit key for the plugin's control", || {
            let Some(focused) = self.firstResponder().filter(|r| !std::ptr::addr_eq(Retained::as_ptr(r), std::ptr::from_ref(window))) else {
                return false;
            };
            let Some(target) = responder_for(focused, action, window) else { return false };
            // The menu item such an Edit menu would validate and send the action from.
            // SAFETY: a valid selector of an action method (`copy:` and the others take a sender).
            let item =
                unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::new(), Some(action), &NSString::new()) };
            // SAFETY: `target` implements `action`, an action method taking its sender, which the
            // item is.
            enabled(&target, &item) && unsafe { NSApplication::sharedApplication(mtm).sendAction_to_from(action, Some(&target), Some(&item)) }
        })
        // A plugin's code threw: the key counts as taken, as in `performKeyEquivalent:`.
        .unwrap_or(true)
    }
}

/// The Edit menu's action for a ⌘-key down: ⌘C `copy:`, ⌘V `paste:`, ⌘X `cut:`, ⌘Z `undo:`,
/// ⇧⌘Z `redo:`, ⌘A `selectAll:`. By the character the key types, so it follows the keyboard
/// layout as menu shortcuts do; no other modifier (Caps Lock aside).
fn edit_action(event: &NSEvent) -> Option<Sel> {
    if event.r#type() != NSEventType::KeyDown {
        return None;
    }
    let held = NSEventModifierFlags::Shift | NSEventModifierFlags::Control | NSEventModifierFlags::Option | NSEventModifierFlags::Command;
    let flags = event.modifierFlags() & held;
    let key = event.charactersIgnoringModifiers()?.to_string().to_ascii_lowercase();
    if flags == NSEventModifierFlags::Command {
        match key.as_str() {
            "c" => Some(sel!(copy:)),
            "v" => Some(sel!(paste:)),
            "x" => Some(sel!(cut:)),
            "z" => Some(sel!(undo:)),
            "a" => Some(sel!(selectAll:)),
            _ => None,
        }
    } else if flags == NSEventModifierFlags::Command | NSEventModifierFlags::Shift && key == "z" {
        Some(sel!(redo:))
    } else {
        None
    }
}

/// The first of `from` and the responders after it, up to and including `window`, that implements
/// `action`. A text field's undo is its window's (`undo:` is NSWindow's). At most 64 responders: a
/// chain that a plugin looped is not followed forever.
fn responder_for(from: Retained<NSResponder>, action: Sel, window: &NSWindow) -> Option<Retained<NSResponder>> {
    let mut next = Some(from);
    for _ in 0..64 {
        let r = next?;
        if r.respondsToSelector(action) {
            return Some(r);
        }
        if std::ptr::addr_eq(Retained::as_ptr(&r), std::ptr::from_ref(window)) {
            return None;
        }
        // SAFETY: the getter retains what it returns.
        next = unsafe { r.nextResponder() };
    }
    None
}

/// Whether `target` enables `item`'s action now, asked as a menu asks: `validateMenuItem:`, else
/// `validateUserInterfaceItem:`, else enabled.
fn enabled(target: &NSResponder, item: &NSMenuItem) -> bool {
    if target.respondsToSelector(sel!(validateMenuItem:)) {
        // SAFETY: `validateMenuItem:` (NSMenuItemValidation) takes a menu item and returns a BOOL.
        unsafe { msg_send![target, validateMenuItem: item] }
    } else if target.respondsToSelector(sel!(validateUserInterfaceItem:)) {
        // SAFETY: `validateUserInterfaceItem:` (NSUserInterfaceValidations) takes an
        // `NSValidatedUserInterfaceItem`, which a menu item is, and returns a BOOL.
        unsafe { msg_send![target, validateUserInterfaceItem: item] }
    } else {
        true
    }
}

/// A plain ⌘W (no other modifier; Caps Lock aside), by the character it types, so it follows
/// the keyboard layout as menu shortcuts do.
fn is_close_window(event: &NSEvent) -> bool {
    let held = NSEventModifierFlags::Shift | NSEventModifierFlags::Control | NSEventModifierFlags::Option | NSEventModifierFlags::Command;
    event.modifierFlags() & held == NSEventModifierFlags::Command
        && event.charactersIgnoringModifiers().is_some_and(|c| c.to_string().eq_ignore_ascii_case("w"))
}

/// The Tab key (`kVK_Tab`, the same on every layout), ⇧ or ⌃ held or not; ⌘Tab is the system's.
fn is_tab(event: &NSEvent) -> bool {
    const TAB: u16 = 48;
    event.keyCode() == TAB && !event.modifierFlags().intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Option)
}

/// A synthetic key event (`None` if AppKit makes none).
fn key_event(
    ty: NSEventType,
    flags: NSEventModifierFlags,
    time: f64,
    window: isize,
    chars: &NSString,
    plain: &NSString,
    code: u16,
) -> Option<Retained<NSEvent>> {
    NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        ty,
        NSPoint::new(0.0, 0.0),
        flags,
        time,
        window,
        None,
        chars,
        plain,
        false,
        code,
    )
}

/// SoundCraft has a text field focused (see [`set_app_wants_keyboard`]).
static APP_WANTS_KEYBOARD: AtomicBool = AtomicBool::new(false);

/// Tells plugin windows whether SoundCraft has a text field focused. While it has, the key-downs a
/// plugin does not take are not handed on to SoundCraft's window, where they would type into that
/// field out of sight; AppKit's own handling runs instead (a beep). The app calls it every frame.
pub fn set_app_wants_keyboard(wants: bool) {
    APP_WANTS_KEYBOARD.store(wants, Ordering::Relaxed);
}

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

/// The class of winit's windows: SoundCraft's own (every egui viewport is one). The name is winit's
/// (0.30.13: `src/platform_impl/macos/window.rs`), not an API: a unit test fails when Cargo.lock
/// moves winit on, and [`check_app_windows`] logs an error at start-up if no window has it.
const APP_WINDOW_CLASS: &std::ffi::CStr = c"WinitWindow";

/// One of SoundCraft's own windows.
fn is_app_window(w: &NSWindow) -> bool {
    AnyClass::get(APP_WINDOW_CLASS).is_some_and(|c| w.isKindOfClass(c))
}

/// Whether plugin windows recognise one of the app's windows as SoundCraft's own (by winit's window
/// class), logging an error naming that class when none is: plugin windows would then hand no key
/// on to SoundCraft, and AppKit would beep at each one instead (a winit upgrade that renamed its
/// class would do that silently). The app calls it once its first window exists.
pub fn check_app_windows(mtm: MainThreadMarker) -> bool {
    autoreleasepool(|_| {
        let windows = NSApplication::sharedApplication(mtm).windows();
        if windows.iter().any(|w| is_app_window(&w)) {
            return true;
        }
        let classes: Vec<String> = windows.iter().map(|w| w.class().name().to_string_lossy().into_owned()).collect();
        let class = if AnyClass::get(APP_WINDOW_CLASS).is_some() { "exists" } else { "does not exist" };
        log::error!(
            "plugin window: none of SoundCraft's windows ({classes:?}) is of class {APP_WINDOW_CLASS:?} (winit's window class; it {class}), so \
             plugin editors will pass no key on to SoundCraft. If winit was upgraded, update APP_WINDOW_CLASS in soundcraft-macos."
        );
        false
    })
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
// and `Drop` releases it there (dropped elsewhere, it hands the retain to the main queue). The
// struct merely carries the retain between threads' locks.
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
            let this = HostWindow::alloc(mtm).set_ivars(Handed::default());
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
        guarded("showing it", || order_front(window));
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
    /// Takes the window off screen, closes and releases it; dropped off the main thread, on the
    /// main thread soon after (AppKit windows are only touched there).
    fn drop(&mut self) {
        let window = Retain(self.window);
        match MainThreadMarker::new() {
            Some(mtm) => window.close(mtm),
            None => on_main_queue(Box::new(move |mtm| window.close(mtm))),
        }
    }
}

/// The retain a [`PluginWindow`] holds, on its way to being released.
struct Retain(*mut NSWindow);

// SAFETY: carried to the main thread, the only place it is used (`close` takes the marker).
unsafe impl Send for Retain {}

impl Retain {
    /// Takes the window off screen, closes and releases it. Closing tells the window's observers,
    /// a plugin's view among them.
    fn close(self, _mtm: MainThreadMarker) {
        autoreleasepool(|_| {
            // SAFETY: the retain a `PluginWindow` took in `new`, released exactly once here (`self`
            // is consumed); main thread (the marker).
            if let Some(w) = unsafe { Retained::from_raw(self.0) } {
                guarded("closing it", || {
                    w.orderOut(None);
                    w.close();
                });
            }
        });
    }
}

// libdispatch (in libSystem): work handed to the main queue runs on the main thread, from its run
// loop.
unsafe extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(queue: *const std::ffi::c_void, context: *mut std::ffi::c_void, work: unsafe extern "C" fn(*mut std::ffi::c_void));
}

/// Work for the main thread, handed over from another (see [`on_main_queue`]).
pub type MainThreadJob = Box<dyn FnOnce(MainThreadMarker) + Send>;

/// Runs `job` on the main thread, from its run loop (the main dispatch queue), soon after this
/// returns. Plugin-editor work that arrives on another thread goes this way (an instance dropped
/// there with its editor open): AppKit objects are only touched on the main thread. A panic or an
/// Objective-C exception (a plugin's view code runs as its editor goes) in `job` is stopped and
/// logged there.
pub fn on_main_queue(job: MainThreadJob) {
    let context = Box::into_raw(Box::new(job));
    // SAFETY: the main queue is a process-wide static; `dispatch_async_f` hands the context to
    // `run_job` once, which takes the box back.
    unsafe { dispatch_async_f((&raw const _dispatch_main_q).cast(), context.cast(), run_job) };
}

unsafe extern "C" fn run_job(context: *mut std::ffi::c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: the box `on_main_queue` handed over, taken back once.
    let job = unsafe { Box::from_raw(context.cast::<MainThreadJob>()) };
    // The main queue runs on the main thread.
    let Some(mtm) = MainThreadMarker::new() else {
        log::error!("plugin window: main-queue work ran off the main thread; it is dropped undone");
        std::mem::forget(job);
        return;
    };
    // Neither a panic nor an Objective-C exception may unwind into libdispatch (and
    // `catch_unwind` aborts on an exception, so the job runs inside `exception::catch`).
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| guarded("main-queue work", || job(mtm)))).is_err() {
        log::error!("plugin window: a panic in main-queue work was stopped");
    }
}

/// Runs `f`, which may run a plugin view's own code (AppKit calls into the views as it shows,
/// closes or refocuses their window), catching an Objective-C exception thrown there (logged, and
/// `None`): uncaught, it would unwind into AppKit or abort SoundCraft.
fn guarded<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    objc2::exception::catch(std::panic::AssertUnwindSafe(f))
        .map_err(|e| log::error!("plugin window: {what} threw an Objective-C exception: {}", e.map_or_else(|| "nil".to_string(), |e| e.to_string())))
        .ok()
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

#[cfg(test)]
mod tests {
    /// The winit whose macOS window class was checked to be [`super::APP_WINDOW_CLASS`]
    /// (`src/platform_impl/macos/window.rs`, `const NAME`).
    const CHECKED_WINIT: &str = "0.30.13";

    /// Plugin windows know SoundCraft's windows by winit's class name, which winit may change in
    /// any release: moving winit on in Cargo.lock fails here until the name is checked again.
    #[test]
    fn the_window_class_was_checked_against_the_winit_in_use() {
        assert_eq!(super::APP_WINDOW_CLASS, c"WinitWindow");
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")).expect("the workspace's Cargo.lock");
        let winit: Vec<&str> = lock
            .split("[[package]]")
            .filter(|p| p.lines().any(|l| l.trim() == "name = \"winit\""))
            .filter_map(|p| p.lines().find_map(|l| l.trim().strip_prefix("version = \"")?.strip_suffix('"')))
            .collect();
        assert_eq!(
            winit,
            [CHECKED_WINIT],
            "winit moved on: check that its macOS window class is still named {:?} (winit's src/platform_impl/macos/window.rs), update \
             APP_WINDOW_CLASS if it is not, then CHECKED_WINIT",
            super::APP_WINDOW_CLASS
        );
    }
}
