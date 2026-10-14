//! The observer of `NSApplicationDidBecomeActiveNotification` behind `crate::activation`.

use block2::RcBlock;
use objc2_app_kit::NSApplicationDidBecomeActiveNotification;
use objc2_foundation::{NSNotification, NSNotificationCenter};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Registers the observer, once.
pub fn install() {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    // Only stores the moment: nothing in it can panic, and no panic may unwind into AppKit.
    let block = RcBlock::new(|_: NonNull<NSNotification>| crate::activation::activated(Instant::now()));
    // SAFETY: AppKit's notification-name constant is an immutable NSString that lives for the
    // whole process.
    let name = unsafe { NSApplicationDidBecomeActiveNotification };
    // SAFETY: no object filter; no queue, so the block runs on the posting thread (AppKit posts
    // this notification on the main thread); the block captures nothing and only stores an
    // `Instant` behind a mutex, so running it on any thread is sound.
    let token = unsafe { NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) };
    // The observation lasts as long as the process: the token is never needed to remove it.
    std::mem::forget(token);
}

#[cfg(test)]
mod tests {
    use objc2_app_kit::NSApplicationDidBecomeActiveNotification;
    use objc2_foundation::NSNotificationCenter;
    use std::time::Instant;

    /// Posts the notification as AppKit does when SoundCraft comes forward. The only test that
    /// touches the crate's activation record.
    #[test]
    fn becoming_the_active_application_is_recorded() {
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: as in `install`.
        let name = unsafe { NSApplicationDidBecomeActiveNotification };
        // SAFETY: AppKit posts this notification with the application as its object; the
        // observer ignores the object, so none is fine.
        unsafe { center.postNotificationName_object(name, None) };
        assert_eq!(crate::activation::last_activated(), None, "not recorded before install");

        crate::activation::install();
        crate::activation::install();
        let before = Instant::now();
        // SAFETY: as above.
        unsafe { center.postNotificationName_object(name, None) };
        let first = crate::activation::last_activated().unwrap();
        assert!(first >= before && first <= Instant::now());
        // SAFETY: as above.
        unsafe { center.postNotificationName_object(name, None) };
        assert!(crate::activation::last_activated().unwrap() >= first, "the latest activation, once observed");
    }
}
