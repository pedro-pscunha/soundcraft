//! The window hosted plugin editors live in, shared by the VST3 and Audio Unit hosts.
//!
//! A [`PluginWindow`] is titled and closable (not minimizable: a minimized window becomes a Dock
//! thumbnail), floats above SoundCraft's windows, hides while SoundCraft is in the background and
//! comes back with it, stays out of the Window menu and window cycling, and fits itself to its
//! plugin view ([`PluginWindow::fit`], keeping its top-left corner). [`PluginWindow::closed_by_user`]
//! counts a window hidden while the app is inactive as open. Everything runs on the main thread:
//! every method takes a `MainThreadMarker`, and a window dropped elsewhere is handed to the main
//! queue ([`on_main_queue`]), which takes it off screen and releases it there.
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
//!   so SoundCraft's window stays main while a plugin window is key; without a main window of
//!   SoundCraft's, the frontmost of SoundCraft's visible windows that can be key. SoundCraft's
//!   windows are winit's (class `WinitWindow`): a window a plugin opened itself (a CLAP plugin's
//!   floating window, a plugin's own panel) never gets the keys, even when it is main or in front.
//!   With no such window, AppKit's own handling runs (a beep, as before). The class name is
//!   winit's (0.30.13), not an API: a unit test fails when Cargo.lock moves winit on, and the app
//!   calls [`check_app_windows`] once its first window exists, which logs an error naming the class
//!   when none of the app's windows has it. SoundCraft does not register its windows instead:
//!   eframe gives the app a window handle for its main window only, not for the separate Mix
//!   window (an egui viewport of its own), so the class would still decide for that one.
//! - Keys still held when the window stops being key or closes: their key-ups and modifier
//!   changes go elsewhere then, so the window lets go of them itself, handing on a key-up for each
//!   key-down it handed on and the modifier state as it is now. SoundCraft never sees a key stuck
//!   down.
//! - Not while SoundCraft has a text field focused, in its main window or the separate Mix window
//!   (the app reports it every frame with [`set_app_wants_keyboard`]; egui keeps focus per window,
//!   so the app asks both): a key-down handed on would type into that field out of sight, so
//!   AppKit's own handling runs (a beep).
//! - What plugins keep: every key their views handle: text fields, their own shortcuts, a
//!   computer-keyboard piano. AppKit offers ⌘-keys to the plugin's views first (the window's
//!   `performKeyEquivalent:` runs NSWindow's own before anything else), then to SoundCraft's menu
//!   bar. The menu bar has few key equivalents (SoundCraft's shortcuts are egui's, Quit and Hide
//!   included): Services shortcuts the user assigned, and Emoji & Symbols (⌃⌘Space) that AppKit
//!   may add to an Edit menu. Every other ⌘-key a plugin does not take arrives as `keyDown:` and is
//!   handed on like any other: ⌘S saves, ⌘Q quits.
//! - The Edit keys: ⌘C, ⌘V, ⌘X, ⌘Z, ⇧⌘Z and ⌘A (by the character typed, no other modifier) that
//!   no plugin view took go to the plugin's focused control as another app's Edit menu sends them
//!   (`copy:`, `paste:`, `cut:`, `undo:`, `redo:`, `selectAll:`): to the first of that control and
//!   the responders after it, up to the window, that implements the action, when it is enabled
//!   there (`validateMenuItem:`, else `validateUserInterfaceItem:`). So a plain Cocoa text field
//!   (Apple's generic view's value boxes, a native plugin UI) copies, pastes and undoes. A text
//!   field's undo is the window's (`undo:` is NSWindow's), so ⌘Z after typing in a plugin's field
//!   undoes that typing. With no control focused (the window itself is first responder), or when
//!   no responder implements the action or it is disabled (nothing selected to copy, nothing to
//!   undo), the key goes on to `keyDown:` and is handed on as before: ⌘Z is SoundCraft's undo.
//! - The other ⌘-keys in a plugin's text field: a plain Cocoa text field edits in a field editor
//!   (an `NSText` whose `isFieldEditor` is true), which keeps a ⌘-key it has no use for in its
//!   `keyDown:` and beeps, so the key never reaches the window. While a field editor is first
//!   responder, a ⌘-key on a character key (⌘S, ⌘N, ⌘Q, ⌘-digits, ⌘Space; any other modifier too)
//!   that no plugin view took and that is not an Edit key is handled in `performKeyEquivalent:` as
//!   AppKit and `keyDown:` handle a key no view took: offered to the menu bar first (a key
//!   equivalent there runs its item, as in any window), then ⌘W closes the editor and the rest is
//!   handed on (not while SoundCraft has a text field focused: the field editor gets the key
//!   then). ⌘ with an arrow, Delete, Return or another function key stays with the field editor,
//!   which binds many of them (⌘→ moves to the end of the line), and so does every key while it
//!   composes text with an input method. Any other first responder gets the key as before: a
//!   plugin's own view, or a text view of its own that is not a field editor (a script or preset
//!   editor); either may take ⌘-keys in its `keyDown:`.
//! - ⌘W (no other modifier) that the plugin does not take closes the editor's own window
//!   (`performClose:`, as its close button does; the editor is taken down at the next idle).
//!   Handed on, it would close SoundCraft's window. ⌘W with another modifier is handed on.
//! - Tab and ⇧Tab (⌃ held or not) that a focused plugin control passed up move the focus to the
//!   plugin's next (previous) control, AppKit's key-view loop, as in any window: when the focused
//!   view has a next (previous) valid key view other than itself. Otherwise they are handed on like
//!   any key (SoundCraft's own Tab): with no view focused (the window itself is first responder),
//!   and with a focused view outside any key-view loop, as a plugin's root view usually is
//!   (VSTGUI's and JUCE's take the focus on a click and pass up the keys they do not use).
//!
//! The window stays key after it opens (`makeKeyAndOrderFront`), so a plugin's text field can be
//! typed in at once.
//!
//! # End-to-end check (needs the app and a plugin with an editor)
//!
//! 1. Open a plugin's editor (click its insert): the editor is in front and key. Press Space:
//!    playback starts and stops. Press ⌘S on a modified session: it saves.
//! 2. With the editor in front, click its insert again: the editor closes. Click once more: it
//!    opens.
//! 3. Click a text field in the plugin's editor (a preset name, a value box) and type: the text
//!    goes to the plugin, and Space types a space instead of starting playback. In a plain Cocoa
//!    field (an Audio Unit with Apple's generic view): ⌘A selects all of it, ⌘C then ⌘V pastes a
//!    copy, ⌘Z undoes the paste and ⇧⌘Z redoes it, ⌘X cuts, and the text pasted into another app
//!    is the one copied. SoundCraft copies, pastes and undoes nothing meanwhile. With the field
//!    still focused, press ⌘S on a modified session: it saves, without a beep, and the field keeps
//!    its text; ⌘→ moves to the end of the field; if SoundCraft's Edit menu lists Emoji & Symbols,
//!    ⌃⌘Space opens the Character Viewer; ⌘W closes the editor. Close the editor, open it again
//!    and, before clicking in it (no field focused), press ⌘Z: SoundCraft undoes its last edit.
//! 4. With the editor key and no field of the plugin focused, press ⇧Space: playback starts at
//!    half speed (a shortcut that needs its modifier). Hold ⌥ or ⇧ and click in the Edit window:
//!    SoundCraft sees the modifier as with its own window key. Hold ⇧ in the editor, ⌘Tab to
//!    another app and back, let go: Space plays at normal speed (nothing stays held).
//! 5. Press ⌘W with the editor key: the editor closes, SoundCraft's window stays.
//! 6. Start renaming a track (its name field focused), click the editor's background and type a
//!    letter: a beep, and the name does not change. The same with the Mix window in its own window
//!    (Window › Mix in Separate Window): focus a field there (a track's comments, the plugin
//!    search), open an editor from that window's insert, click the editor's background and type a
//!    letter: a beep, and the field does not change.
//! 7. In an editor with several text fields, Tab moves to the next and ⇧Tab back. In a VST3 editor
//!    drawn by VSTGUI or JUCE, click a knob and press Tab: SoundCraft's Tab runs (Tab to
//!    Transient in the Edit window).
//! 8. With a CLAP plugin's own floating window open next to an Audio Unit or VST3 editor, press
//!    Space with the latter key: playback starts (the key never goes to the CLAP window).
//! 9. Switch to another app and back: the editor hides and comes back, still open.

pub use crate::ffi::plugin_window::{
    DEFAULT_CONTENT, MainThreadJob, PluginWindow, check_app_windows, content_size, fit_content, on_main_queue, set_app_wants_keyboard,
};

/// Test seams (the `testing` feature, never on in the app).
#[cfg(feature = "testing")]
pub mod testing {
    pub use crate::ffi::plugin_window::testing::{choose_target, set_app_active, set_key_target, set_order_front};
}
