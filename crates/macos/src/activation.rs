//! When SoundCraft last became the active application.
//!
//! macOS hides plugin editors while SoundCraft is in the background and shows them again when it
//! comes forward (see `plugin_window`), so the click that brings SoundCraft forward lands on an
//! editor that has just come back. The UI tells that click from later ones by its timing:
//! [`last_activated`] is the moment SoundCraft last became the active application, recorded by an
//! observer of `NSApplicationDidBecomeActiveNotification` that [`install`] registers.
//!
//! Window focus is no substitute: an editor's window becomes key when it opens or is raised, so
//! SoundCraft's own window regains focus with the next click on it while SoundCraft stays the
//! active application all along.
//!
//! Off macOS [`install`] does nothing and [`last_activated`] is always `None`.

use std::sync::{Mutex, PoisonError};
use std::time::Instant;

/// Starts recording SoundCraft's activations. Call it once at start-up, from any thread (later
/// calls do nothing); activations before it are not recorded. Off macOS it does nothing.
pub fn install() {
    #[cfg(target_os = "macos")]
    crate::ffi::activation::install();
}

/// When SoundCraft last became the active application: `None` before the first activation after
/// [`install`], and always off macOS.
pub fn last_activated() -> Option<Instant> {
    *LAST.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Records that SoundCraft became the active application at `at` (the observer calls it).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn activated(at: Instant) {
    *LAST.lock().unwrap_or_else(PoisonError::into_inner) = Some(at);
}

static LAST: Mutex<Option<Instant>> = Mutex::new(None);

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    #[test]
    fn nothing_is_recorded_off_macos() {
        super::install();
        assert_eq!(super::last_activated(), None);
    }
}
