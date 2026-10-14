//! SoundCraft's VST3 plugin host.
//!
//! Scans the standard VST3 folders (plus `VST3_PATH`), loads `.vst3` bundles in-process and
//! exposes each "Audio Module Class" as a [`soundcraft_dsp::Plugin`], so the mix engine treats
//! third-party plugins exactly like the built-in ones. Plugin ids are `vst3:<class id hex>` (32
//! upper-case hex digits of the component's class id); parameter ids are VST3 `ParamID`s as
//! decimal strings, with titles, units and plain ranges from the edit controller in
//! [`PluginInfo::params`]. Values are plain (display) values; the host converts to and from the
//! VST3 normalized 0..1 values. Stepped parameters become toggles or choices.
//!
//! Like `soundcraft-clap-host`, this is an isolated `unsafe` crate (craftrules never-crash):
//! `unsafe` is denied crate-wide and allowed only in the private `ffi` module. The public API is
//! safe, returns `Result`/`Option` and never panics. Limits of in-process hosting: a plugin that
//! itself crashes (segfault) takes the process down with it; we cannot sandbox native code.
//!
//! Scanning can run out of process ([`set_scan_helper`]), as the SoundCraft apps do: each bundle is
//! loaded in a helper process (the app's own executable, run as
//! `<exe> --soundcraft-scan-vst3 <bundle>` and answering through [`run_scan_helper`]), at most
//! [`HELPER_PARALLEL`] at a time, which prints the bundle's plugins as one line of JSON. A helper
//! that takes longer than [`HELPER_TIMEOUT`] is killed; a bundle whose helper times out, crashes
//! or fails is logged once with the reason and skipped, and remembered ([`skipped`]) so later scans
//! do not try it again until [`rescan`]. A plugin that hangs in its module entry (a PACE/iLok-
//! protected one without its licence service) or crashes while being scanned therefore no longer
//! hangs or kills SoundCraft, and SoundCraft's own process loads only the bundles of the plugins it
//! creates. The helper's standard error (plugin chatter) goes to the debug log. Without a helper
//! (library users, tests) scans load every bundle in this process, as before.
//!
//! A helper scan can keep a cache file ([`ScanHelper::cache`]; the apps put it in their settings
//! folder): per bundle, its plugins or why it was skipped, under the bundle's fingerprint (its
//! binary's size and modification time, the bundle's modification time) and a format version. A
//! later scan, in this run or another, starts helpers only for new or changed bundles, drops the
//! bundles that are gone and writes the file back whole (a temporary file renamed over it). So a
//! plugin that never answers costs its time limit once, not in every run: it stays skipped (logged
//! once per process at info level) until it changes or [`rescan`] retries every bundle and rewrites
//! the cache. An unreadable or corrupt cache is logged and scanned over, never an error. In-process
//! scans use no cache: they exist to load each bundle here, and cannot skip one that hangs anyway.
//!
//! `PluginInfo` must be `&'static`: the registry leaks exactly one `PluginInfo` (with its param
//! table) per plugin *type* the first time that type is instantiated, and caches it. The leak is
//! bounded by the number of distinct installed plugins actually used. Loaded binaries likewise
//! stay mapped for the life of the process (module exit functions are never called).
//!
//! On `wasm32` the crate compiles to stubs: scans are empty and nothing can be created.
//!
//! Plugin state: [`Plugin::save_state`] stores the component state and the controller state in
//! one blob (`SCV3` + length-prefixed parts); [`Plugin::load_state`] restores both (a bare
//! component state is accepted too) and re-reads the parameter values.
//!
//! Offline renders ([`Plugin::set_render_mode`]) set the plugin up with the `kOffline` process
//! mode at the next `prepare` (and pass it in every `process` call); otherwise it is `kRealtime`.
//! A plugin that refuses `kOffline` renders in `kRealtime` (logged once per instance).
//!
//! Plugin editors (`IPlugView`): on macOS the view is attached to the content `NSView` of
//! SoundCraft's plugin window (`soundcraft_macos::plugin_window`, shared with the Audio Unit host:
//! titled and closable, floating above the app, resized on `IPlugFrame::resizeView`, and handing
//! the keys the view does not take to SoundCraft's own window);
//! on Windows it is attached to a host `HWND` owned by the app's window (no taskbar button,
//! closing it hides it until the link tears it down).
//! [`Plugin::open_editor`] opens it directly; [`Plugin::editor`] returns a handle usable on the
//! main thread while the instance processes on the audio thread (VST3's own threading model:
//! the controller and its views live on the UI thread). Parameter edits made in the editor
//! (`IComponentHandler::performEdit`) are reported by the handle's `idle` so the host can write
//! them into the session. Values the host sets reach the processor with the next block and the
//! controller from the UI thread: the state reader's `idle`, every frame, hands it the values set
//! since (`setParamNormalized`, so the editor shows them and a preset the plugin loads afterwards
//! is not set back at the next save), except the editor's own edits coming back from the session,
//! which would set a knob being dragged back, and the values the editor handle's `set_param`
//! already handed it (or older ones). A `performEdit` a controller makes from inside
//! `setParamNormalized` for the parameter the host is setting (the SDK says it must not; some do)
//! is the host's value coming back, not an edit, and is dropped. An instance dropped off the main thread with its editor open (the audio
//! thread drops a torn-down mixer's instances) is not touched there: its view is removed, its
//! window closed and the plugin terminated on the main thread (macOS: the main dispatch queue;
//! Windows: a message to the editor window). Linux (X11) editor windows are not hosted yet:
//! opening reports "unsupported".
//!
//! Not hosted yet: `IMessage`/`IAttributeList` creation through the host (plugins that need it
//! for processor ↔ controller messaging may lose those messages), sidechain (aux buses are kept
//! inactive) and 64-bit processing.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod scan;

#[cfg(not(target_arch = "wasm32"))]
mod cache;
#[cfg(not(target_arch = "wasm32"))]
#[allow(unsafe_code)]
mod ffi;
#[cfg(not(target_arch = "wasm32"))]
mod helper;
#[cfg(not(target_arch = "wasm32"))]
mod plugin;
#[cfg(not(target_arch = "wasm32"))]
mod registry;

#[cfg(not(target_arch = "wasm32"))]
pub use plugin::Vst3Plugin;

#[cfg(all(not(target_arch = "wasm32"), feature = "testing"))]
pub use ffi::instances_awaiting_main_thread;

/// Test seam: scans use these folders instead of the standard ones (`None`: the standard ones
/// again), so a test process never loads the installed plugins. Other processes get the same
/// from `SOUNDCRAFT_VST3_DEFAULT_PATHS` (see [`scan::default_search_paths`]).
#[cfg(all(not(target_arch = "wasm32"), feature = "testing"))]
pub fn set_default_search_paths(paths: Option<Vec<PathBuf>>) {
    registry::set_default_search_paths(paths)
}

use soundcraft_dsp::{Category, Plugin, PluginInfo};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Prefix of every hosted-VST3 plugin id.
pub const ID_PREFIX: &str = "vst3:";

/// The factory class category of audio processors (the only classes SoundCraft lists).
pub const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

/// The 16-byte class id inside a `vst3:<32 hex digits>` SoundCraft id (`None` for other ids).
pub fn parse_id(id: &str) -> Option<[u8; 16]> {
    let hex = id.strip_prefix(ID_PREFIX)?;
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        let pair = hex.get(i * 2..i * 2 + 2)?;
        *o = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// One plugin found by a scan.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Vst3Descriptor {
    /// SoundCraft id: `vst3:<class_id>`.
    pub id: String,
    /// The component's class id as 32 upper-case hex digits.
    pub class_id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    /// The VST3 SDK version the plugin was built with (when reported).
    pub sdk_version: String,
    /// The `|`-separated sub-categories, split (`["Fx", "Reverb"]`).
    pub sub_categories: Vec<String>,
    pub category: Category,
    pub is_instrument: bool,
    /// The `.vst3` bundle or file it lives in.
    pub path: PathBuf,
}

/// Why a VST3 operation failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Vst3Error {
    #[error("cannot load VST3 plugin {0}: {1}")]
    Load(String, String),
    #[error("no VST3 plugin `{0}`")]
    NotFound(String),
    #[error("cannot create VST3 plugin `{0}`: {1}")]
    Instantiate(String, String),
    #[error("VST3 processing: {0}")]
    Process(String),
    #[error("VST3 plugins are not supported on this platform")]
    Unsupported,
}

/// The argument that runs a SoundCraft executable as a scan helper:
/// `<exe> --soundcraft-scan-vst3 <bundle>` (see [`run_scan_helper`]).
pub const SCAN_HELPER_FLAG: &str = "--soundcraft-scan-vst3";

/// How long a scan helper may take over one bundle before it is killed and the bundle skipped
/// (the slowest plugins seen, Omnisphere and Keyscape, take a few seconds).
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(30);

/// How many scan helpers run at once. A helper stuck in a plugin is idle, so several stuck ones
/// cost one time limit rather than one each.
pub const HELPER_PARALLEL: usize = 8;

/// Scanning in helper processes (see [`set_scan_helper`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanHelper {
    /// The executable to run as `<exe> --soundcraft-scan-vst3 <bundle>`: a SoundCraft app that
    /// answers through [`run_scan_helper`].
    pub exe: PathBuf,
    /// How long one bundle may take ([`HELPER_TIMEOUT`]).
    pub timeout: Duration,
    /// How many helpers run at once ([`HELPER_PARALLEL`]).
    pub parallel: usize,
    /// The scan cache file (none: every scan starts a helper for every bundle). Bundles whose
    /// fingerprint (binary size and modification time, bundle modification time) is unchanged
    /// since an earlier scan, in this run or another, are not sent to a helper again: their
    /// plugins, or why they were skipped, come from the cache until they change or [`rescan`]
    /// retries everything.
    pub cache: Option<PathBuf>,
}

impl ScanHelper {
    /// Helpers started from `exe`, with the default time limit and parallelism.
    pub fn new(exe: PathBuf) -> ScanHelper {
        ScanHelper { exe, timeout: HELPER_TIMEOUT, parallel: HELPER_PARALLEL, cache: None }
    }
}

/// A bundle a scan helper could not scan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkippedBundle {
    /// The `.vst3` bundle or file.
    pub path: PathBuf,
    /// Why: the helper timed out, crashed, or the bundle failed to load.
    pub reason: String,
}

#[cfg(not(target_arch = "wasm32"))]
mod api {
    use super::*;

    pub fn cached() -> Option<Vec<Vst3Descriptor>> {
        registry::cached()
    }
    pub fn scan() -> Vec<Vst3Descriptor> {
        registry::scan()
    }
    pub fn rescan() -> Vec<Vst3Descriptor> {
        registry::rescan()
    }
    pub fn add_search_dir(dir: &Path) -> usize {
        registry::add_search_dir(dir)
    }
    pub fn scan_paths(dirs: &[PathBuf]) -> Vec<Vst3Descriptor> {
        registry::scan_paths(dirs)
    }
    pub fn load_bundle(path: &Path) -> Result<Vec<Vst3Descriptor>, Vst3Error> {
        registry::load_bundle(path)
    }
    pub fn instantiate(id: &str) -> Result<Box<dyn Plugin>, Vst3Error> {
        registry::instantiate(id)
    }
    pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
        registry::plugin_info(id)
    }
    pub fn shutdown() {
        ffi::exit_modules();
    }
    pub fn set_scan_helper(helper: Option<ScanHelper>) {
        registry::set_scan_helper(helper)
    }
    pub fn skipped() -> Vec<SkippedBundle> {
        registry::skipped()
    }
    pub fn scan_paths_with(dirs: &[PathBuf], helper: &ScanHelper) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
        registry::scan_paths_with(dirs, helper)
    }
    pub fn run_scan_helper(args: impl IntoIterator<Item = OsString>) -> Option<i32> {
        helper::run(args)
    }
}

#[cfg(target_arch = "wasm32")]
mod api {
    use super::*;

    pub fn cached() -> Option<Vec<Vst3Descriptor>> {
        Some(Vec::new())
    }
    pub fn scan() -> Vec<Vst3Descriptor> {
        Vec::new()
    }
    pub fn rescan() -> Vec<Vst3Descriptor> {
        Vec::new()
    }
    pub fn add_search_dir(_dir: &Path) -> usize {
        0
    }
    pub fn scan_paths(_dirs: &[PathBuf]) -> Vec<Vst3Descriptor> {
        Vec::new()
    }
    pub fn load_bundle(_path: &Path) -> Result<Vec<Vst3Descriptor>, Vst3Error> {
        Err(Vst3Error::Unsupported)
    }
    pub fn instantiate(_id: &str) -> Result<Box<dyn Plugin>, Vst3Error> {
        Err(Vst3Error::Unsupported)
    }
    pub fn plugin_info(_id: &str) -> Option<&'static PluginInfo> {
        None
    }
    pub fn shutdown() {}
    pub fn set_scan_helper(_helper: Option<ScanHelper>) {}
    pub fn skipped() -> Vec<SkippedBundle> {
        Vec::new()
    }
    pub fn scan_paths_with(_dirs: &[PathBuf], _helper: &ScanHelper) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
        (Vec::new(), Vec::new())
    }
    pub fn run_scan_helper(_args: impl IntoIterator<Item = OsString>) -> Option<i32> {
        None
    }
}

/// Every plugin in the standard VST3 folders, `VST3_PATH` and folders added with
/// [`add_search_dir`]. Scanned on first call (loads each binary and reads its factory, but
/// creates no plugin instances; in helper processes when [`set_scan_helper`] set one), then cached
/// in memory.
pub fn scan() -> Vec<Vst3Descriptor> {
    api::scan()
}

/// Forgets the cached scan (and remembered failures and [`skipped`] bundles) and scans again.
pub fn rescan() -> Vec<Vst3Descriptor> {
    api::rescan()
}

/// Adds a folder to the search path and merges its plugins into the cache. Returns how many
/// plugins the folder holds.
pub fn add_search_dir(dir: &Path) -> usize {
    api::add_search_dir(dir)
}

/// Scans the given folders without touching the cache.
pub fn scan_paths(dirs: &[PathBuf]) -> Vec<Vst3Descriptor> {
    api::scan_paths(dirs)
}

/// The plugins of the last scan, without scanning: `None` until something has scanned.
pub fn cached() -> Option<Vec<Vst3Descriptor>> {
    api::cached()
}

/// Shuts plugin hosting down: runs the module exit of every loaded binary that has no live
/// instance left, each on the thread that loaded it; later loads fail. Call it on the main
/// thread once the plugin instances are gone, before the process exits. Without it the exits
/// run from an exit handler, by which time some plugins have already torn down state they need
/// (HALion Sonic crashes in its module exit after it has been used).
pub fn shutdown() {
    api::shutdown()
}

/// The audio module classes in one `.vst3` bundle or file.
pub fn load_bundle(path: &Path) -> Result<Vec<Vst3Descriptor>, Vst3Error> {
    api::load_bundle(path)
}

/// Creates a plugin by `vst3:<class id>`, prepared for 48 kHz stereo with 1024-frame blocks (like
/// `soundcraft_dsp::create`), with the reason when it fails.
pub fn instantiate(id: &str) -> Result<Box<dyn Plugin>, Vst3Error> {
    api::instantiate(id)
}

/// Like [`instantiate`], but returns the concrete [`Vst3Plugin`] (for its extra methods, such
/// as [`Vst3Plugin::editor_size`]).
#[cfg(not(target_arch = "wasm32"))]
pub fn instantiate_plugin(id: &str) -> Result<Vst3Plugin, Vst3Error> {
    registry::instantiate_plugin(id)
}

/// Creates a plugin by `vst3:<class id>`; `None` if unknown or it fails to load.
pub fn create(id: &str) -> Option<Box<dyn Plugin>> {
    parse_id(id)?;
    match api::instantiate(id) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("{e}");
            None
        }
    }
}

/// The description of a `vst3:<class id>` plugin. The first call for a type instantiates it once
/// to read its parameters; the result is cached (and leaked, see the crate docs).
pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    api::plugin_info(id)
}

/// Scans in helper processes from now on (`Some`), or in this process again (`None`, the
/// default). With a helper, [`scan()`], [`rescan`], [`add_search_dir`] and [`scan_paths`] never load
/// a plugin binary here: a bundle that hangs, crashes or fails in its helper is logged and skipped
/// (see [`skipped`]), and [`instantiate`] loads only the bundle of the plugin it creates.
pub fn set_scan_helper(helper: Option<ScanHelper>) {
    api::set_scan_helper(helper)
}

/// The bundles scan helpers could not scan since the last [`rescan`] (which retries them); later
/// scans skip them without starting a helper.
pub fn skipped() -> Vec<SkippedBundle> {
    api::skipped()
}

/// Scans `dirs` in helper processes, whatever [`set_scan_helper`] says, without touching the cache
/// or the remembered [`skipped`] bundles: the plugins found (first id wins, sorted by name) and the
/// bundles skipped.
pub fn scan_paths_with(dirs: &[PathBuf], helper: &ScanHelper) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
    api::scan_paths_with(dirs, helper)
}

/// Answers a scan request when `args` (the process arguments after the executable) are
/// `--soundcraft-scan-vst3 <bundle>`: prints the bundle's plugins on standard output, shuts plugin
/// hosting down and returns the exit status for `std::process::exit`. `None` for any other
/// arguments. Apps call it first thing in `main`, before logging, windows or audio.
pub fn run_scan_helper(args: impl IntoIterator<Item = OsString>) -> Option<i32> {
    api::run_scan_helper(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse() {
        let id = "vst3:0123456789ABCDEFfedcba9876543210";
        assert_eq!(parse_id(id), Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32, 0x10]));
        assert_eq!(parse_id("vst3:"), None);
        assert_eq!(parse_id("vst3:0123"), None);
        assert_eq!(parse_id("vst3:0123456789ABCDEF0123456789ABCDEG"), None);
        assert_eq!(parse_id("vst3:ééééééééééééééééé"), None);
        assert_eq!(parse_id("VST3:0123456789ABCDEF0123456789ABCDEF"), None);
        assert_eq!(parse_id("clap:x"), None);
        assert_eq!(parse_id("eq_7band"), None);
    }

    #[test]
    fn non_vst3_ids_are_rejected_without_scanning() {
        assert!(create("eq_7band").is_none());
        assert!(create("clap:com.x").is_none());
        assert!(plugin_info("reverb").is_none());
    }

    #[test]
    fn garbage_files_are_errors_not_crashes() {
        let d = std::env::temp_dir().join(format!("soundcraft-vst3-garbage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let arch = if cfg!(target_os = "macos") {
            "MacOS"
        } else if cfg!(windows) {
            "x86_64-win"
        } else {
            "x86_64-linux"
        };
        let exe = if cfg!(target_os = "macos") {
            "Bundle"
        } else if cfg!(windows) {
            "Bundle.vst3"
        } else {
            "Bundle.so"
        };
        std::fs::create_dir_all(d.join(format!("Bundle.vst3/Contents/{arch}"))).unwrap();
        std::fs::write(d.join("garbage.vst3"), b"\x7fELF this is not a plugin \x00\x01\x02").unwrap();
        std::fs::write(d.join("empty.vst3"), b"").unwrap();
        std::fs::write(d.join(format!("Bundle.vst3/Contents/{arch}/{exe}")), [0xcf, 0xfa, 0xed, 0xfe, 1, 2, 3]).unwrap();
        std::fs::create_dir_all(d.join("NoBinary.vst3/Contents")).unwrap();
        for f in ["garbage.vst3", "empty.vst3", "Bundle.vst3", "NoBinary.vst3", "missing.vst3"] {
            assert!(load_bundle(&d.join(f)).is_err(), "{f}");
        }
        assert!(scan_paths(std::slice::from_ref(&d)).is_empty());
        let missing = "vst3:00000000000000000000000000000000";
        assert!(create(missing).is_none());
        assert!(plugin_info(missing).is_none());
        assert!(instantiate(missing).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    // Unix only: the test points a ".vst3" at a system library through a symlink.
    #[cfg(unix)]
    #[test]
    fn a_real_library_without_the_vst3_export_is_rejected() {
        // A system library loads fine but has no `GetPluginFactory`.
        // (macOS system libraries live only in the dyld shared cache, not as files.)
        let lib = if cfg!(target_os = "macos") {
            None
        } else {
            ["/lib/x86_64-linux-gnu/libz.so.1", "/usr/lib/libz.so.1", "/lib64/libz.so.1"].iter().map(PathBuf::from).find(|p| p.exists())
        };
        let Some(lib) = lib else { return };
        let d = std::env::temp_dir().join(format!("soundcraft-vst3-nolib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // Point a single-file ".vst3" at it via a symlink (no copying of binaries).
        std::os::unix::fs::symlink(&lib, d.join("z.vst3")).unwrap();
        let e = load_bundle(&d.join("z.vst3")).unwrap_err();
        assert!(e.to_string().contains("not a VST3 plugin"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
