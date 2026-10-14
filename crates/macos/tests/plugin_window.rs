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
    use objc2::runtime::NSObject;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSFloatingWindowLevel, NSResponder, NSView, NSWindow,
        NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{NSException, NSPoint, NSRect, NSSize, NSString};
    use soundcraft_macos::plugin_window::{PluginWindow, content_size, testing};
    use std::cell::{Cell, RefCell};

    /// Space and left Shift (virtual key codes).
    const SPACE: u16 = 49;
    const SHIFT: u16 = 56;

    thread_local! {
        /// Key events seen: (who, what, key code).
        static SEEN: RefCell<Vec<(&'static str, &'static str, u16)>> = const { RefCell::new(Vec::new()) };
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
        let chars = NSString::from_str(if code == SPACE { " " } else { "" });
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

        // SoundCraft's window, hidden, with its view as first responder.
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 100.0));
        let app_window = hidden_window(mtm, true);
        // SAFETY: NSView's designated initializer on a fresh instance.
        let target: Retained<Target> = unsafe { msg_send![super(Target::alloc(mtm).set_ivars(())), initWithFrame: rect] };
        app_window.setContentView(Some(&target));
        assert!(app_window.makeFirstResponder(Some(&target)));
        testing::set_key_target(Some(&app_window));

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

            // No view has the focus: the window itself is first responder, and hands keys on.
            taking.removeFromSuperview();
            assert!(win.makeFirstResponder(None));
            type_key(win, NSEventType::KeyDown, SPACE, none);
            assert_eq!(seen(), [("soundcraft", "down", SPACE)]);

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

        w.close(mtm);
        assert_eq!(windows(mtm), (base + 1, 0), "the plugin window is gone");

        // Dropped on another thread it is leaked, never touched there.
        let leaked = autoreleasepool(|_| PluginWindow::new(mtm, "Leaked", NSSize::new(10.0, 10.0)));
        std::thread::spawn(move || drop(leaked)).join().expect("drop elsewhere");
        assert_eq!(windows(mtm), (base + 2, 0), "leaked");

        testing::set_key_target(None);
        app_window.close();
        println!("plugin_window: ok");
    }
}
