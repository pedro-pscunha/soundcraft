//! The open-documents hook: the files macOS opens with SoundCraft.
//!
//! Double-clicking a `.scraft` in Finder, dropping a file on the Dock icon and
//! `open -a SoundCraft file` do not put the file on the command line: Launch Services sends the
//! app an "open documents" Apple Event. AppKit's own handler for that event passes the files to
//! the application delegate's `application:openURLs:`; a delegate without it makes AppKit fall
//! back to `NSDocumentController`, which knows no SoundCraft document class and shows "SoundCraft
//! cannot open files in the 'SoundCraft Session' format". [`install`] gives the delegate that
//! method; the files it receives wait in a queue that the app drains with [`take_paths`] each
//! frame, and [`set_waker`] wakes an idle app when files arrive.
//!
//! # Mechanism
//!
//! The delegate belongs to winit (eframe's windowing layer): its `WinitApplicationDelegate` class
//! implements only the launch and terminate callbacks, and winit expects to stay the delegate
//! (it looks itself up there for every event), so SoundCraft cannot replace it. Instead the
//! method is added to the delegate's class at run time with `class_addMethod`. AppKit's own
//! open-documents handler checks which of these methods the delegate responds to each time an
//! event arrives, so a method added after winit set the delegate is found, and AppKit's handler
//! stays in charge of answering the Apple Event and of the launch sequence. Implementing the
//! delegate method is what Apple's "How Cocoa Applications Handle Apple Events" calls the
//! standard mechanism. A class that already answers `application:openURLs:` (a future winit) is
//! left alone, and the failure is logged.
//!
//! Replacing AppKit's `kAEOpenDocuments` handler through `NSAppleEventManager` (installed from
//! `applicationWillFinishLaunching:`, as Apple's guide advises) would work too, but SoundCraft
//! would then have to parse and answer the Apple Event itself.
//!
//! # Timing
//!
//! When a document starts the app, AppKit sends `applicationWillFinishLaunching:`, then the
//! documents, then `applicationDidFinishLaunching:` (Apple's `application(_:openFile:)` reference
//! states this order). winit creates the delegate in `EventLoop::new` and starts the launch in
//! `NSApplication::run`, both inside `eframe::run_native`, and eframe builds the app only after
//! the launch has finished. So [`install`] must run **before** `eframe::run_native`: it registers
//! an `NSApplicationWillFinishLaunchingNotification` observer, which fires once the delegate is
//! set and before the launch document is delivered, and adds the method from there. The launch
//! document then waits in the queue until the app's first frame. Documents opened while running
//! arrive between frames on the main thread and call the waker.
//!
//! # Safety
//!
//! The Objective-C side lives in the crate's `ffi` module (see the crate docs). The API here is
//! safe and never panics; the added method catches any panic before it can unwind into AppKit,
//! ignores URLs that are not file URLs and objects that are not URLs, and keeps at most
//! [`MAX_PENDING`] paths. Off macOS [`install`] does nothing and the queue stays empty.
//!
//! # End-to-end check (needs the bundle)
//!
//! Launch Services sends these events only to an app bundle whose `Info.plist` declares the
//! document types (`packaging/macos/Info.plist.in`), so the check runs on the installed app
//! (`packaging/macos/install-local.sh`):
//!
//! 1. Quit SoundCraft. In Finder, double-click a saved `.scraft` (or run
//!    `open -a SoundCraft path/to/Song.scraft`): SoundCraft starts with that session open and no
//!    "cannot open files" alert.
//! 2. With SoundCraft running, change the session, then `open -a SoundCraft other.scraft`: the
//!    save prompt appears, and after it the other session opens.
//! 3. With SoundCraft running, drag a `.wav` onto its Dock icon (or
//!    `open -a SoundCraft take.wav`): the file is imported at the edit selection, as when it is
//!    dropped on the window.
//! 4. `~/Library/Application Support/SoundCraft/logs/soundcraft.log` has, once per start,
//!    "open-documents: application:openURLs: added to WinitApplicationDelegate", and no other
//!    "open-documents" record.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// Most opened paths kept until the app takes them; later ones are dropped (and logged).
pub const MAX_PENDING: usize = 4096;

/// Makes the files the system opens with SoundCraft reach [`take_paths`].
///
/// Call it before the event loop starts (before `eframe::run_native`), so the document that
/// launches the app is caught too; see the crate docs. Later calls do nothing.
/// Failures (no application delegate, a delegate that already handles the event) are logged:
/// the app still runs, without open-documents support. Off macOS it does nothing.
pub fn install() {
    #[cfg(target_os = "macos")]
    crate::ffi::open_documents::install();
}

/// The paths opened since the last call, oldest first. Cheap when there are none (one
/// uncontended lock), so the app calls it every frame.
pub fn take_paths() -> Vec<PathBuf> {
    QUEUE.take()
}

/// Called (on the main thread) each time new paths are queued: the app asks for a repaint here
/// so an idle window picks them up. Replaces the previous waker.
pub fn set_waker(waker: impl Fn() + Send + Sync + 'static) {
    QUEUE.set_waker(Arc::new(waker));
}

pub(crate) static QUEUE: Queue = Queue::new();

type Waker = Arc<dyn Fn() + Send + Sync>;

/// Opened paths waiting for the app, and how to wake it.
pub(crate) struct Queue {
    paths: Mutex<Vec<PathBuf>>,
    waker: Mutex<Option<Waker>>,
}

impl Queue {
    const fn new() -> Self {
        Self { paths: Mutex::new(Vec::new()), waker: Mutex::new(None) }
    }

    /// Queues paths (up to [`MAX_PENDING`] waiting) and wakes the app when any were queued.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn push(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let (mut queued, mut dropped) = (0usize, 0usize);
        {
            let mut pending = self.paths.lock().unwrap_or_else(PoisonError::into_inner);
            for path in paths {
                if pending.len() < MAX_PENDING {
                    pending.push(path);
                    queued += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        if dropped > 0 {
            log::warn!("open-documents: {dropped} paths dropped, more than {MAX_PENDING} waiting");
        }
        if queued > 0 {
            // Cloned out of the lock: the waker may take its own locks.
            let waker = self.waker.lock().unwrap_or_else(PoisonError::into_inner).clone();
            if let Some(waker) = waker {
                waker();
            }
        }
    }

    fn take(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.paths.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(waker);
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_PENDING, Queue};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn paths_come_out_once_in_the_order_they_arrived() {
        let q = Queue::new();
        assert!(q.take().is_empty());
        q.push(paths(&["/a/Song.scraft", "/b/take.wav"]));
        q.push(paths(&["/c/riff.mid"]));
        assert_eq!(q.take(), paths(&["/a/Song.scraft", "/b/take.wav", "/c/riff.mid"]));
        assert!(q.take().is_empty());
    }

    #[test]
    fn the_waker_runs_once_per_batch_that_queued_something() {
        let q = Queue::new();
        let wakes = Arc::new(AtomicUsize::new(0));
        q.push(paths(&["/before/the/waker.scraft"]));
        let w = wakes.clone();
        q.set_waker(Arc::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        }));
        q.push(paths(&["/a.scraft", "/b.scraft"]));
        q.push(Vec::new());
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        q.push(paths(&["/c.scraft"]));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        assert_eq!(q.take().len(), 4);
    }

    #[test]
    fn the_waker_may_use_the_queue_itself() {
        let q = Arc::new(Queue::new());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (inner, s) = (Arc::downgrade(&q), seen.clone());
        q.set_waker(Arc::new(move || {
            if let Some(q) = inner.upgrade() {
                s.lock().unwrap().extend(q.take());
            }
        }));
        q.push(paths(&["/first.scraft"]));
        assert_eq!(*seen.lock().unwrap(), paths(&["/first.scraft"]));
        assert!(q.take().is_empty());
    }

    #[test]
    fn at_most_max_pending_paths_wait() {
        let q = Queue::new();
        q.push((0..MAX_PENDING + 10).map(|i| PathBuf::from(format!("/f/{i}.wav"))));
        q.push(paths(&["/late.wav"]));
        let got = q.take();
        assert_eq!(got.len(), MAX_PENDING);
        assert_eq!(got.first(), Some(&PathBuf::from("/f/0.wav")));
        // Once taken, there is room again.
        q.push(paths(&["/late.wav"]));
        assert_eq!(q.take(), paths(&["/late.wav"]));
    }

    #[test]
    fn install_off_the_main_thread_and_twice_is_harmless() {
        std::thread::spawn(|| {
            super::install();
            super::install();
        })
        .join()
        .unwrap();
    }
}
