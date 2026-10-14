//! The only module with `unsafe`: the launch observer and the `application:openURLs:` method
//! added to the application delegate's class.
//!
//! The method is a plain `extern "C-unwind"` function registered with the Objective-C runtime;
//! it reads only its arguments and the crate's queue, so it stays valid for the whole process.

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::sel;
use objc2_app_kit::{NSApplication, NSApplicationWillFinishLaunchingNotification};
use objc2_foundation::{NSArray, NSNotification, NSNotificationCenter, NSURL};
use std::ffi::CStr;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

/// The Objective-C signature of `application:openURLs:`: returns void; takes self, `_cmd`, the
/// application and the array of URLs.
const OPEN_URLS_TYPES: &CStr = c"v@:@@";

/// The Rust signature of the method, matching [`OPEN_URLS_TYPES`].
type OpenUrls = extern "C-unwind" fn(&AnyObject, Sel, Option<&AnyObject>, Option<&NSArray>);

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Why `application:openURLs:` could not be added to a class.
#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("{0} already implements application:openURLs:")]
    AlreadyHandled(String),
    #[error("the Objective-C runtime refused to add application:openURLs: to {0}")]
    Refused(String),
}

/// Registers the observer that adds the method when the launch begins (see the crate docs).
pub fn install() {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let block = RcBlock::new(|_: NonNull<NSNotification>| on_will_finish_launching());
    // SAFETY: AppKit's notification-name constant is an immutable NSString that lives for the
    // whole process.
    let name = unsafe { NSApplicationWillFinishLaunchingNotification };
    // SAFETY: no object filter; no queue, so the block runs on the posting thread (AppKit posts
    // this notification on the main thread, and the block checks that before touching AppKit);
    // the block captures nothing, so sending it is sound.
    let token = unsafe { NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) };
    // The notification is posted once per process: the observation simply stays registered.
    std::mem::forget(token);
}

fn on_will_finish_launching() {
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("open-documents: launch notification off the main thread; files opened from Finder will not arrive");
        return;
    };
    let Some(delegate) = NSApplication::sharedApplication(mtm).delegate() else {
        log::warn!("open-documents: the application has no delegate; files opened from Finder will not arrive");
        return;
    };
    let delegate: &AnyObject = delegate.as_ref();
    match add_open_urls(delegate.class()) {
        Ok(()) => log::info!("open-documents: application:openURLs: added to {}", delegate.class().name().to_string_lossy()),
        Err(e) => log::warn!("open-documents: {e}; files opened from Finder will not arrive"),
    }
}

/// Adds SoundCraft's `application:openURLs:` to `class`, unless instances already answer it.
pub fn add_open_urls(class: &AnyClass) -> Result<(), AddError> {
    let sel = sel!(application:openURLs:);
    let name = || class.name().to_string_lossy().into_owned();
    if class.responds_to(sel) {
        return Err(AddError::AlreadyHandled(name()));
    }
    let imp: OpenUrls = open_urls;
    // SAFETY: function pointers all have the same size and representation; the runtime calls this
    // one only through `application:openURLs:` with the arguments `OPEN_URLS_TYPES` declares,
    // which are the ones `OpenUrls` takes.
    let imp = unsafe { std::mem::transmute::<OpenUrls, Imp>(imp) };
    // SAFETY: `class` is a registered class (`&AnyClass` cannot be anything else); the selector,
    // implementation and type encoding agree; `class_addMethod` takes the runtime's own lock, so
    // adding a method to a class that is in use is supported.
    let added = unsafe { objc2::ffi::class_addMethod(std::ptr::from_ref(class).cast_mut(), sel, imp, OPEN_URLS_TYPES.as_ptr()) };
    if added.as_bool() { Ok(()) } else { Err(AddError::Refused(name())) }
}

/// `-[delegate application:openURLs:]`: queues the file paths among `urls`.
extern "C-unwind" fn open_urls(_delegate: &AnyObject, _cmd: Sel, _app: Option<&AnyObject>, urls: Option<&NSArray>) {
    // A panic must never unwind into AppKit.
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(urls) = urls {
            crate::QUEUE.push(file_paths(urls));
        }
    }));
    if caught.is_err() {
        log::error!("open-documents: a panic while queueing opened files was stopped");
    }
}

/// The paths of the file URLs in `urls`; other objects and other URLs are skipped.
fn file_paths(urls: &NSArray) -> Vec<PathBuf> {
    urls.iter().take(crate::MAX_PENDING).filter_map(|obj| obj.downcast_ref::<NSURL>().and_then(file_path)).collect()
}

/// The path of a file URL (a file reference URL is resolved first); `None` for other URLs.
///
/// The path is the URL's own, as text (`-[NSURL path]`), like the paths of files dropped on the
/// window; `fileSystemRepresentation` would decompose accented names (NFD).
fn file_path(url: &NSURL) -> Option<PathBuf> {
    if !url.isFileURL() {
        log::info!("open-documents: ignored a URL that is not a file");
        return None;
    }
    let path = url.filePathURL()?.path()?.to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::{AddError, add_open_urls, file_path};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, NSObject};
    use objc2::sel;
    use objc2::{ClassType, msg_send};
    use objc2_foundation::{NSArray, NSString, NSURL};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn url(s: &str) -> Retained<NSURL> {
        NSURL::URLWithString(&NSString::from_str(s)).unwrap()
    }

    #[test]
    fn file_urls_become_decoded_paths() {
        assert_eq!(file_path(&url("file:///Users/me/My%20Song.scraft")), Some(PathBuf::from("/Users/me/My Song.scraft")));
        assert_eq!(file_path(&url("file://localhost/tmp/take.wav")), Some(PathBuf::from("/tmp/take.wav")));
        assert_eq!(file_path(&url("file:///tmp/Can%C3%A7%C3%A3o%20%231.scraft")), Some(PathBuf::from("/tmp/Canção #1.scraft")));
    }

    #[test]
    fn other_urls_are_ignored() {
        for s in ["https://example.com/Song.scraft", "soundcraft://open?path=/tmp/a.scraft", "mailto:me@example.com", "ftp://host/a.wav"] {
            assert_eq!(file_path(&url(s)), None, "{s}");
        }
    }

    #[test]
    fn file_reference_urls_resolve_to_their_path() {
        let dir = std::env::temp_dir().join(format!("soundcraft-macos-open-test-ref-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Ref Song.scraft");
        std::fs::write(&file, b"{}").unwrap();
        let reference = NSURL::from_file_path(&file).unwrap().fileReferenceURL().unwrap();
        assert!(reference.isFileReferenceURL());
        let resolved = file_path(&reference).unwrap();
        assert_eq!(resolved.canonicalize().unwrap(), file.canonicalize().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Adds the method to a stand-in delegate class and calls it through the Objective-C runtime,
    /// as AppKit does. The only test that feeds the crate's global queue.
    #[test]
    fn the_added_method_queues_the_file_paths_and_wakes_the_app() {
        let class: &AnyClass = ClassBuilder::new(c"SoundCraftMacosOpenTestDelegate", NSObject::class()).unwrap().register();
        let sel = sel!(application:openURLs:);
        assert!(!class.responds_to(sel));
        add_open_urls(class).unwrap();
        assert!(class.responds_to(sel), "AppKit asks respondsToSelector: before calling the delegate");
        // Never twice, and never over a method someone else defined.
        assert!(matches!(add_open_urls(class), Err(AddError::AlreadyHandled(_))));

        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        crate::set_waker(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        let song = url("file:///tmp/soundcraft-macos-open-test/My%20Song.scraft");
        let web = url("https://example.com/x.scraft");
        let take = url("file:///tmp/soundcraft-macos-open-test/take.wav");
        let not_a_url = NSString::from_str("/tmp/not-a-url.scraft");
        let urls: Retained<NSArray> = NSArray::from_slice(&[song.as_ref(), web.as_ref(), take.as_ref(), not_a_url.as_ref()] as &[&AnyObject]);
        // SAFETY: `new` on an NSObject subclass returns a +1 instance.
        let delegate: Retained<AnyObject> = unsafe { msg_send![class, new] };
        // SAFETY: the method was just added with this signature (application, NSArray of URLs).
        let () = unsafe { msg_send![&*delegate, application: None::<&AnyObject>, openURLs: &*urls] };
        // A nil array (never sent by AppKit) is ignored.
        // SAFETY: as above, with a nil array.
        let () = unsafe { msg_send![&*delegate, application: None::<&AnyObject>, openURLs: None::<&NSArray>] };

        assert_eq!(
            crate::take_paths(),
            [PathBuf::from("/tmp/soundcraft-macos-open-test/My Song.scraft"), PathBuf::from("/tmp/soundcraft-macos-open-test/take.wav")]
        );
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert!(crate::take_paths().is_empty());
    }
}
