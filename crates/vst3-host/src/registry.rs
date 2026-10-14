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
    /// Replaces the standard folders (tests: see `set_default_search_paths`).
    default_paths: Option<Vec<PathBuf>>,
    /// Bumped by every rescan: a scan that began before one does not store its result.
    generation: u64,
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

/// Adds the bundles skipped by a scan that ran without the lock (those past `known` in
/// `skipped`, a copy taken before it) to the registry's.
fn remember_skips(mut skipped: Vec<SkippedBundle>, known: usize) {
    if skipped.len() > known {
        let mut r = lock();
        for s in skipped.drain(known..) {
            if !r.skipped.iter().any(|k| k.path == s.path) {
                r.skipped.push(s);
            }
        }
    }
}

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

/// Scans `dirs`, sorted by name: in helpers when there is one, else in this process.
fn scan_dirs(dirs: &[PathBuf], helper: Option<&ScanHelper>, skipped: &mut Vec<SkippedBundle>, fresh: bool) -> Vec<Vst3Descriptor> {
    let bundles = paths::find_bundles(dirs);
    let mut out = match helper {
        Some(h) => scan_in_helpers(bundles, h, skipped, fresh),
        None => scan_in_process(bundles),
    };
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// Loads every bundle here; unloadable ones are logged and left out.
fn scan_in_process(bundles: Vec<PathBuf>) -> Vec<Vst3Descriptor> {
    let mut out = Vec::new();
    for path in bundles {
        match load_bundle(&path) {
            Ok(ds) => merge(&mut out, ds),
            Err(e) => log::warn!("{e}"),
        }
    }
    out
}

/// Scans bundles in helpers. Bundles in `skipped` are left out, and a bundle its helper cannot scan
/// is logged and added to `skipped`. With a scan cache, unchanged bundles come from the cache unless
/// `fresh` (a rescan), and what the helpers find is written back to it. When the helpers fail on
/// SoundCraft's side (cannot start, no scan helper of this protocol), the bundles they had not
/// scanned are loaded here instead, as without a helper: logged once, nothing remembered.
fn scan_in_helpers(bundles: Vec<PathBuf>, h: &ScanHelper, skipped: &mut Vec<SkippedBundle>, fresh: bool) -> Vec<Vst3Descriptor> {
    let mut out = Vec::new();
    let bundles: Vec<PathBuf> = bundles.into_iter().filter(|p| !skipped.iter().any(|s| &s.path == p)).collect();
    let mut cache = h.cache.as_deref().map(ScanCache::load);
    let prints: Vec<Option<Fingerprint>> = bundles.iter().map(|p| cache.as_ref().and_then(|_| cache::fingerprint(p))).collect();
    // What the cache already knows (nothing on a rescan), then the rest from helpers.
    let mut found: Vec<Option<Result<Vec<Vst3Descriptor>, Skip>>> = bundles
        .iter()
        .zip(&prints)
        .map(|(path, print)| match (&cache, print) {
            (Some(c), Some(print)) if !fresh => c.lookup(path, *print).map(|o| match o {
                Outcome::Plugins(ds) => Ok(ds.clone()),
                Outcome::Skipped(why) => Err(Skip::Bundle(why.clone())),
            }),
            _ => None,
        })
        .collect();
    let cached: Vec<bool> = found.iter().map(Option::is_some).collect();
    let earlier: Vec<String> = bundles
        .iter()
        .zip(&found)
        .filter(|(_, f)| matches!(f, Some(Err(_))))
        .map(|(p, _)| p.file_stem().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()))
        .collect();
    let todo: Vec<PathBuf> = bundles.iter().zip(&found).filter(|(_, f)| f.is_none()).map(|(p, _)| p.clone()).collect();
    let mut answers = crate::helper::scan_bundles(h, &todo).into_iter();
    for ((path, print), slot) in bundles.iter().zip(&prints).zip(found.iter_mut()) {
        if slot.is_some() {
            continue;
        }
        let Some(answer) = answers.next() else { break };
        if let Err(Skip::Bundle(why)) = &answer {
            log::warn!("vst3: skipped {}: {why}", path.display());
        }
        let outcome = match &answer {
            Ok(ds) => Some(Outcome::Plugins(ds.clone())),
            Err(Skip::Bundle(why)) => Some(Outcome::Skipped(why.clone())),
            Err(Skip::Host(_)) => None,
        };
        if let (Some(c), Some(print), Some(outcome)) = (&mut cache, print, outcome) {
            c.record(path, *print, outcome);
        }
        *slot = Some(answer);
    }
    if !earlier.is_empty() {
        log::info!("vst3: {} bundles skipped by an earlier scan ({}); rescan to retry", earlier.len(), earlier.join(", "));
    }
    let failed: Vec<&str> = found
        .iter()
        .filter_map(|f| match f {
            Some(Err(Skip::Host(why))) => Some(why.as_str()),
            _ => None,
        })
        .collect();
    if let Some(why) = failed.first() {
        log::warn!("vst3: the scan helper failed ({why}); scanning {} bundles in this process instead", failed.len());
    }
    for ((path, f), from_cache) in bundles.iter().zip(found).zip(cached) {
        match f {
            Some(Ok(ds)) => merge(&mut out, ds),
            Some(Err(Skip::Bundle(why))) => skipped.push(SkippedBundle { path: path.clone(), reason: why, from_cache }),
            Some(Err(Skip::Host(_))) => match load_bundle(path) {
                Ok(ds) => merge(&mut out, ds),
                Err(e) => log::warn!("{e}"),
            },
            None => {}
        }
    }
    if let Some(c) = &mut cache {
        c.drop_gone();
        c.save();
    }
    out
}

/// Scans `dirs` (uncached), in helpers when one is set (remembering what they skip).
pub fn scan_paths(dirs: &[PathBuf]) -> Vec<Vst3Descriptor> {
    let (helper, mut skipped) = {
        let r = lock();
        (r.helper.clone(), r.skipped.clone())
    };
    let known = skipped.len();
    let found = scan_dirs(dirs, helper.as_ref(), &mut skipped, false);
    remember_skips(skipped, known);
    found
}

/// Scans `dirs` in helpers, leaving the registry alone.
pub fn scan_paths_with(dirs: &[PathBuf], helper: &ScanHelper) -> (Vec<Vst3Descriptor>, Vec<SkippedBundle>) {
    let mut skipped = Vec::new();
    let found = scan_dirs(dirs, Some(helper), &mut skipped, false);
    (found, skipped)
}

/// The plugins of the search folders, scanned once (`fresh`: again, without the scan cache's
/// answers, for a rescan). The scan runs without the registry lock, so the registry answers other
/// calls meanwhile (helpers may take their time limit); callers that need the list wait for it.
fn ensure_scanned(fresh: bool) -> Vec<Vst3Descriptor> {
    let mut r = lock();
    if fresh {
        r.generation = r.generation.wrapping_add(1);
        r.scanned = None;
        r.failed.clear();
        r.skipped.clear();
    }
    loop {
        if let Some(list) = &r.scanned {
            return list.clone();
        }
        if !r.scanning {
            break;
        }
        r = scan_done().wait(r).unwrap_or_else(PoisonError::into_inner);
    }
    r.scanning = true;
    let generation = r.generation;
    let mut dirs = r.default_paths.clone().unwrap_or_else(paths::default_search_paths);
    dirs.extend(r.extra_dirs.iter().cloned());
    let helper = r.helper.clone();
    let mut skipped = r.skipped.clone();
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
    let found = scan_dirs(&dirs, helper.as_ref(), &mut skipped, fresh);
    let mut r = lock();
    // A rescan that began meanwhile wins: it scans again once this one has ended.
    if r.generation == generation {
        r.scanned = Some(found.clone());
        r.skipped = skipped;
    }
    found
}

pub fn scan() -> Vec<Vst3Descriptor> {
    ensure_scanned(false)
}

pub fn cached() -> Option<Vec<Vst3Descriptor>> {
    lock().scanned.clone()
}

pub fn rescan() -> Vec<Vst3Descriptor> {
    ensure_scanned(true)
}

/// Adds `dir` to the search folders and returns how many plugins it holds. Its plugins join the
/// scan when there is one; with none yet, the first scan covers it together with the standard
/// folders.
pub fn add_search_dir(dir: &Path) -> usize {
    let (helper, mut skipped) = {
        let mut r = lock();
        if !r.extra_dirs.iter().any(|d| d == dir) {
            r.extra_dirs.push(dir.to_path_buf());
        }
        (r.helper.clone(), r.skipped.clone())
    };
    let known = skipped.len();
    let found = scan_dirs(&[dir.to_path_buf()], helper.as_ref(), &mut skipped, false);
    let n = found.len();
    remember_skips(skipped, known);
    if let Some(list) = lock().scanned.as_mut() {
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
    lock().skipped.clone()
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
    let desc = ensure_scanned(false).into_iter().find(|d| d.id == id).ok_or_else(|| Vst3Error::NotFound(id.to_string()))?;
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
