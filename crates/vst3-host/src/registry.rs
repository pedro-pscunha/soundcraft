//! The process-wide VST3 registry: scan cache, leaked `PluginInfo`s, instantiation.

use crate::cache::{self, Fingerprint, Outcome, ScanCache};
use crate::ffi::{Bundle, Instance, PARAM_BYPASS, PARAM_HIDDEN, PARAM_READ_ONLY, RawParam};
use crate::helper::Skip;
use crate::plugin::Vst3Plugin;
use crate::scan as paths;
use crate::{AUDIO_MODULE_CLASS, ScanHelper, SkippedBundle, Vst3Descriptor, Vst3Error, parse_id};
use soundcraft_dsp::{ParamInfo, Plugin, PluginInfo, Taper, Unit};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};

/// Most labels read for a stepped parameter (more steps are shown as a plain range).
const MAX_CHOICES: i32 = 128;

#[derive(Default)]
struct Registry {
    scanned: Option<Vec<Vst3Descriptor>>,
    extra_dirs: Vec<PathBuf>,
    /// One leaked `PluginInfo` per plugin type that was actually instantiated (bounded by the
    /// number of distinct installed plugins).
    infos: HashMap<String, &'static PluginInfo>,
    /// Ids that failed to instantiate (not retried every UI frame).
    failed: HashSet<String>,
    /// Scan in helper processes (none: in this process).
    helper: Option<ScanHelper>,
    /// Bundles a helper could not scan, not tried again until `rescan`.
    skipped: Vec<SkippedBundle>,
    /// Bundles not scanned because the scan helper failed on SoundCraft's side, as the last scans
    /// left them: listed with `skipped`, but tried again by the next scan.
    not_scanned: Vec<SkippedBundle>,
    /// Replaces the standard folders (tests: see `set_default_search_paths`).
    default_paths: Option<Vec<PathBuf>>,
    /// Bumped by every rescan: a scan that began before one does not store its result.
    generation: u64,
    /// A rescan was asked for and no scan has begun since: whichever caller begins the next scan
    /// makes it a rescan (the one who asked may not be the one who gets the lock first).
    rescan_pending: bool,
    /// A scan of the search folders is running (without the lock); others wait for it on
    /// [`scan_done`].
    scanning: bool,
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(PoisonError::into_inner)
}

/// Signalled when a scan of the search folders ends.
fn scan_done() -> &'static Condvar {
    static C: OnceLock<Condvar> = OnceLock::new();
    C.get_or_init(Condvar::new)
}

/// Adds `new` to `list`, an entry for the same bundle replacing the older one.
fn add_skips(list: &mut Vec<SkippedBundle>, new: impl IntoIterator<Item = SkippedBundle>) {
    for s in new {
        match list.iter_mut().find(|k| k.path == s.path) {
            Some(k) => *k = s,
            None => list.push(s),
        }
    }
}

/// Adds the bundles a scan that ran without the lock skipped, or could not scan, to the
/// registry's.
fn remember_skips(new: Vec<SkippedBundle>) {
    if new.is_empty() {
        return;
    }
    let (not_scanned, skipped): (Vec<_>, Vec<_>) = new.into_iter().partition(|s| s.helper_failed);
    let mut r = lock();
    add_skips(&mut r.skipped, skipped);
    add_skips(&mut r.not_scanned, not_scanned);
}

/// The revision of how a bundle is scanned: bump it when `Bundle::load` or the way descriptors are
/// derived from a factory ([`load_bundle`]) changes. Scan caches written by another revision (or
/// another SoundCraft version, or helper protocol) start cold, so an update's scanning fixes reach
/// bundles scanned before it.
pub(crate) const SCANNER_REVISION: u32 = 1;

/// Descriptors of every audio module class in one `.vst3`.
pub fn load_bundle(path: &Path) -> Result<Vec<Vst3Descriptor>, Vst3Error> {
    let b = Bundle::load(path)?;
    Ok(b.classes()
        .into_iter()
        .filter(|c| c.category == AUDIO_MODULE_CLASS)
        .map(|c| {
            let subs = paths::split_sub_categories(&c.sub_categories);
            let class_id = crate::ffi::hex(&c.cid);
            Vst3Descriptor {
                id: format!("{}{class_id}", crate::ID_PREFIX),
                category: paths::category_from_sub_categories(&subs),
                is_instrument: paths::is_instrument(&subs),
                class_id,
                name: if c.name.is_empty() { "VST3 plugin".to_string() } else { c.name },
                vendor: c.vendor,
                version: c.version,
                sdk_version: c.sdk_version,
                sub_categories: subs,
                path: path.to_path_buf(),
            }
        })
        .collect())
}

/// Adds the plugins of one bundle to `out` (the first plugin with a given id wins).
fn merge(out: &mut Vec<Vst3Descriptor>, found: Vec<Vst3Descriptor>) {
    for d in found {
        if !out.iter().any(|o| o.id == d.id) {
            out.push(d);
        }
    }
}

/// Loads every bundle in `dirs` here, sorted by name; unloadable ones are logged and left out.
fn scan_in_process(dirs: &[PathBuf]) -> Vec<Vst3Descriptor> {
    let mut out = Vec::new();
    for path in paths::find_bundles(dirs) {
        match load_bundle(&path) {
            Ok(ds) => merge(&mut out, ds),
            Err(e) => log::warn!("{e}"),
        }
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// One bundle on its way through a helper scan.
struct Pending {
    path: PathBuf,
    /// Its fingerprint now (with a scan cache).
    print: Option<Fingerprint>,
    /// Its helper timed out on the last scan too (the cache says so, for this fingerprint).
    timed_out_before: bool,
    state: State,
}

/// Where a bundle of a helper scan stands.
enum State {
    /// Answered by the scan cache: not scanned again.
    Cached(Outcome),
    /// Waiting for a helper.
    Todo,
    /// Answered by its helper.
    Done(Result<Vec<Vst3Descriptor>, Skip>),
}

/// A bundle's name for a log line.
fn bundle_name(path: &Path) -> String {
    path.file_stem().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Scans the bundles in `dirs` in helpers, sorted by name; bundles in `known` (skipped before) are
/// left out. Returns the plugins and the bundles skipped now, each logged. With a scan cache,
/// unchanged bundles come from the cache unless `fresh` (a rescan), and what the helpers find is
/// written back to it.
///
/// When the helpers fail on SoundCraft's side (cannot start, no scan helper of this protocol), the
/// bundles they had not scanned are loaded here instead, but only those the scan cache knows to
/// load (their last outcome, for the same fingerprint, is a list of plugins; looked up on a rescan
/// too). The others, and any bundle a stuck helper was running, are listed as not scanned
/// ([`SkippedBundle::helper_failed`]): they may hang, and nothing here can stop that.
fn scan_in_helpers(dirs: &[PathBuf], h: &ScanHelper, known: &[SkippedBundle], fresh: bool) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
    let mut cache = h.cache.as_deref().map(ScanCache::load);
    let mut pending: Vec<Pending> = paths::find_bundles(dirs)
        .into_iter()
        .filter(|p| !known.iter().any(|s| &s.path == p))
        .map(|path| {
            let print = cache.as_ref().and_then(|_| cache::fingerprint(&path));
            let known = match (&cache, print) {
                (Some(c), Some(print)) if !fresh => c.lookup(&path, print).cloned(),
                _ => None,
            };
            let timed_out_before = matches!(known, Some(Outcome::TimedOutOnce(_)));
            // A first timeout is tried again: only a second one in a row is a verdict.
            let state = match known {
                Some(Outcome::TimedOutOnce(_)) | None => State::Todo,
                Some(outcome) => State::Cached(outcome),
            };
            Pending { path, print, timed_out_before, state }
        })
        .collect();
    let earlier: Vec<String> =
        pending.iter().filter(|p| matches!(p.state, State::Cached(Outcome::Skipped(_)))).map(|p| bundle_name(&p.path)).collect();
    if !earlier.is_empty() {
        log::info!("vst3: {} bundles skipped by an earlier scan ({}); rescan to retry", earlier.len(), earlier.join(", "));
    }
    let again: Vec<String> = pending.iter().filter(|p| p.timed_out_before).map(|p| bundle_name(&p.path)).collect();
    if !again.is_empty() {
        log::info!("vst3: scanning {} bundles that timed out last time once more ({})", again.len(), again.join(", "));
    }
    let todo: Vec<PathBuf> = pending.iter().filter(|p| matches!(p.state, State::Todo)).map(|p| p.path.clone()).collect();
    // One answer per bundle to do, in the same order.
    let answers = crate::helper::scan_bundles(h, &todo);
    for (p, answer) in pending.iter_mut().filter(|p| matches!(p.state, State::Todo)).zip(answers) {
        if let Err(Skip::Bundle(why) | Skip::TimedOut(why)) = &answer {
            log::warn!("vst3: skipped {}: {why}", p.path.display());
        }
        let outcome = match &answer {
            Ok(ds) => Some(Outcome::Plugins(ds.clone())),
            Err(Skip::Bundle(why)) => Some(Outcome::Skipped(why.clone())),
            // Provisional the first time; a second timeout in a row is a verdict.
            Err(Skip::TimedOut(why)) if p.timed_out_before => Some(Outcome::Skipped(why.clone())),
            Err(Skip::TimedOut(why)) => Some(Outcome::TimedOutOnce(why.clone())),
            Err(Skip::Host(_) | Skip::HostStuck(_)) => None,
        };
        if let (Some(c), Some(print), Some(outcome)) = (&mut cache, p.print, outcome) {
            c.record(&p.path, print, outcome);
        }
        p.state = State::Done(answer);
    }
    // Known to load: the cache's last outcome for this very build of the bundle is its plugins.
    let loads = |p: &Pending| match (&cache, p.print) {
        (Some(c), Some(print)) => matches!(c.lookup(&p.path, print), Some(Outcome::Plugins(_))),
        _ => false,
    };
    let failed = pending.iter().find_map(|p| match &p.state {
        State::Done(Err(Skip::Host(why) | Skip::HostStuck(why))) => Some(why.clone()),
        _ => None,
    });
    if let Some(why) = &failed {
        let here = pending.iter().filter(|p| matches!(p.state, State::Done(Err(Skip::Host(_)))) && loads(p)).count();
        let not = pending.iter().filter(|p| matches!(p.state, State::Done(Err(Skip::Host(_) | Skip::HostStuck(_))))).count() - here;
        log::warn!("vst3: the scan helper failed ({why}); loading {here} bundles known to load in this process, {not} not scanned");
    }
    let not_scanned = |path: PathBuf, why: &str| SkippedBundle {
        path,
        reason: crate::helper::clean_reason(&format!("not scanned: the scan helper failed: {why}")),
        from_cache: false,
        helper_failed: true,
    };
    let (mut out, mut skipped) = (Vec::new(), Vec::new());
    for p in pending {
        let known_to_load = loads(&p);
        match p.state {
            State::Cached(Outcome::Plugins(ds)) | State::Done(Ok(ds)) => merge(&mut out, ds),
            State::Cached(Outcome::Skipped(reason)) => skipped.push(SkippedBundle { path: p.path, reason, from_cache: true, helper_failed: false }),
            State::Done(Err(Skip::Bundle(reason) | Skip::TimedOut(reason))) => {
                skipped.push(SkippedBundle { path: p.path, reason, from_cache: false, helper_failed: false })
            }
            State::Done(Err(Skip::Host(why))) if known_to_load => match load_bundle(&p.path) {
                Ok(ds) => merge(&mut out, ds),
                Err(e) => {
                    log::warn!("{e}");
                    skipped.push(not_scanned(p.path, &why));
                }
            },
            State::Done(Err(Skip::Host(why) | Skip::HostStuck(why))) => skipped.push(not_scanned(p.path, &why)),
            State::Cached(Outcome::TimedOutOnce(_)) | State::Todo => {}
        }
    }
    if let Some(c) = &mut cache {
        c.drop_gone();
        c.save();
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    (out, skipped)
}

/// Scans `dirs` without the registry lock: in helpers when one is set, leaving out the bundles in
/// `known` (returns the bundles skipped now), else in this process.
fn scan_unlocked(dirs: &[PathBuf], helper: Option<&ScanHelper>, known: &[SkippedBundle], fresh: bool) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
    match helper {
        Some(h) => scan_in_helpers(dirs, h, known, fresh),
        None => (scan_in_process(dirs), Vec::new()),
    }
}

/// Scans `dirs` (uncached), in helpers when one is set (remembering what they skip).
pub fn scan_paths(dirs: &[PathBuf]) -> Vec<Vst3Descriptor> {
    let (helper, known) = {
        let r = lock();
        (r.helper.clone(), r.skipped.clone())
    };
    let (found, new) = scan_unlocked(dirs, helper.as_ref(), &known, false);
    remember_skips(new);
    found
}

/// Scans `dirs` in helpers, leaving the registry alone.
pub fn scan_paths_with(dirs: &[PathBuf], helper: &ScanHelper) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
    scan_in_helpers(dirs, helper, &[], false)
}

/// The plugins of the search folders, scanned once (`fresh`: again, without the scan cache's
/// answers, for a rescan). The scan runs without the registry lock, so the registry answers other
/// calls meanwhile (helpers may take their time limit); callers that need the list wait for it.
///
/// Returns the plugins with the bundles skipped, read together, so they describe the same scan.
fn ensure_scanned(fresh: bool) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
    let mut r = lock();
    if fresh {
        r.generation = r.generation.wrapping_add(1);
        r.scanned = None;
        r.failed.clear();
        r.skipped.clear();
        r.not_scanned.clear();
        r.rescan_pending = true;
    }
    loop {
        if let Some(list) = &r.scanned {
            return (list.clone(), all_skipped(&r));
        }
        if !r.scanning {
            break;
        }
        r = scan_done().wait(r).unwrap_or_else(PoisonError::into_inner);
    }
    let fresh = std::mem::take(&mut r.rescan_pending) || fresh;
    r.scanning = true;
    let generation = r.generation;
    let mut dirs = r.default_paths.clone().unwrap_or_else(paths::default_search_paths);
    dirs.extend(r.extra_dirs.iter().cloned());
    let helper = r.helper.clone();
    let known = r.skipped.clone();
    drop(r);
    /// Ends the scan however it ends, so the callers waiting for it go on.
    struct Scanning;
    impl Drop for Scanning {
        fn drop(&mut self) {
            lock().scanning = false;
            scan_done().notify_all();
        }
    }
    let _scanning = Scanning;
    let (found, new) = scan_unlocked(&dirs, helper.as_ref(), &known, fresh);
    let mut r = lock();
    // A rescan that began meanwhile wins: it scans again once this one has ended. This scan's
    // own result goes back to its caller.
    if r.generation != generation {
        let mut own = known;
        add_skips(&mut own, new);
        return (found, own);
    }
    r.scanned = Some(found.clone());
    let (not_scanned, skipped): (Vec<_>, Vec<_>) = new.into_iter().partition(|s| s.helper_failed);
    // Added to what is there: other scans (a folder added meanwhile) may have skipped bundles too.
    add_skips(&mut r.skipped, skipped);
    // This scan covered every folder: only its own bundles stay not scanned.
    r.not_scanned = not_scanned;
    (found, all_skipped(&r))
}

/// The bundles skipped, then those not scanned (see `skipped`).
fn all_skipped(r: &Registry) -> Vec<SkippedBundle> {
    r.skipped.iter().chain(&r.not_scanned).cloned().collect()
}

pub fn scan() -> Vec<Vst3Descriptor> {
    ensure_scanned(false).0
}

pub fn scan_with_skipped(rescan: bool) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
    ensure_scanned(rescan)
}

pub fn cached() -> Option<Vec<Vst3Descriptor>> {
    lock().scanned.clone()
}

pub fn rescan() -> Vec<Vst3Descriptor> {
    ensure_scanned(true).0
}

/// Adds `dir` to the search folders and returns how many plugins it holds. Its plugins join the
/// scan when there is one; with none yet, the first scan covers it together with the standard
/// folders.
pub fn add_search_dir(dir: &Path) -> usize {
    let (helper, known) = {
        let mut r = lock();
        if !r.extra_dirs.iter().any(|d| d == dir) {
            r.extra_dirs.push(dir.to_path_buf());
        }
        (r.helper.clone(), r.skipped.clone())
    };
    let (found, new) = scan_unlocked(&[dir.to_path_buf()], helper.as_ref(), &known, false);
    let n = found.len();
    remember_skips(new);
    // A scan of the search folders running now began without this folder: its result is merged
    // into that scan's once it is stored.
    let mut r = lock();
    while r.scanning {
        r = scan_done().wait(r).unwrap_or_else(PoisonError::into_inner);
    }
    if let Some(list) = r.scanned.as_mut() {
        merge(list, found);
    }
    n
}

#[cfg(feature = "testing")]
pub fn set_default_search_paths(paths: Option<Vec<PathBuf>>) {
    lock().default_paths = paths;
}

pub fn set_scan_helper(helper: Option<ScanHelper>) {
    lock().helper = helper;
}

pub fn skipped() -> Vec<SkippedBundle> {
    all_skipped(&lock())
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Maps a VST3 unit label to a SoundCraft display unit.
pub(crate) fn unit_from_label(units: &str) -> Unit {
    match units.trim().to_ascii_lowercase().as_str() {
        "db" => Unit::Db,
        "hz" => Unit::Hz,
        "ms" => Unit::Ms,
        "%" => Unit::Percent,
        "s" | "sec" | "secs" | "seconds" => Unit::Seconds,
        "st" | "semi" | "semitones" => Unit::Semitones,
        "ct" | "cent" | "cents" => Unit::Cents,
        _ => Unit::None,
    }
}

fn param_info(inst: &Instance, p: &RawParam) -> ParamInfo {
    let name = if p.title.is_empty() { format!("Param {}", p.id) } else { p.title.clone() };
    let base = ParamInfo {
        id: leak(p.id.to_string()),
        name: leak(name),
        min: 0.0,
        max: 1.0,
        default: 0.0,
        unit: Unit::None,
        taper: Taper::Linear,
        choices: &[],
    };
    if p.step_count == 1 {
        return ParamInfo { unit: Unit::Toggle, default: p.default_normalized.round() as f32, ..base };
    }
    if (2..=MAX_CHOICES).contains(&p.step_count) {
        let steps = f64::from(p.step_count);
        let labels: Vec<&'static str> =
            (0..=p.step_count).map(|i| leak(inst.value_text(p.id, f64::from(i) / steps).unwrap_or_else(|| i.to_string()))).collect();
        return ParamInfo {
            max: p.step_count as f32,
            default: (p.default_normalized * steps).round() as f32,
            unit: Unit::Choice,
            choices: Box::leak(labels.into_boxed_slice()),
            ..base
        };
    }
    // Continuous (or too many steps for a menu): plain range from the controller.
    let unit = unit_from_label(&p.units);
    match (inst.to_plain(p.id, 0.0), inst.to_plain(p.id, 1.0)) {
        (Some(lo), Some(hi)) if hi > lo && (lo as f32).is_finite() && (hi as f32).is_finite() && (hi as f32) > (lo as f32) => {
            let default = inst.to_plain(p.id, p.default_normalized).map_or(lo, |d| d.clamp(lo, hi));
            // A logarithmic plugin mapping (frequency knobs) shows its geometric mean at mid travel.
            let taper = match inst.to_plain(p.id, 0.5) {
                Some(mid)
                    if lo > 0.0
                        && ((mid - (lo * hi).sqrt()).abs() <= 0.01 * (lo * hi).sqrt())
                        && (mid - (lo + hi) / 2.0).abs() > 0.01 * (hi - lo) =>
                {
                    Taper::Log
                }
                _ => Taper::Linear,
            };
            ParamInfo { min: lo as f32, max: hi as f32, default: default as f32, unit, taper, ..base }
        }
        _ => ParamInfo { default: p.default_normalized as f32, unit, ..base },
    }
}

/// Builds and leaks the `PluginInfo` for one plugin type. Called at most once per id (cached).
fn build_info(d: &Vst3Descriptor, inst: &Instance) -> &'static PluginInfo {
    let params: Vec<ParamInfo> =
        inst.params().iter().filter(|p| p.flags & (PARAM_HIDDEN | PARAM_READ_ONLY | PARAM_BYPASS) == 0).map(|p| param_info(inst, p)).collect();
    let short: String = d.name.chars().take(8).collect();
    Box::leak(Box::new(PluginInfo {
        id: leak(d.id.clone()),
        name: leak(d.name.clone()),
        short_name: leak(short),
        category: d.category,
        params: Box::leak(params.into_boxed_slice()),
        is_instrument: d.is_instrument,
        audiosuite: false,
    }))
}

/// Finds the descriptor, loads its binary and creates an instance plus its (cached) info.
fn instantiate_raw(id: &str) -> Result<(Instance, &'static PluginInfo), Vst3Error> {
    let cid = parse_id(id).ok_or_else(|| Vst3Error::NotFound(id.to_string()))?;
    if lock().failed.contains(id) {
        return Err(Vst3Error::Instantiate(id.to_string(), "failed before".into()));
    }
    let desc = ensure_scanned(false).0.into_iter().find(|d| d.id == id).ok_or_else(|| Vst3Error::NotFound(id.to_string()))?;
    let made = Bundle::load(&desc.path).and_then(|b| b.instantiate(&cid));
    let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    match made {
        Ok(inst) => {
            let info = match r.infos.get(id) {
                Some(i) => *i,
                None => {
                    let i = build_info(&desc, &inst);
                    r.infos.insert(id.to_string(), i);
                    i
                }
            };
            Ok((inst, info))
        }
        Err(e) => {
            r.failed.insert(id.to_string());
            Err(e)
        }
    }
}

pub fn instantiate(id: &str) -> Result<Box<dyn Plugin>, Vst3Error> {
    Ok(Box::new(instantiate_plugin(id)?))
}

pub fn instantiate_plugin(id: &str) -> Result<Vst3Plugin, Vst3Error> {
    let (inst, info) = instantiate_raw(id)?;
    let mut p = Vst3Plugin::new(inst, info);
    p.prepare(48_000.0, 1024, 2);
    Ok(p)
}

pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    parse_id(id)?;
    {
        let r = registry().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = r.infos.get(id) {
            return Some(*i);
        }
        if r.failed.contains(id) {
            return None;
        }
    }
    // Instantiate once to learn the parameters (VST3 exposes them only on a live controller).
    instantiate_raw(id).ok().map(|(_, info)| info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_labels_map() {
        assert_eq!(unit_from_label("dB"), Unit::Db);
        assert_eq!(unit_from_label(" Hz "), Unit::Hz);
        assert_eq!(unit_from_label("%"), Unit::Percent);
        assert_eq!(unit_from_label("ms"), Unit::Ms);
        assert_eq!(unit_from_label("sec"), Unit::Seconds);
        assert_eq!(unit_from_label("cents"), Unit::Cents);
        assert_eq!(unit_from_label("st"), Unit::Semitones);
        assert_eq!(unit_from_label("furlongs"), Unit::None);
    }
}
