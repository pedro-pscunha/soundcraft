//! SoundCraft's macOS app integration.
//!
//! - [`open_documents`]: the files macOS opens with SoundCraft (Finder, the Dock, `open -a`) reach
//!   the app.
//! - `plugin_window` (macOS only): the window hosted plugin editors live in, shared by the VST3
//!   and Audio Unit hosts; it hands the keys a plugin does not take to SoundCraft's own window.
//!
//! # Safety
//!
//! Like `soundcraft-au-host`, this is an isolated `unsafe` crate (craftrules never-crash): `unsafe`
//! is denied crate-wide and allowed only in the private `ffi` module (`src/ffi/`), with a
//! `// SAFETY:` comment on every block. The public API is safe and never panics. Off macOS the
//! crate compiles to stubs.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod ffi;

pub mod open_documents;
#[cfg(target_os = "macos")]
pub mod plugin_window;
