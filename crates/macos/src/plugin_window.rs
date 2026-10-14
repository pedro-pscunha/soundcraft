//! The window hosted plugin editors live in, shared by the VST3 and Audio Unit hosts.
//!
//! A [`PluginWindow`] is titled and closable (not minimizable: a minimized window becomes a Dock
//! thumbnail), floats above SoundCraft's windows, hides while SoundCraft is in the background and
//! comes back with it, stays out of the Window menu and window cycling, and fits itself to its
//! plugin view ([`PluginWindow::fit`], keeping its top-left corner). [`PluginWindow::closed_by_user`]
//! counts a window hidden while the app is inactive as open. Everything runs on the main thread:
//! every method takes a `MainThreadMarker`, and a window dropped elsewhere is leaked rather than
//! touched off the main thread.
//!
//! # Keys
//!
//! Opening an editor makes its window key, so without help every key typed while it is in front
//! (Space, ⌘S, every SoundCraft shortcut) would stop at the plugin. The window is an `NSWindow`
//! subclass that hands on the key events its plugin does not take:
//!
//! - Which: `keyDown:`, `keyUp:` and `flagsChanged:` that reach the window itself, that is, that
//!   no view of the plugin handled (views pass unhandled keys up the responder chain, and the
//!   window is its end).
//! - To whom: the first responder of SoundCraft's main window (winit's view, so egui sees the key
//!   as if its window were key). Plugin windows never become main (`canBecomeMainWindow` is NO),
//!   so SoundCraft's window stays main while a plugin window is key; without a main window, the
//!   frontmost visible window that is no plugin window and can be key. With neither, AppKit's own
//!   handling runs (a beep, as before).
//! - What plugins keep: every key their views handle: text fields, their own shortcuts, a
//!   computer-keyboard piano. `performKeyEquivalent:` is not overridden, so AppKit still offers
//!   ⌘-keys to the plugin's views first and then to the menu bar (Quit, Hide); a ⌘-key nobody
//!   takes arrives as `keyDown:` and is handed on like any other, so ⌘S saves and ⌘W runs
//!   SoundCraft's Close Window.
//!
//! The window stays key after it opens (`makeKeyAndOrderFront`), so a plugin's text field can be
//! typed in at once.
//!
//! # End-to-end check (needs the app and a plugin with an editor)
//!
//! 1. Open a plugin's editor (click its insert): the editor is in front and key. Press Space:
//!    playback starts and stops. Press ⌘S on a modified session: it saves.
//! 2. Click a text field in the plugin's editor (a preset name, a value box) and type: the text
//!    goes to the plugin, and Space types a space instead of starting playback.
//! 3. Press ⌥ or ⇧ with the editor key, then click in the Edit window: SoundCraft sees the
//!    modifier state it would see with its own window key.
//! 4. Switch to another app and back: the editor hides and comes back, still open.

pub use crate::ffi::plugin_window::{DEFAULT_CONTENT, PluginWindow, content_size, fit_content};

/// Test seams (the `testing` feature, never on in the app).
#[cfg(feature = "testing")]
pub mod testing {
    pub use crate::ffi::plugin_window::testing::{set_key_target, set_order_front};
}
