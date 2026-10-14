//! `open_documents::install` called after the launch began (its own process: it sets the
//! application's delegate). The launch notification has been posted by then, so waiting for it
//! would leave Finder's files unanswered: the method must be added at once. Runs on the main
//! thread (`harness = false`); shows nothing.

// The stand-in delegate is an Objective-C class defined here.
#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
    use objc2::{ClassType, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{NSApplication, NSApplicationDelegate};
    use objc2_foundation::{NSArray, NSObjectProtocol, NSString, NSURL};
    use std::path::PathBuf;

    define_class!(
        // SAFETY: an NSObject subclass without ivars or Drop, conforming to the (all optional)
        // NSApplicationDelegate protocol, like winit's delegate.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SoundCraftTestLateDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}
        unsafe impl NSApplicationDelegate for Delegate {}
    );

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("the main thread");
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: NSObject's `init` on a fresh instance.
        let delegate: Retained<Delegate> = unsafe { msg_send![super(Delegate::alloc(mtm).set_ivars(())), init] };
        app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let sel = sel!(application:openURLs:);
        assert!(!Delegate::class().responds_to(sel));

        soundcraft_macos::open_documents::install();
        assert!(Delegate::class().responds_to(sel), "added at once: the launch notification will not come again");

        let url = NSURL::URLWithString(&NSString::from_str("file:///tmp/soundcraft-macos-test/Late.scraft")).expect("a URL");
        let urls: Retained<NSArray> = NSArray::from_slice(&[url.as_ref()] as &[&AnyObject]);
        // SAFETY: the method install added, with its signature (an application, an array of URLs).
        let () = unsafe { msg_send![&*delegate, application: None::<&AnyObject>, openURLs: &*urls] };
        assert_eq!(soundcraft_macos::open_documents::take_paths(), [PathBuf::from("/tmp/soundcraft-macos-test/Late.scraft")]);
        app.setDelegate(None);
        println!("open_documents_late: ok");
    }
}
