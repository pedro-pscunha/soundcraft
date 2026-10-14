//! The persistent scan cache of out-of-process scans: per bundle, its plugins or why it was
//! skipped, under a fingerprint of the bundle (its binary's size and modification time and the
//! bundle's modification time), so a later run sends only new or changed bundles to helpers. A
//! bundle skipped once (a PACE plugin that never answers) then costs nothing until it changes or
//! an explicit rescan.
//!
//! The file is JSON (`{"version":1,"scanner":"…","bundles":[…]}`), written whole to a temporary
//! file next to it and renamed over it. `scanner` names the code that made the outcomes
//! ([`scanner`]: SoundCraft's version, the helper protocol and the scanner revision): a file
//! written by other code starts cold, so an update's scanning fixes reach every bundle. A missing,
//! unreadable, corrupt, other-version or other-scanner file is logged and treated as empty (then
//! replaced): the cache never fails a scan.

use crate::Vst3Descriptor;
use crate::scan as paths;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The cache format; another version is ignored and replaced.
pub(crate) const VERSION: u32 = 1;
/// Largest cache file read (a bigger one is treated as corrupt).
const MAX_BYTES: u64 = 64 << 20;

/// What identifies a bundle's current build: any change means it is scanned again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Fingerprint {
    binary_size: u64,
    /// Nanoseconds since the Unix epoch (0 when unknown).
    binary_modified_ns: u64,
    bundle_modified_ns: u64,
}

/// What a helper made of a bundle.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Plugins(Vec<Vst3Descriptor>),
    Skipped(String),
    /// Its helper ran past its time limit once: scanned again next time, and skipped only if that
    /// times out too.
    TimedOutOnce(String),
    /// Its helper ran past its time limit twice in a row: skipped. A rescan tries it again, and
    /// another timeout keeps it here (it does not start over as a first one).
    TimedOut(String),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    path: PathBuf,
    fingerprint: Fingerprint,
    outcome: Outcome,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheFile {
    version: u32,
    /// The code that made the outcomes (see [`scanner`]).
    #[serde(default)]
    scanner: String,
    bundles: Vec<Entry>,
}

/// The code that makes scan outcomes: `<SoundCraft version>/<helper protocol>/<scanner revision>`.
pub(crate) fn scanner() -> String {
    format!("{}/{}/{}", env!("CARGO_PKG_VERSION"), crate::helper::PROTOCOL, crate::registry::SCANNER_REVISION)
}

/// The fingerprint of `bundle` now, `None` when its binary cannot be found or read.
pub(crate) fn fingerprint(bundle: &Path) -> Option<Fingerprint> {
    let binary = paths::bundle_binary(bundle)?;
    let md = std::fs::metadata(&binary).ok()?;
    let modified = |t: std::io::Result<SystemTime>| {
        t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    };
    Some(Fingerprint {
        binary_size: md.len(),
        binary_modified_ns: modified(md.modified()),
        bundle_modified_ns: modified(std::fs::metadata(bundle).and_then(|m| m.modified())),
    })
}

/// How old a leftover temporary file of the cache must be before it is deleted (a younger one
/// may be another process's write in progress).
const STALE_TEMPORARY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The cache file's contents, in memory while one scan runs.
pub(crate) struct ScanCache {
    file: PathBuf,
    entries: BTreeMap<PathBuf, (Fingerprint, Outcome)>,
    /// What this scan found, and the bundles it found gone: all that [`ScanCache::save`] writes over
    /// the file as it is then (another process may have written it meanwhile).
    recorded: BTreeMap<PathBuf, (Fingerprint, Outcome)>,
    gone: Vec<PathBuf>,
    /// Differs from the file (or the file was unusable): written back by [`ScanCache::save`].
    changed: bool,
}

impl ScanCache {
    /// Reads the cache file; anything wrong with it is logged and the cache starts empty.
    pub(crate) fn load(file: &Path) -> ScanCache {
        remove_stale_temporaries(file);
        let mut cache = ScanCache { file: file.to_path_buf(), entries: BTreeMap::new(), recorded: BTreeMap::new(), gone: Vec::new(), changed: false };
        match read(file) {
            Ok(Some(entries)) => cache.entries = entries,
            Ok(None) => {}
            Err(why) => {
                log::warn!("vst3: ignoring the scan cache {}: {why}; scanning again", file.display());
                cache.changed = true;
            }
        }
        cache
    }

    /// The outcome remembered for `bundle` when its fingerprint still matches.
    pub(crate) fn lookup(&self, bundle: &Path, fingerprint: Fingerprint) -> Option<&Outcome> {
        self.entries.get(bundle).filter(|(f, _)| *f == fingerprint).map(|(_, o)| o)
    }

    pub(crate) fn record(&mut self, bundle: &Path, fingerprint: Fingerprint, outcome: Outcome) {
        self.entries.insert(bundle.to_path_buf(), (fingerprint, outcome.clone()));
        self.recorded.insert(bundle.to_path_buf(), (fingerprint, outcome));
        self.changed = true;
    }

    /// Forgets bundles that are no longer on disk.
    pub(crate) fn drop_gone(&mut self) {
        let gone: Vec<PathBuf> = self.entries.keys().filter(|path| !path.exists()).cloned().collect();
        for path in &gone {
            self.entries.remove(path);
        }
        self.changed |= !gone.is_empty();
        self.gone.extend(gone);
    }

    /// Writes the cache back when it changed: whole, to a temporary file renamed over the old one.
    /// A failure is logged; the scan stands.
    pub(crate) fn save(&self) {
        if !self.changed {
            return;
        }
        if let Err(why) = self.write() {
            log::warn!("vst3: cannot write the scan cache {}: {why}", self.file.display());
        }
    }

    fn write(&self) -> Result<(), String> {
        // The file as it is now, with this scan's findings over it: what another process wrote
        // meanwhile (the app and the CLI share the file) stays.
        let mut entries = read(&self.file).ok().flatten().unwrap_or_default();
        for path in &self.gone {
            entries.remove(path);
        }
        entries.extend(self.recorded.iter().map(|(path, entry)| (path.clone(), entry.clone())));
        let bundles = entries
            .into_iter()
            // JSON strings are UTF-8: a bundle with another path (Linux only) is not cached, and so
            // scanned every time.
            .filter(|(path, _)| path.to_str().is_some())
            .map(|(path, (fingerprint, outcome))| Entry { path, fingerprint, outcome })
            .collect();
        let bytes = serde_json::to_vec(&CacheFile { version: VERSION, scanner: scanner(), bundles }).map_err(|e| e.to_string())?;
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
        let name = self.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "vst3.json".into());
        let temporary = self.file.with_file_name(format!("{name}.{}.{nonce}.tmp", std::process::id()));
        let written = (|| -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            std::fs::rename(&temporary, &self.file)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        written.map_err(|e| e.to_string())
    }
}

/// Deletes temporary files of `file` left by a write that never finished (a crash between writing
/// and renaming), once they are [`STALE_TEMPORARY`] old.
fn remove_stale_temporaries(file: &Path) {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name().and_then(|n| n.to_str())) else { return };
    let Ok(listing) = std::fs::read_dir(dir) else { return };
    let prefix = format!("{name}.");
    for entry in listing.flatten() {
        let stale = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age >= STALE_TEMPORARY);
        let ours = entry.file_name().to_str().is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".tmp"));
        if ours && stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The entries of a cache file: `None` when there is none yet, an error when it cannot be used.
fn read(file: &Path) -> Result<Option<BTreeMap<PathBuf, (Fingerprint, Outcome)>>, String> {
    let f = match std::fs::File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let mut bytes = Vec::new();
    f.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("larger than 64 MiB".into());
    }
    let parsed: CacheFile = serde_json::from_slice(&bytes).map_err(|e| format!("unreadable: {e}"))?;
    if parsed.version != VERSION {
        return Err(format!("format version {} (this build reads {VERSION})", parsed.version));
    }
    if parsed.scanner != scanner() {
        return Err(format!("written by scanner {:?} (this build is {:?})", parsed.scanner, scanner()));
    }
    Ok(Some(
        parsed
            .bundles
            .into_iter()
            .map(|e| {
                // The plugins belong to the bundle they are filed under, whatever the file says, and
                // must be well formed; a reason is cleaned like a helper's.
                let outcome = match e.outcome {
                    Outcome::Plugins(ds) => Outcome::Plugins(crate::helper::well_formed(ds, &e.path)),
                    Outcome::Skipped(why) => Outcome::Skipped(crate::helper::clean_reason(&why)),
                    Outcome::TimedOutOnce(why) => Outcome::TimedOutOnce(crate::helper::clean_reason(&why)),
                    Outcome::TimedOut(why) => Outcome::TimedOut(crate::helper::clean_reason(&why)),
                };
                (e.path, (e.fingerprint, outcome))
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_dsp::Category;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundcraft-vst3-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn plugin(path: &Path) -> Vst3Descriptor {
        Vst3Descriptor {
            id: "vst3:5C0A1D00111122223333444455556666".into(),
            class_id: "5C0A1D00111122223333444455556666".into(),
            name: "Gain".into(),
            vendor: "V".into(),
            version: "1".into(),
            sdk_version: "VST 3.8.0".into(),
            sub_categories: vec!["Fx".into()],
            category: Category::Other,
            is_instrument: false,
            path: path.to_path_buf(),
        }
    }

    const PRINT: Fingerprint = Fingerprint { binary_size: 7, binary_modified_ns: 1, bundle_modified_ns: 2 };

    #[test]
    fn entries_round_trip_and_match_only_their_fingerprint() {
        let d = dir("round-trip");
        let (file, a, b) = (d.join("cache/vst3.json"), d.join("A.vst3"), d.join("B.vst3"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut c = ScanCache::load(&file);
        c.record(&a, PRINT, Outcome::Plugins(vec![plugin(&a)]));
        c.record(&b, PRINT, Outcome::Skipped("timed out after 30s".into()));
        c.save();
        let c = ScanCache::load(&file);
        assert_eq!(c.lookup(&a, PRINT), Some(&Outcome::Plugins(vec![plugin(&a)])));
        assert_eq!(c.lookup(&b, PRINT), Some(&Outcome::Skipped("timed out after 30s".into())));
        assert_eq!(c.lookup(&a, Fingerprint { binary_modified_ns: 9, ..PRINT }), None);
        assert_eq!(c.lookup(&d.join("C.vst3"), PRINT), None);
        assert!(!c.changed, "a file read back as it was is not rewritten");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn plugins_belong_to_the_bundle_they_are_filed_under() {
        let d = dir("filed");
        let (file, a) = (d.join("vst3.json"), d.join("A.vst3"));
        let elsewhere = plugin(Path::new("/elsewhere/Evil.vst3"));
        let json = serde_json::json!({"version": VERSION, "scanner": scanner(), "bundles": [{"path": a, "fingerprint": PRINT, "outcome": {"plugins": [elsewhere]}}]});
        std::fs::write(&file, json.to_string()).unwrap();
        assert_eq!(ScanCache::load(&file).lookup(&a, PRINT), Some(&Outcome::Plugins(vec![plugin(&a)])));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn malformed_entries_are_cleaned_on_reading() {
        let d = dir("malformed");
        let (file, a, b) = (d.join("vst3.json"), d.join("A.vst3"), d.join("B.vst3"));
        let other_host = Vst3Descriptor { id: "clap:com.x".into(), ..plugin(&a) };
        let long = Vst3Descriptor { name: format!("Gain{}", "\n".repeat(5000)), ..plugin(&a) };
        let json = serde_json::json!({"version": VERSION, "scanner": scanner(), "bundles": [
            {"path": a, "fingerprint": PRINT, "outcome": {"plugins": [other_host, long]}},
            {"path": b, "fingerprint": PRINT, "outcome": {"skipped": format!("x\n{}", "y".repeat(5000))}},
        ]});
        std::fs::write(&file, json.to_string()).unwrap();
        let c = ScanCache::load(&file);
        let Some(Outcome::Plugins(ds)) = c.lookup(&a, PRINT) else { panic!() };
        assert_eq!(ds.len(), 1, "the other host's id is dropped");
        assert!(ds[0].name.starts_with("Gain ") && ds[0].name.chars().count() == 256 && !ds[0].name.contains('\n'), "{:?}", &ds[0].name[..8]);
        let Some(Outcome::Skipped(why)) = c.lookup(&b, PRINT) else { panic!() };
        assert!(why.starts_with("x y") && why.chars().count() <= 300, "{why:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_usual_file_name_names_the_format() {
        assert_eq!(crate::SCAN_CACHE_FILE, format!("vst3.v{VERSION}.json"));
    }

    #[test]
    fn two_scans_writing_one_file_keep_each_others_findings() {
        let d = dir("two-writers");
        let (file, a, b, gone) = (d.join("vst3.json"), d.join("A.vst3"), d.join("B.vst3"), d.join("Gone.vst3"));
        std::fs::create_dir_all(&gone).unwrap();
        let mut seed = ScanCache::load(&file);
        seed.record(&gone, PRINT, Outcome::Skipped("x".into()));
        seed.save();
        // The app and the CLI, each with the file as it was when it began.
        let (mut app, mut cli) = (ScanCache::load(&file), ScanCache::load(&file));
        app.record(&a, PRINT, Outcome::Plugins(vec![plugin(&a)]));
        cli.record(&b, PRINT, Outcome::Skipped("timed out".into()));
        std::fs::remove_dir_all(&gone).unwrap();
        cli.drop_gone();
        app.save();
        cli.save();
        let both = ScanCache::load(&file);
        assert!(both.lookup(&a, PRINT).is_some(), "the first writer's finding stays");
        assert!(both.lookup(&b, PRINT).is_some());
        assert!(both.lookup(&gone, PRINT).is_none(), "a bundle found gone is dropped");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn leftover_temporary_files_are_deleted_once_stale() {
        let d = dir("leftovers");
        let file = d.join("vst3.json");
        let (old, young, other) = (d.join("vst3.json.1.2.tmp"), d.join("vst3.json.3.4.tmp"), d.join("other.json.5.6.tmp"));
        for f in [&old, &young, &other] {
            std::fs::write(f, b"{").unwrap();
        }
        let long_ago = SystemTime::now() - std::time::Duration::from_secs(3600);
        for f in [&old, &other] {
            std::fs::File::options().write(true).open(f).unwrap().set_modified(long_ago).unwrap();
        }
        ScanCache::load(&file);
        assert!(!old.exists(), "a stale leftover of this cache is deleted");
        assert!(young.exists(), "a write in progress is left alone");
        assert!(other.exists(), "another file's leftovers are not this cache's");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn derived_fields_are_derived_again_on_reading() {
        let d = dir("derived");
        let (file, a) = (d.join("vst3.json"), d.join("A.vst3"));
        // Written by a build whose mapping got an instrument wrong.
        let stale = Vst3Descriptor {
            sub_categories: vec!["Instrument".into(), "Synth".into()],
            category: Category::Other,
            is_instrument: false,
            ..plugin(&a)
        };
        let json = serde_json::json!({"version": VERSION, "scanner": scanner(), "bundles": [{"path": a, "fingerprint": PRINT, "outcome": {"plugins": [stale]}}]});
        std::fs::write(&file, json.to_string()).unwrap();
        let Some(Outcome::Plugins(ds)) = ScanCache::load(&file).lookup(&a, PRINT).cloned() else { panic!() };
        assert_eq!((ds[0].category, ds[0].is_instrument), (Category::Instrument, true));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cache_made_by_other_scanning_code_starts_cold() {
        let d = dir("scanner");
        let (file, a) = (d.join("vst3.json"), d.join("A.vst3"));
        let mut c = ScanCache::load(&file);
        c.record(&a, PRINT, Outcome::Plugins(vec![plugin(&a)]));
        c.save();
        assert!(ScanCache::load(&file).lookup(&a, PRINT).is_some(), "the same scanner reads it");
        for other in [format!("0.0.0/{}/{}", crate::helper::PROTOCOL, crate::registry::SCANNER_REVISION), String::new()] {
            let mut saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            saved["scanner"] = other.clone().into();
            std::fs::write(&file, saved.to_string()).unwrap();
            let c = ScanCache::load(&file);
            assert!(c.lookup(&a, PRINT).is_none() && c.changed, "scanner {other:?}: cold, and replaced");
            c.save();
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn other_versions_and_garbage_start_empty_and_are_replaced() {
        let d = dir("versions");
        let file = d.join("vst3.json");
        for bad in [serde_json::json!({"version": VERSION + 1, "scanner": scanner(), "bundles": []}).to_string(), "{not json".into(), String::new()] {
            std::fs::write(&file, &bad).unwrap();
            let c = ScanCache::load(&file);
            assert!(c.entries.is_empty() && c.changed, "{bad:?}");
            c.save();
            let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(saved["version"], VERSION, "{bad:?} replaced");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cache_that_cannot_be_written_is_no_error() {
        let d = dir("unwritable");
        // Its parent is a file, so neither the folder nor the file can be made.
        std::fs::write(d.join("file"), b"").unwrap();
        let mut c = ScanCache::load(&d.join("file/vst3.json"));
        c.record(&d.join("A.vst3"), PRINT, Outcome::Skipped("x".into()));
        c.save();
        let _ = std::fs::remove_dir_all(&d);
    }
}
