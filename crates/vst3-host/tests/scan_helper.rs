//! Scanning in helper processes (`harness = false`). This test binary is its own scan helper, the
//! way the apps are: it answers `--soundcraft-scan-vst3 <bundle>` first thing in `main`. Copies of
//! the fixture misbehave in their module entry when a file next to their binary says so: one never
//! returns (like a PACE/iLok-protected plugin without its licence service), one crashes, one
//! fails. Each must be skipped with its reason while the good bundle's plugins still come back,
//! and the skipped ones are not tried again until `rescan`. A helper stuck in a plugin ends when
//! its host is gone (Unix). With a scan cache, a later scan (as in another run) starts helpers only
//! for new or changed bundles; each helper start is counted in `<bundle>.launches` (bundles under
//! this test's temp folder only).

mod common;

use soundcraft_vst3_host::{SCAN_HELPER_FLAG, ScanHelper, SkippedBundle};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
use std::time::{Duration, Instant};

/// Short, so the hanging bundle costs little; long enough for a debug build to load the fixture.
const TIMEOUT: Duration = Duration::from_secs(3);

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let [flag, bundle] = args.as_slice()
        && flag == SCAN_HELPER_FLAG
        && Path::new(bundle).starts_with(env!("CARGO_TARGET_TMPDIR"))
    {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(Path::new(bundle).with_extension("launches"));
        let _ = log.and_then(|mut f| writeln!(f, "launched"));
    }
    if let Some(code) = soundcraft_vst3_host::run_scan_helper(args) {
        std::process::exit(code);
    }
    // The short-lived host of `a_helper_ends_when_its_host_is_gone`: starts scanning a folder
    // whose bundle hangs, and exits in the middle of it.
    if let Some(dir) = std::env::args_os().nth(2).filter(|_| std::env::args().nth(1).as_deref() == Some("--scan-and-exit")) {
        let slow = ScanHelper { timeout: Duration::from_secs(60), ..helper() };
        std::thread::spawn(move || soundcraft_vst3_host::scan_paths_with(&[PathBuf::from(dir)], &slow));
        std::thread::sleep(Duration::from_secs(1));
        std::process::exit(0);
    }
    // A scan that hangs fails the test instead of hanging it.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("scan_helper: a scan hung");
        std::process::exit(1);
    });
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vst3-scan-helper-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    hanging_crashing_and_failing_bundles_are_skipped(&root.join("mixed"));
    the_scan_cache_starts_helpers_only_for_new_or_changed_bundles(&root.join("cached"), &root.join("cache"));
    skipped_bundles_are_remembered_until_rescan(&root.join("remembered"), &root.join("remembered-cache/vst3.json"));
    #[cfg(unix)]
    a_helper_ends_when_its_host_is_gone(&root.join("orphan"));
    let _ = std::fs::remove_dir_all(&root);
    println!("scan_helper: ok");
}

fn helper() -> ScanHelper {
    ScanHelper { timeout: TIMEOUT, ..ScanHelper::new(std::env::current_exe().expect("test binary")) }
}

/// Installs the fixture as `<dir>/<name>.vst3`, misbehaving as `mode` says. Returns the bundle
/// and the mode file.
fn install(dir: &Path, name: &str, mode: Option<&str>) -> (PathBuf, Option<PathBuf>) {
    let bin = common::install_fixture(dir, name);
    let mode = mode.map(|m| bin.join(m));
    if let Some(m) = &mode {
        std::fs::write(m, b"").unwrap();
    }
    (dir.join(format!("{name}.vst3")), mode)
}

/// The outcome the cache file holds for `bundle`.
fn cached_outcome(cache: &Path, bundle: &Path) -> serde_json::Value {
    let file: serde_json::Value = serde_json::from_slice(&std::fs::read(cache).unwrap()).unwrap();
    let entry = file["bundles"].as_array().unwrap().iter().find(|e| e["path"].as_str() == bundle.to_str()).cloned();
    entry.unwrap_or_else(|| panic!("{} not in the cache", bundle.display()))["outcome"].clone()
}

/// How many helpers were started for `bundle`.
fn launches(bundle: &Path) -> usize {
    std::fs::read_to_string(bundle.with_extension("launches")).map_or(0, |s| s.lines().count())
}

fn reason<'a>(skipped: &'a [SkippedBundle], bundle: &Path) -> &'a str {
    &skipped.iter().find(|s| s.path == bundle).unwrap_or_else(|| panic!("{} not skipped: {skipped:?}", bundle.display())).reason
}

fn hanging_crashing_and_failing_bundles_are_skipped(dir: &Path) {
    let (good, _) = install(dir, "Fixture", None);
    let (hang, _) = install(dir, "Hang", Some("hang-entry"));
    let (crash, _) = install(dir, "Crash", Some("crash-entry"));
    let (fail, _) = install(dir, "Fail", Some("fail-entry"));
    let started = Instant::now();
    let (found, skipped) = soundcraft_vst3_host::scan_paths_with(&[dir.to_path_buf()], &helper());
    let took = started.elapsed();
    // The good bundle's plugins, every field as an in-process load reads them (the JSON round trip).
    let mut direct = soundcraft_vst3_host::load_bundle(&good).unwrap();
    direct.sort_by_key(|d| d.name.to_lowercase());
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found, direct);
    assert!(found.iter().all(|d| d.path == good));
    assert_eq!(skipped.len(), 3, "{skipped:?}");
    let hung = reason(&skipped, &hang);
    assert!(hung.starts_with(&format!("timed out after {TIMEOUT:?}")), "{hung}");
    let crashed = reason(&skipped, &crash);
    if cfg!(unix) {
        assert_eq!(crashed, "the scan helper crashed (signal 9)");
    } else {
        assert!(crashed.contains("without an answer"), "{crashed}");
    }
    let failed = reason(&skipped, &fail);
    assert!(failed.contains("Fail.vst3") && failed.contains("failed"), "{failed}");
    // The helpers ran side by side: the hanging one was waited for once, not once per bundle.
    assert!(took >= TIMEOUT && took < TIMEOUT * 4, "{took:?}");
    assert!(soundcraft_vst3_host::skipped().is_empty(), "scan_paths_with leaves the registry alone");
}

fn the_scan_cache_starts_helpers_only_for_new_or_changed_bundles(dir: &Path, cache_dir: &Path) {
    let cache = cache_dir.join("vst3.json");
    let cached = ScanHelper { cache: Some(cache.clone()), ..helper() };
    let scan = || {
        let started = Instant::now();
        let (found, skipped) = soundcraft_vst3_host::scan_paths_with(&[dir.to_path_buf()], &cached);
        (found, skipped, started.elapsed())
    };
    // Only the cache file is ever left in its folder: it is written to a temporary file renamed
    // over it.
    let only_the_cache_file = || {
        let names: Vec<String> = std::fs::read_dir(cache_dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["vst3.json".to_string()]);
    };
    let (good, _) = install(dir, "Good", None);
    let (other, _) = install(dir, "Other", None);
    let (stuck, stuck_hangs) = install(dir, "Stuck", Some("hang-entry"));
    let (first, first_skipped, took) = scan();
    assert_eq!(first.len(), 2, "{first:?}");
    assert_eq!(first_skipped.iter().map(|s| &s.path).collect::<Vec<_>>(), vec![&stuck]);
    assert!(took >= TIMEOUT, "{took:?}");
    assert_eq!((launches(&good), launches(&other), launches(&stuck)), (1, 1, 1));
    only_the_cache_file();

    // Unchanged: no helper at all, the same plugins, and the stuck bundle still skipped with its
    // reason (even once it would load: its binary and bundle are unchanged).
    std::fs::remove_file(stuck_hangs.unwrap()).unwrap();
    let (second, second_skipped, took) = scan();
    assert_eq!((second, second_skipped), (first.clone(), first_skipped.clone()));
    assert_eq!((launches(&good), launches(&other), launches(&stuck)), (1, 1, 1), "no helper started");
    assert!(took < TIMEOUT, "{took:?}");

    // A changed binary (here only its modification time) is scanned again; the others are not.
    let binary = soundcraft_vst3_host::scan::bundle_binary(&good).unwrap();
    let modified = std::fs::metadata(&binary).unwrap().modified().unwrap() - Duration::from_secs(3600);
    std::fs::File::options().write(true).open(&binary).unwrap().set_modified(modified).unwrap();
    let (third, _, _) = scan();
    assert_eq!(third, first);
    assert_eq!((launches(&good), launches(&other), launches(&stuck)), (2, 1, 1));

    // A removed bundle leaves the cache.
    assert!(std::fs::read_to_string(&cache).unwrap().contains("Other.vst3"));
    std::fs::remove_dir_all(&other).unwrap();
    let (fourth, _, _) = scan();
    assert_eq!(fourth.len(), 2, "Good has the same plugins");
    assert!(!std::fs::read_to_string(&cache).unwrap().contains("Other.vst3"), "dropped from the cache");
    only_the_cache_file();

    // A corrupt cache is ignored (everything is scanned again; the stuck bundle loads now) and
    // replaced.
    std::fs::write(&cache, b"{\"version\": 1, \"bundles\": [ garbage").unwrap();
    let (fifth, fifth_skipped, _) = scan();
    assert_eq!(fifth.len(), 2);
    assert!(fifth_skipped.is_empty(), "{fifth_skipped:?}");
    assert_eq!((launches(&good), launches(&stuck)), (3, 2));
    let replaced: serde_json::Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    assert_eq!(replaced["version"], 1);
    assert_eq!(replaced["bundles"].as_array().map(Vec::len), Some(2), "{replaced}");
    only_the_cache_file();
}

fn skipped_bundles_are_remembered_until_rescan(dir: &Path, cache: &Path) {
    let (good, _) = install(dir, "Fixture", None);
    let (slow, hang_file) = install(dir, "Slow", Some("hang-entry"));
    let mine = || -> Vec<SkippedBundle> { soundcraft_vst3_host::skipped().into_iter().filter(|s| s.path.starts_with(dir)).collect() };
    soundcraft_vst3_host::set_scan_helper(Some(ScanHelper { cache: Some(cache.to_path_buf()), ..helper() }));
    let started = Instant::now();
    assert_eq!(soundcraft_vst3_host::add_search_dir(dir), 2, "the good bundle's plugins");
    assert!(started.elapsed() >= TIMEOUT, "the hanging bundle was waited for");
    assert_eq!(mine().iter().map(|s| &s.path).collect::<Vec<_>>(), vec![&slow]);
    assert!(soundcraft_vst3_host::scan().iter().any(|d| d.path == good));
    // Later scans leave it out without starting a helper for it.
    let started = Instant::now();
    assert_eq!(soundcraft_vst3_host::add_search_dir(dir), 2);
    assert_eq!(soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]).len(), 2);
    assert!(started.elapsed() < TIMEOUT, "{:?}: the skipped bundle was tried again", started.elapsed());
    assert_eq!(mine().len(), 1);
    assert_eq!((launches(&good), launches(&slow)), (1, 1));
    assert!(cached_outcome(cache, &slow)["skipped"].as_str().is_some_and(|r| r.starts_with("timed out")), "the skip is in the cache");
    // `rescan` tries every bundle again, the cache notwithstanding: the stuck one loads now that it
    // no longer hangs, and the cache is rewritten. (It scans the standard folders too, through
    // helpers.)
    std::fs::remove_file(hang_file.unwrap()).unwrap();
    soundcraft_vst3_host::rescan();
    assert!(mine().is_empty(), "retried: {:?}", mine());
    assert_eq!((launches(&good), launches(&slow)), (2, 2), "every bundle retried");
    assert!(cached_outcome(cache, &slow)["plugins"].is_array(), "the cache was rewritten");
    assert!(soundcraft_vst3_host::scan().iter().any(|d| d.path == good));
    soundcraft_vst3_host::set_scan_helper(None);
}

/// A host killed mid-scan must not leave its helper stuck in a plugin's module entry for good.
#[cfg(unix)]
fn a_helper_ends_when_its_host_is_gone(dir: &Path) {
    let (stuck, _) = install(dir, "Stuck", Some("hang-entry"));
    let helpers = || {
        let out = Command::new("pgrep").arg("-f").arg(stuck.as_os_str()).output().unwrap();
        String::from_utf8_lossy(&out.stdout).split_whitespace().map(str::to_string).collect::<Vec<_>>()
    };
    let host = Command::new(std::env::current_exe().unwrap()).arg("--scan-and-exit").arg(dir).output().unwrap();
    assert!(host.status.success(), "{host:?}");
    let started = Instant::now();
    while !helpers().is_empty() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let left = helpers();
    for pid in &left {
        let _ = Command::new("kill").args(["-9", pid]).status();
    }
    assert!(left.is_empty(), "helpers {left:?} outlived their host by {:?}", started.elapsed());
}
