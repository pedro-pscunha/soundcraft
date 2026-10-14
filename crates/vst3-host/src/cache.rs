//! The persistent scan cache of out-of-process scans: per bundle, its plugins or why it was
//! skipped, under a fingerprint of the bundle (its binary's size and modification time and the
//! bundle's modification time), so a later run sends only new or changed bundles to helpers. A
//! bundle skipped once (a PACE plugin that never answers) then costs nothing until it changes or
//! an explicit rescan.
//!
//! The file is JSON (`{"version":1,"bundles":[…]}`), written whole to a temporary file next to it
//! and renamed over it. A missing, unreadable, corrupt or other-version file is logged and treated
//! as empty (then replaced): the cache never fails a scan.

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
    bundles: Vec<Entry>,
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

/// The cache file's contents, in memory while one scan runs.
pub(crate) struct ScanCache {
    file: PathBuf,
    entries: BTreeMap<PathBuf, (Fingerprint, Outcome)>,
    /// Differs from the file (or the file was unusable): written back by [`ScanCache::save`].
    changed: bool,
}

impl ScanCache {
    /// Reads the cache file; anything wrong with it is logged and the cache starts empty.
    pub(crate) fn load(file: &Path) -> ScanCache {
        let mut cache = ScanCache { file: file.to_path_buf(), entries: BTreeMap::new(), changed: false };
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
        self.entries.insert(bundle.to_path_buf(), (fingerprint, outcome));
        self.changed = true;
    }

    /// Forgets bundles that are no longer on disk.
    pub(crate) fn drop_gone(&mut self) {
        let before = self.entries.len();
        self.entries.retain(|path, _| path.exists());
        self.changed |= self.entries.len() != before;
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
        let bundles = self
            .entries
            .iter()
            // JSON strings are UTF-8: a bundle with another path is scanned every time instead.
            .filter(|(path, _)| path.to_str().is_some())
            .map(|(path, (fingerprint, outcome))| Entry { path: path.clone(), fingerprint: *fingerprint, outcome: outcome.clone() })
            .collect();
        let bytes = serde_json::to_vec(&CacheFile { version: VERSION, bundles }).map_err(|e| e.to_string())?;
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
        let json = serde_json::json!({"version": VERSION, "bundles": [{"path": a, "fingerprint": PRINT, "outcome": {"plugins": [elsewhere]}}]});
        std::fs::write(&file, json.to_string()).unwrap();
        assert_eq!(ScanCache::load(&file).lookup(&a, PRINT), Some(&Outcome::Plugins(vec![plugin(&a)])));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn malformed_entries_are_cleaned_on_reading() {
        let d = dir("malformed");
        let (file, a, b) = (d.join("vst3.json"), d.join("A.vst3"), d.join("B.vst3"));
        let other_host = Vst3Descriptor { id: "clap:com.x".into(), ..plugin(&a) };
        let json = serde_json::json!({"version": VERSION, "bundles": [
            {"path": a, "fingerprint": PRINT, "outcome": {"plugins": [other_host, plugin(&a)]}},
            {"path": b, "fingerprint": PRINT, "outcome": {"skipped": format!("x\n{}", "y".repeat(5000))}},
        ]});
        std::fs::write(&file, json.to_string()).unwrap();
        let c = ScanCache::load(&file);
        assert_eq!(c.lookup(&a, PRINT), Some(&Outcome::Plugins(vec![plugin(&a)])));
        let Some(Outcome::Skipped(why)) = c.lookup(&b, PRINT) else { panic!() };
        assert!(why.starts_with("x y") && why.chars().count() <= 300, "{why:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn other_versions_and_garbage_start_empty_and_are_replaced() {
        let d = dir("versions");
        let file = d.join("vst3.json");
        for bad in [serde_json::json!({"version": VERSION + 1, "bundles": []}).to_string(), "{not json".into(), String::new()] {
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
