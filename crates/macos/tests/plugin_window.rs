//! The plugin editor window on the process main thread (`harness = false`: the standard harness
//! never runs tests there). Nothing reaches the screen: every window stays hidden, SoundCraft's
//! window is a hidden stand-in made the key target with the `testing` seam (a hidden window is
//! never main nor ordered front), and the key events are synthetic.

// Stand-in views are Objective-C classes defined here.
#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSFloatingWindowLevel, NSMenuItem, NSPasteboard,
        NSPasteboardTypeString, NSResponder, NSText, NSTextView, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSException, NSPoint, NSRange, NSRect, NSRunLoop, NSSize, NSString};
    use soundcraft_macos::plugin_window::{PluginWindow, check_app_windows, content_size, testing};
    use std::cell::{Cell, RefCell};

    /// Space, W, Tab and left Shift (virtual key codes).
    const SPACE: u16 = 49;
    const W: u16 = 13;
    const TAB: u16 = 48;
    const SHIFT: u16 = 56;
    /// The Edit keys' letters, and S (virtual key codes).
    const A: u16 = 0;
    const S: u16 = 1;
    const Z: u16 = 6;
    const X: u16 = 7;
    const C: u16 = 8;
    const V: u16 = 9;

    thread_local! {
        /// Key events seen: (who, what, key code).
        static SEEN: RefCell<Vec<(&'static str, &'static str, u16)>> = const { RefCell::new(Vec::new()) };
        /// The private pasteboard the test text view copies to and pastes from: the user's general
        /// pasteboard is never touched.
        static CLIPBOARD: RefCell<Option<Retained<NSPasteboard>>> = const { RefCell::new(None) };
    }

    fn clipboard() -> Retained<NSPasteboard> {
        CLIPBOARD.with(|c| c.borrow_mut().get_or_insert_with(NSPasteboard::pasteboardWithUniqueName).clone())
    }

    fn clipboard_text() -> Option<String> {
        // SAFETY: a constant AppKit string.
        clipboard().stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
    }

    fn record(who: &'static str, what: &'static str, e: &NSEvent) {
        SEEN.with(|s| s.borrow_mut().push((who, what, e.keyCode())));
    }

    fn seen() -> Vec<(&'static str, &'static str, u16)> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    define_class!(
        // SAFETY: an NSView subclass without Drop that overrides responder methods with their
        // declared signatures.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestKeyTarget"]
        /// SoundCraft's view (winit's, in the app): takes every key event.
        struct Target;

        impl Target {
            #[unsafe(method(acceptsFirstResponder))]
            fn accepts_first_responder(&self) -> bool {
                true
            }
            #[unsafe(method(keyDown:))]
            fn key_down(&self, e: &NSEvent) {
                record("soundcraft", "down", e);
            }
            #[unsafe(method(keyUp:))]
            fn key_up(&self, e: &NSEvent) {
                record("soundcraft", "up", e);
            }
            #[unsafe(method(flagsChanged:))]
            fn flags_changed(&self, e: &NSEvent) {
                record("soundcraft", "flags", e);
            }
        }
    );

    define_class!(
        // SAFETY: as above; the ivar is a plain `Cell`.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestPluginView"]
        #[ivars = Cell<bool>]
        /// A plugin's view: takes the keys itself when its ivar says so (a text field), else
        /// passes them up the responder chain as `NSView` does.
        struct PluginView;

        impl PluginView {
            #[unsafe(method(acceptsFirstResponder))]
            fn accepts_first_responder(&self) -> bool {
                true
            }
            #[unsafe(method(keyDown:))]
            fn key_down(&self, e: &NSEvent) {
                if self.ivars().get() {
                    record("plugin", "down", e);
                } else {
                    // SAFETY: NSView's own `keyDown:` (on up the responder chain).
                    let () = unsafe { msg_send![super(self), keyDown: e] };
                }
            }
            #[unsafe(method(keyUp:))]
            fn key_up(&self, e: &NSEvent) {
                if self.ivars().get() {
                    record("plugin", "up", e);
                } else {
                    // SAFETY: as above.
                    let () = unsafe { msg_send![super(self), keyUp: e] };
                }
            }
        }
    );

    define_class!(
        // SAFETY: an NSView subclass without Drop that overrides `keyDown:` with its declared
        // signature.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestThrowingTarget"]
        /// A key target whose own code throws an Objective-C exception.
        struct ThrowingTarget;

        impl ThrowingTarget {
            #[unsafe(method(acceptsFirstResponder))]
            fn accepts_first_responder(&self) -> bool {
                true
            }
            #[unsafe(method(keyDown:))]
            fn key_down(&self, _e: &NSEvent) {
                throws_now();
            }
        }
    );

    define_class!(
        // SAFETY: an NSView subclass without Drop that overrides responder methods with their
        // declared signatures.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestThrowingControl"]
        /// A plugin control whose own code throws as it takes the focus.
        struct ThrowingControl;

        impl ThrowingControl {
            #[unsafe(method(acceptsFirstResponder))]
            fn accepts_first_responder(&self) -> bool {
                true
            }
            #[unsafe(method(becomeFirstResponder))]
            fn become_first_responder(&self) -> bool {
                throws_now();
                true
            }
        }
    );

    define_class!(
        // SAFETY: an NSObject subclass without Drop that implements `windowShouldClose:` with its
        // declared signature.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestThrowingDelegate"]
        /// A window delegate whose own code throws when the window is about to close (a plugin
        /// may observe its window).
        struct ThrowingDelegate;

        unsafe impl NSObjectProtocol for ThrowingDelegate {}

        unsafe impl NSWindowDelegate for ThrowingDelegate {
            #[unsafe(method(windowShouldClose:))]
            fn window_should_close(&self, _sender: &NSWindow) -> bool {
                throws_now();
                true
            }
        }
    );

    define_class!(
        // SAFETY: an NSWindow subclass without instance variables, Drop or overrides.
        #[unsafe(super(NSWindow, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "WinitWindow"]
        /// SoundCraft's own window: winit's class (winit is not linked here).
        struct AppWindow;
    );

    define_class!(
        // SAFETY: an NSResponder subclass without Drop that overrides responder methods with
        // their declared signatures.
        #[unsafe(super(NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestUnhandled"]
        /// The plugin window's next responder in this test: what AppKit's own handling would get
        /// (it beeps), recorded instead.
        struct Unhandled;

        impl Unhandled {
            #[unsafe(method(keyDown:))]
            fn key_down(&self, e: &NSEvent) {
                record("unhandled", "down", e);
            }
        }
    );

    define_class!(
        // SAFETY: an NSTextView subclass without instance variables or Drop that overrides `copy:`,
        // `cut:`, `paste:` and `validateMenuItem:` with their declared signatures.
        #[unsafe(super(NSTextView, NSText, NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestTextView"]
        /// A plain Cocoa text view (a value box of Apple's generic view, a native plugin UI) whose
        /// copy, cut and paste use a private pasteboard: the stock ones use the user's.
        struct TextView;

        impl TextView {
            #[unsafe(method(copy:))]
            fn copy_action(&self, _sender: Option<&AnyObject>) {
                self.copy_selection();
            }
            #[unsafe(method(cut:))]
            fn cut_action(&self, _sender: Option<&AnyObject>) {
                self.copy_selection();
                // SAFETY: NSText's `delete:` (deletes the selection, no pasteboard), no sender.
                unsafe { self.delete(None) };
            }
            #[unsafe(method(paste:))]
            fn paste_action(&self, _sender: Option<&AnyObject>) {
                // SAFETY: a constant AppKit string.
                self.readSelectionFromPasteboard_type(&clipboard(), unsafe { NSPasteboardTypeString });
            }
            #[unsafe(method(validateMenuItem:))]
            fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
                if item.action() == Some(sel!(paste:)) {
                    clipboard_text().is_some()
                } else {
                    // SAFETY: NSTextView's own `validateMenuItem:` (copy: needs a selection).
                    unsafe { msg_send![super(self), validateMenuItem: item] }
                }
            }
        }
    );

    impl TextView {
        fn copy_selection(&self) {
            let selected = self.string().substringWithRange(self.selectedRange());
            let pasteboard = clipboard();
            pasteboard.clearContents();
            // SAFETY: a constant AppKit string.
            pasteboard.setString_forType(&selected, unsafe { NSPasteboardTypeString });
        }
    }

    define_class!(
        // SAFETY: an NSView subclass without Drop that overrides `copy:` with its declared
        // signature.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestThrowingCopy"]
        /// A plugin control whose own `copy:` throws an Objective-C exception.
        struct ThrowingCopy;

        impl ThrowingCopy {
            #[unsafe(method(acceptsFirstResponder))]
            fn accepts_first_responder(&self) -> bool {
                true
            }
            #[unsafe(method(copy:))]
            fn copy_action(&self, _sender: Option<&AnyObject>) {
                throws_now();
            }
        }
    );

    /// Throws an Objective-C exception.
    fn throws_now() {
        let name = NSString::from_str("SoundCraftTestException");
        NSException::new(&name, Some(&NSString::from_str("a plugin view threw")), None).expect("an exception").raise();
    }

    /// An order-front that throws, as a plugin view's own code may when it is drawn.
    fn throws(_: &NSWindow) {
        throws_now();
    }

    /// An order-front that leaves the window hidden: `show` marks it as shown.
    fn stays_hidden(_: &NSWindow) {}

    fn plugin_view(mtm: MainThreadMarker, takes_keys: bool) -> Retained<PluginView> {
        let this = PluginView::alloc(mtm).set_ivars(Cell::new(takes_keys));
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 240.0));
        // SAFETY: NSView's designated initializer on a fresh instance.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// A synthetic key event for `window`.
    fn key(window: &NSWindow, ty: NSEventType, code: u16, flags: NSEventModifierFlags) -> Retained<NSEvent> {
        let chars = NSString::from_str(match code {
            SPACE => " ",
            W => "w",
            TAB => "\t",
            A => "a",
            S => "s",
            Z => "z",
            X => "x",
            C => "c",
            V => "v",
            _ => "",
        });
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            ty,
            NSPoint::new(0.0, 0.0),
            flags,
            0.0,
            window.windowNumber(),
            None,
            &chars,
            &chars,
            false,
            code,
        )
        .expect("a key event")
    }

    /// Delivers a key event to `window` as AppKit does for its key window: to its first responder.
    /// In a pool, as the run loop has one: the event retains the window until it is drained.
    fn type_key(window: &NSWindow, ty: NSEventType, code: u16, flags: NSEventModifierFlags) {
        autoreleasepool(|_| window.sendEvent(&key(window, ty, code, flags)));
    }

    /// Delivers a key-down as AppKit does to its key window: offered as a key equivalent first (to
    /// the window), it arrives as `keyDown:` only if nothing took it. In a pool, as in `type_key`.
    fn press(window: &NSWindow, code: u16, flags: NSEventModifierFlags) {
        autoreleasepool(|_| {
            let e = key(window, NSEventType::KeyDown, code, flags);
            if !window.performKeyEquivalent(&e) {
                window.sendEvent(&e);
            }
        });
    }

    /// A text view in `window`'s content view, holding `text`, focused.
    fn focused_text(mtm: MainThreadMarker, window: &PluginWindow, text: &str) -> Retained<TextView> {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 40.0));
        // SAFETY: NSTextView's `initWithFrame:` on a fresh instance of the subclass.
        let view: Retained<TextView> = unsafe { msg_send![super(TextView::alloc(mtm).set_ivars(())), initWithFrame: frame] };
        view.setString(&NSString::from_str(text));
        view.setAllowsUndo(true);
        window.content_view(mtm).expect("a content view").addSubview(&view);
        assert!(window.window(mtm).makeFirstResponder(Some(&view)));
        view
    }

    /// A hidden, titled window: SoundCraft's (winit's class) when `app`, else a plain `NSWindow`.
    fn hidden_window(mtm: MainThreadMarker, app: bool) -> Retained<NSWindow> {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 100.0));
        let (style, backing) = (NSWindowStyleMask::Titled, NSBackingStoreType::Buffered);
        let w = if app {
            // SAFETY: NSWindow's designated initializer on a fresh instance of the subclass, main
            // thread, valid arguments.
            let w: Retained<AppWindow> = unsafe {
                msg_send![super(AppWindow::alloc(mtm).set_ivars(())), initWithContentRect: rect, styleMask: style, backing: backing, defer: false]
            };
            Retained::into_super(w)
        } else {
            // SAFETY: as above, on a fresh NSWindow.
            unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, backing, false) }
        };
        // SAFETY: the caller holds its own retain of the window.
        unsafe { w.setReleasedWhenClosed(false) };
        w
    }

    /// The titles of the process's windows.
    fn titles(mtm: MainThreadMarker) -> Vec<String> {
        autoreleasepool(|_| NSApplication::sharedApplication(mtm).windows().iter().map(|w| w.title().to_string()).collect())
    }

    /// The process's windows: (all, visible).
    fn windows(mtm: MainThreadMarker) -> (usize, usize) {
        autoreleasepool(|_| {
            let all = NSApplication::sharedApplication(mtm).windows();
            let visible = (0..all.count()).filter(|&i| all.objectAtIndex(i).isVisible()).count();
            (all.count(), visible)
        })
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("the main thread");
        let _app = NSApplication::sharedApplication(mtm);
        let (base, visible) = windows(mtm);
        assert_eq!(visible, 0);
        // No window of SoundCraft's yet (a winit that renamed its window class looks the same): the
        // start-up check reports it (an error naming the class is logged).
        assert!(!check_app_windows(mtm), "no window of winit's class");

        // SoundCraft's window, hidden, with its view as first responder.
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 100.0));
        let app_window = hidden_window(mtm, true);
        // SAFETY: NSView's designated initializer on a fresh instance.
        let target: Retained<Target> = unsafe { msg_send![super(Target::alloc(mtm).set_ivars(())), initWithFrame: rect] };
        app_window.setContentView(Some(&target));
        assert!(app_window.makeFirstResponder(Some(&target)));
        testing::set_key_target(Some(&app_window));
        assert!(check_app_windows(mtm), "SoundCraft's window (winit's class) is recognised");

        // Each step in its own pool, as each run-loop turn has one: AppKit keeps windows in
        // autoreleased references until the pool drains.
        let mut w = autoreleasepool(|_| PluginWindow::new(mtm, "Test Plugin", NSSize::new(320.0, 240.0)));
        autoreleasepool(|_| {
            let win = w.window(mtm);
            assert_eq!(win.styleMask(), NSWindowStyleMask::Titled | NSWindowStyleMask::Closable, "titled and closable, not minimizable");
            assert_eq!(win.level(), NSFloatingWindowLevel, "floats above the app");
            assert!(win.hidesOnDeactivate(), "hides with the app");
            assert!(win.isExcludedFromWindowsMenu());
            assert!(win.collectionBehavior().contains(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle));
            assert!(!win.canBecomeMainWindow(), "SoundCraft's window stays main");
            assert!(win.canBecomeKeyWindow());
            assert_eq!(win.title().to_string(), "Test Plugin");
            let content = win.contentRectForFrameRect(win.frame()).size;
            assert_eq!((content.width, content.height), (320.0, 240.0));
            assert!(!win.isVisible());
            assert!(!w.closed_by_user(mtm), "never shown: not closed");
        });
        assert_eq!(windows(mtm), (base + 2, 0));

        let none = NSEventModifierFlags::empty();
        // SAFETY: NSResponder's designated initializer on a fresh instance.
        let unhandled: Retained<Unhandled> = unsafe { msg_send![super(Unhandled::alloc(mtm).set_ivars(())), init] };
        autoreleasepool(|_| {
            let win = w.window(mtm);
            // SAFETY: the window does not retain its next responder; `unhandled` lives until the
            // end of the test, after the window is gone.
            unsafe { win.setNextResponder(Some(&unhandled)) };
            let content = w.content_view(mtm).expect("a content view");

            // A view that passes keys up: Space, its release and Shift reach SoundCraft's view.
            let passing = plugin_view(mtm, false);
            content.addSubview(&passing);
            assert!(win.makeFirstResponder(Some(&passing)));
            type_key(win, NSEventType::KeyDown, SPACE, none);
            type_key(win, NSEventType::KeyUp, SPACE, none);
            type_key(win, NSEventType::FlagsChanged, SHIFT, NSEventModifierFlags::Shift);
            assert_eq!(seen(), [("soundcraft", "down", SPACE), ("soundcraft", "up", SPACE), ("soundcraft", "flags", SHIFT)]);

            // While SoundCraft has a text field focused, a key-down is not handed on (it would type
            // into that field out of sight): AppKit's own handling runs.
            soundcraft_macos::plugin_window::set_app_wants_keyboard(true);
            type_key(win, NSEventType::KeyDown, SPACE, none);
            assert_eq!(seen(), [("unhandled", "down", SPACE)]);
            soundcraft_macos::plugin_window::set_app_wants_keyboard(false);
            type_key(win, NSEventType::KeyDown, SPACE, none);
            assert_eq!(seen(), [("soundcraft", "down", SPACE)]);

            // A ⌘-key nobody took arrives as keyDown: and is handed on with its modifiers.
            type_key(win, NSEventType::KeyDown, 1, NSEventModifierFlags::Command);
            assert_eq!(seen(), [("soundcraft", "down", 1)]);

            // A view that takes keys (a text field) keeps them.
            passing.removeFromSuperview();
            let taking = plugin_view(mtm, true);
            content.addSubview(&taking);
            assert!(win.makeFirstResponder(Some(&taking)));
            type_key(win, NSEventType::KeyDown, SPACE, none);
            type_key(win, NSEventType::KeyUp, SPACE, none);
            assert_eq!(seen(), [("plugin", "down", SPACE), ("plugin", "up", SPACE)]);

            // Tab that a focused plugin control passes up moves the focus to the plugin's next
            // control (AppKit's key-view loop), ⇧Tab back; it is not handed on.
            taking.removeFromSuperview();
            content.addSubview(&passing);
            let next = plugin_view(mtm, false);
            content.addSubview(&next);
            // SAFETY: plain key-view links between two views of the same window, both alive here.
            unsafe {
                passing.setNextKeyView(Some(&next));
                next.setNextKeyView(Some(&passing));
            }
            assert!(win.makeFirstResponder(Some(&passing)));
            type_key(win, NSEventType::KeyDown, TAB, none);
            assert_eq!(seen(), []);
            assert!(
                win.firstResponder().is_some_and(|r| std::ptr::addr_eq(Retained::as_ptr(&r), Retained::as_ptr(&next))),
                "the next control has the focus"
            );
            type_key(win, NSEventType::KeyDown, TAB, NSEventModifierFlags::Shift);
            assert_eq!(seen(), []);
            assert!(win.firstResponder().is_some_and(|r| std::ptr::addr_eq(Retained::as_ptr(&r), Retained::as_ptr(&passing))), "⇧Tab: back");
            // A control whose own code throws as it takes the focus: caught, not left to abort.
            // SAFETY: NSView's designated initializer on a fresh instance.
            let thrower: Retained<ThrowingControl> = unsafe { msg_send![super(ThrowingControl::alloc(mtm).set_ivars(())), initWithFrame: rect] };
            content.addSubview(&thrower);
            // SAFETY: as above.
            unsafe { passing.setNextKeyView(Some(&thrower)) };
            type_key(win, NSEventType::KeyDown, TAB, none);
            assert_eq!(seen(), []);
            thrower.removeFromSuperview();
            passing.removeFromSuperview();
            next.removeFromSuperview();

            // A focused view outside any key-view loop (a plugin's root view usually is: VSTGUI's,
            // JUCE's) has nowhere to move the focus to: Tab and ⇧Tab are handed on (SoundCraft's
            // own Tab).
            let lone = plugin_view(mtm, false);
            content.addSubview(&lone);
            assert!(win.makeFirstResponder(Some(&lone)));
            type_key(win, NSEventType::KeyDown, TAB, none);
            type_key(win, NSEventType::KeyUp, TAB, none);
            type_key(win, NSEventType::KeyDown, TAB, NSEventModifierFlags::Shift);
            type_key(win, NSEventType::KeyUp, TAB, NSEventModifierFlags::Shift);
            assert_eq!(
                seen(),
                [("soundcraft", "down", TAB), ("soundcraft", "up", TAB), ("soundcraft", "down", TAB), ("soundcraft", "up", TAB)],
                "no key-view loop: handed on"
            );
            assert!(win.firstResponder().is_some_and(|r| std::ptr::addr_eq(Retained::as_ptr(&r), Retained::as_ptr(&lone))), "the focus stays");
            lone.removeFromSuperview();

            // No view has the focus: the window itself is first responder, and hands keys on (Tab
            // too: SoundCraft's own Tab).
            assert!(win.makeFirstResponder(None));
            type_key(win, NSEventType::KeyDown, TAB, none);
            type_key(win, NSEventType::KeyUp, TAB, none);
            assert_eq!(seen(), [("soundcraft", "down", TAB), ("soundcraft", "up", TAB)]);
            assert!(win.makeFirstResponder(None));
            type_key(win, NSEventType::KeyDown, SPACE, none);
            assert_eq!(seen(), [("soundcraft", "down", SPACE)]);

            // Once the window is no longer key, the key-ups and modifier changes of keys still held
            // go elsewhere, and SoundCraft would see those keys down: the window lets go of them,
            // a key-up for each, then the modifier state as it is now (key code 0: no key).
            win.resignKeyWindow();
            assert_eq!(
                seen(),
                [("soundcraft", "up", 1), ("soundcraft", "up", SPACE), ("soundcraft", "flags", 0)],
                "⌘A and Space were handed on and never released, ⇧ was handed on"
            );
            win.resignKeyWindow();
            assert_eq!(seen(), [], "let go of once");
            type_key(win, NSEventType::KeyDown, SPACE, none);
            type_key(win, NSEventType::KeyUp, SPACE, none);
            win.resignKeyWindow();
            assert_eq!(seen(), [("soundcraft", "down", SPACE), ("soundcraft", "up", SPACE)], "released: nothing to let go of");

            // No SoundCraft window to hand keys to (none is main or on screen here): AppKit's
            // own handling.
            testing::set_key_target(None);
            type_key(win, NSEventType::KeyDown, SPACE, none);
            assert_eq!(seen(), [("unhandled", "down", SPACE)]);

            // A target that sends keys back (here the plugin window itself) is not followed in
            // a loop: the key is handed on once, then AppKit's own handling runs.
            content.addSubview(&passing);
            assert!(win.makeFirstResponder(Some(&passing)));
            testing::set_key_target(Some(win));
            type_key(win, NSEventType::KeyDown, SPACE, none);
            assert_eq!(seen(), [("unhandled", "down", SPACE)]);
            passing.removeFromSuperview();

            // A target whose own code throws: the exception is caught and the key counts as
            // handed on (no beep), never left to abort the app.
            let throwing_window = hidden_window(mtm, true);
            // SAFETY: NSView's designated initializer on a fresh instance.
            let thrower: Retained<ThrowingTarget> = unsafe { msg_send![super(ThrowingTarget::alloc(mtm).set_ivars(())), initWithFrame: rect] };
            throwing_window.setContentView(Some(&thrower));
            assert!(throwing_window.makeFirstResponder(Some(&thrower)));
            testing::set_key_target(Some(&throwing_window));
            assert!(win.makeFirstResponder(None));
            type_key(win, NSEventType::KeyDown, SPACE, none);
            assert_eq!(seen(), []);
            throwing_window.close();
            testing::set_key_target(Some(&app_window));
        });

        // Who gets the keys a plugin window passes on: SoundCraft's main window, else its
        // frontmost visible window that can be key; never a plugin's window, neither a plugin
        // window nor one a plugin opened itself (here a plain NSWindow: a CLAP plugin's floating
        // window), even when that one is main or in front.
        autoreleasepool(|_| {
            let other_app_window = hidden_window(mtm, true);
            let plugins_own = hidden_window(mtm, false);
            let plugin = w.window(mtm);
            let all = |_: &NSWindow| true;
            let pick = |main: Option<&NSWindow>, ordered: &[&NSWindow], visible: &dyn Fn(&NSWindow) -> bool| {
                testing::choose_target(main, ordered, visible).map(|t| Retained::as_ptr(&t))
            };
            let ptr = |w: &NSWindow| Some(std::ptr::from_ref(w));
            assert_eq!(pick(Some(&app_window), &[plugin, &plugins_own, &other_app_window, &app_window], &all), ptr(&app_window), "the main window");
            assert_eq!(
                pick(Some(&plugins_own), &[&plugins_own, plugin, &app_window], &all),
                ptr(&app_window),
                "a plugin's own main window is passed over"
            );
            assert_eq!(
                pick(None, &[plugin, &plugins_own, &other_app_window, &app_window], &all),
                ptr(&other_app_window),
                "the frontmost of SoundCraft's"
            );
            assert_eq!(pick(None, &[plugin, &plugins_own], &all), None, "only plugins' windows: none");
            let not_other = |w: &NSWindow| !std::ptr::eq(w, &*other_app_window);
            assert_eq!(pick(None, &[&other_app_window, &app_window], &not_other), ptr(&app_window), "a hidden one is passed over");
            assert_eq!(pick(None, &[&app_window], &|_: &NSWindow| false), None, "none visible: none");
            other_app_window.close();
            plugins_own.close();
        });

        // The window follows the view's size, keeping its top-left corner.
        autoreleasepool(|_| {
            let before = w.window(mtm).frame();
            w.fit(mtm, NSSize::new(500.0, 410.0));
            let after = w.window(mtm).frame();
            let content = w.window(mtm).contentRectForFrameRect(after).size;
            assert_eq!((content.width, content.height), (500.0, 410.0));
            assert_eq!(after.origin.x, before.origin.x);
            assert_eq!(after.origin.y + after.size.height, before.origin.y + before.size.height, "the top edge stays");
            w.fit(mtm, NSSize::new(f64::NAN, 10.0));
            assert_eq!(w.window(mtm).frame(), after, "no size, no change");
        });

        // The content size rule, shared with the Audio Unit host: rounded, at most 16 384 points a
        // side, and no size for what is not one.
        assert_eq!(content_size(NSSize::new(320.4, 239.6)), Some(NSSize::new(320.0, 240.0)));
        assert_eq!(content_size(NSSize::new(50_000.0, 10.0)), Some(NSSize::new(16_384.0, 10.0)));
        for none in [NSSize::new(f64::NAN, 10.0), NSSize::new(0.5, 10.0), NSSize::new(10.0, f64::INFINITY), NSSize::new(0.0, 0.0)] {
            assert_eq!(content_size(none), None, "{none:?}");
        }

        // Showing the window runs its plugin view's own code (here an order-front that throws, as
        // a view's drawing may): the exception is caught, not left to abort the app.
        testing::set_order_front(Some(throws));
        autoreleasepool(|_| w.show(mtm));
        testing::set_order_front(None);
        assert_eq!(windows(mtm), (base + 2, 0), "nothing on screen");

        // Shown, then hidden: closed by the user while SoundCraft is the active app, still open
        // while SoundCraft is in the background (the window hides with it).
        testing::set_order_front(Some(stays_hidden));
        autoreleasepool(|_| w.show(mtm));
        testing::set_order_front(None);
        testing::set_app_active(Some(false));
        assert!(!w.closed_by_user(mtm), "hidden with the app: open");
        testing::set_app_active(Some(true));
        assert!(w.closed_by_user(mtm), "hidden while the app is active: closed");
        testing::set_app_active(None);

        // ⌘W that the plugin does not take closes its editor window; handed on, it would close
        // SoundCraft's window. ⌘W with another modifier is handed on like any ⌘-key, and a
        // plugin view that takes ⌘W keeps it.
        autoreleasepool(|_| {
            let other = PluginWindow::new(mtm, "Other Plugin", NSSize::new(100.0, 100.0));
            let win = other.window(mtm);
            assert!(win.makeFirstResponder(None));
            type_key(win, NSEventType::KeyDown, W, NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
            type_key(win, NSEventType::KeyDown, W, NSEventModifierFlags::Command | NSEventModifierFlags::Option);
            assert_eq!(seen(), [("soundcraft", "down", W), ("soundcraft", "down", W)]);
            let taking = plugin_view(mtm, true);
            other.content_view(mtm).expect("a content view").addSubview(&taking);
            assert!(win.makeFirstResponder(Some(&taking)));
            type_key(win, NSEventType::KeyDown, W, NSEventModifierFlags::Command);
            assert_eq!(seen(), [("plugin", "down", W)]);
            // Code that runs as the window closes throws: caught, never left to abort the app.
            assert!(win.makeFirstResponder(None));
            // SAFETY: NSObject's designated initializer on a fresh instance.
            let delegate: Retained<ThrowingDelegate> = unsafe { msg_send![super(ThrowingDelegate::alloc(mtm).set_ivars(())), init] };
            win.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            type_key(win, NSEventType::KeyDown, W, NSEventModifierFlags::Command);
            assert_eq!(seen(), [], "not handed on");
            win.setDelegate(None);

            // Closed (the window's `close` lets go of the ⌘⌥W still held), not handed on.
            type_key(win, NSEventType::KeyDown, W, NSEventModifierFlags::Command | NSEventModifierFlags::CapsLock);
            assert_eq!(seen(), [("soundcraft", "up", W), ("soundcraft", "flags", 0)], "⌘W closes the window");
        });

        // Closing lets go of the keys too.
        autoreleasepool(|_| {
            assert!(w.window(mtm).makeFirstResponder(None));
            type_key(w.window(mtm), NSEventType::KeyDown, SPACE, none);
        });
        assert_eq!(seen(), [("soundcraft", "down", SPACE)]);
        w.close(mtm);
        assert_eq!(seen(), [("soundcraft", "up", SPACE), ("soundcraft", "flags", 0)], "closed with Space held");
        assert_eq!(windows(mtm), (base + 1, 0), "the plugin window is gone");

        // Dropped on another thread it is not touched there: the main queue takes it off screen
        // and releases it on the main thread, at the run loop's next turn.
        let elsewhere = autoreleasepool(|_| PluginWindow::new(mtm, "Dropped elsewhere", NSSize::new(10.0, 10.0)));
        std::thread::spawn(move || drop(elsewhere)).join().expect("drop elsewhere");
        assert_eq!(windows(mtm), (base + 2, 0), "not touched off the main thread");
        autoreleasepool(|_| NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05)));
        assert_eq!(windows(mtm), (base + 1, 0), "gone on the main thread");

        // Main-queue work whose code throws (a plugin view's, as its editor goes) or panics is
        // stopped there, not left to abort the app; the queue goes on.
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let after = std::sync::Arc::clone(&ran);
        soundcraft_macos::plugin_window::on_main_queue(Box::new(|_| throws_now()));
        soundcraft_macos::plugin_window::on_main_queue(Box::new(|_| panic!("a panic in main-queue work")));
        soundcraft_macos::plugin_window::on_main_queue(Box::new(move |_| after.store(true, std::sync::atomic::Ordering::Relaxed)));
        // The expected panic is not printed.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        autoreleasepool(|_| NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05)));
        std::panic::set_hook(hook);
        assert!(ran.load(std::sync::atomic::Ordering::Relaxed), "later work still runs");

        // The Edit keys go to the plugin's focused control as another app's Edit menu sends them
        // (SoundCraft's menu bar has none): a plain Cocoa text view copies, pastes, cuts, undoes,
        // redoes and selects all. Its copy and paste use a private pasteboard here. It runs last: a
        // text view with an insertion point makes AppKit add a hidden window of its own
        // (TextInputUI's), which the window counts above would see.
        let general = NSPasteboard::generalPasteboard().changeCount();
        autoreleasepool(|_| {
            let cmd = NSEventModifierFlags::Command;
            let edit = PluginWindow::new(mtm, "Edit Keys", NSSize::new(200.0, 100.0));
            let win = edit.window(mtm);

            // No control focused (the window itself is first responder): they are handed on to
            // SoundCraft as before (⌘Z is its undo), as is any other ⌘-key.
            assert!(win.makeFirstResponder(None));
            for code in [C, V, X, Z, A, S] {
                press(win, code, cmd);
            }
            press(win, Z, cmd | NSEventModifierFlags::Shift);
            let downs = |codes: &[u16]| codes.iter().map(|&c| ("soundcraft", "down", c)).collect::<Vec<_>>();
            assert_eq!(seen(), downs(&[C, V, X, Z, A, S, Z]), "no control focused: handed on");

            // A focused control that implements none of the actions, with nothing to undo in the
            // window: handed on too.
            let passing = plugin_view(mtm, false);
            edit.content_view(mtm).expect("a content view").addSubview(&passing);
            assert!(win.makeFirstResponder(Some(&passing)));
            press(win, C, cmd);
            press(win, Z, cmd);
            assert_eq!(seen(), downs(&[C, Z]), "nothing takes them: handed on");
            passing.removeFromSuperview();

            let text = focused_text(mtm, &edit, "hello");
            press(win, A, cmd);
            assert_eq!(text.selectedRange(), NSRange::new(0, 5), "⌘A selects all");
            press(win, C, cmd);
            assert_eq!(clipboard_text().as_deref(), Some("hello"), "⌘C copies");
            text.setSelectedRange(NSRange::new(5, 0));
            press(win, V, cmd);
            assert_eq!(text.string().to_string(), "hellohello", "⌘V pastes");
            press(win, Z, cmd);
            assert_eq!(text.string().to_string(), "hello", "⌘Z undoes the paste (the window's undo)");
            press(win, Z, cmd | NSEventModifierFlags::Shift);
            assert_eq!(text.string().to_string(), "hellohello", "⇧⌘Z redoes it");
            text.setSelectedRange(NSRange::new(0, 5));
            press(win, X, cmd);
            assert_eq!(text.string().to_string(), "hello", "⌘X cuts");
            assert_eq!(clipboard_text().as_deref(), Some("hello"));
            assert_eq!(seen(), [], "none of them reached SoundCraft");
            text.removeFromSuperview();

            // A control whose own `copy:` throws: caught, and the key counts as taken (it is not
            // handed on), never left to abort the app.
            // SAFETY: NSView's designated initializer on a fresh instance.
            let thrower: Retained<ThrowingCopy> = unsafe { msg_send![super(ThrowingCopy::alloc(mtm).set_ivars(())), initWithFrame: rect] };
            edit.content_view(mtm).expect("a content view").addSubview(&thrower);
            assert!(win.makeFirstResponder(Some(&thrower)));
            press(win, C, cmd);
            assert_eq!(seen(), [], "taken");
            thrower.removeFromSuperview();

            // Closing lets go of the keys handed on above (their key-ups); not under test here.
            drop(edit);
            let _ = seen();
        });
        assert!(!titles(mtm).iter().any(|t| t == "Edit Keys"), "the plugin window is gone");
        assert_eq!(NSPasteboard::generalPasteboard().changeCount(), general, "the user's pasteboard is untouched");
        if let Some(private) = CLIPBOARD.with(|c| c.borrow_mut().take()) {
            // SAFETY: `releaseGlobally` takes no arguments; the pasteboard is not used after it.
            let () = unsafe { msg_send![&*private, releaseGlobally] };
        }

        testing::set_key_target(None);
        app_window.close();
        println!("plugin_window: ok");
    }
}
