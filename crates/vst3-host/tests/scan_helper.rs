//! Scanning in helper processes (`harness = false`). This test binary is its own scan helper, the
//! way the apps are: it answers `--soundcraft-scan-vst3 <bundle>` first thing in `main`. Copies of
//! the fixture misbehave in their module entry when a file next to their binary says so: one never
//! returns (like a PACE/iLok-protected plugin without its licence service), one crashes, one
//! fails. Each must be skipped with its reason while the good bundle's plugins still come back,
//! and the skipped ones are not tried again until `rescan`. A helper stuck in a plugin ends when
//! its host is gone (Unix). With a scan cache, a later scan (as in another run) starts helpers only
//! for new or changed bundles; each helper start is logged, with its process id, in
//! `<bundle>.launches` (bundles under this test's temp folder only). The tests run in a child of
//! this binary with `SOUNDCRAFT_VST3_DEFAULT_PATHS` set to an empty folder, so no installed plugin
//! is ever scanned.

mod common;

use soundcraft_vst3_host::{SCAN_HELPER_FLAG, ScanHelper, SkippedBundle};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
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
        let _ = log.and_then(|mut f| writeln!(f, "{}", std::process::id()));
    }
    // A program that is no scan helper of this protocol, as an older SoundCraft answers.
    // A program that greets with nothing and never ends, as a helper stuck before it starts.
    if std::env::current_exe().is_ok_and(|e| e.file_stem().is_some_and(|n| n == "stalling-scan-helper")) {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    if std::env::current_exe().is_ok_and(|e| e.file_stem().is_some_and(|n| n == "not-a-scan-helper")) {
        eprintln!("soundcraft-cli: unknown subcommand `{}`", SCAN_HELPER_FLAG);
        std::process::exit(1);
    }
    if let Some(code) = soundcraft_vst3_host::run_scan_helper(args.clone()) {
        std::process::exit(code);
    }
    // The short-lived host of `a_helper_ends_when_its_host_is_gone`: starts scanning a folder
    // whose one bundle hangs, and exits as soon as its helper has started.
    if let [mode, bundle] = args.as_slice()
        && mode == "--scan-and-exit"
    {
        let (bundle, slow) = (PathBuf::from(bundle), ScanHelper { timeout: Duration::from_secs(60), ..helper() });
        let dir = bundle.parent().unwrap().to_path_buf();
        std::thread::spawn(move || soundcraft_vst3_host::scan_paths_with(&[dir], &slow));
        let started = Instant::now();
        while launches(&bundle) == 0 && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::process::exit(if launches(&bundle) == 0 { 1 } else { 0 });
    }
    // A fresh process for `adding_a_folder_first_keeps_the_standard_ones`: its standard folders hold
    // `standard`; `extra` is added before anything has scanned.
    // A fresh process for `a_folder_added_during_a_scan_is_kept`: a scan of the standard folders is
    // stuck on `slow` while `late` (one good bundle, one that crashes) is added.
    if let [mode, slow, late] = args.as_slice()
        && mode == "--add-during-scan"
    {
        let (slow, late) = (PathBuf::from(slow), PathBuf::from(late));
        soundcraft_vst3_host::set_scan_helper(Some(helper()));
        let first = std::thread::spawn(soundcraft_vst3_host::scan);
        let started = Instant::now();
        while launches(&slow) == 0 && started.elapsed() < TIMEOUT {
            std::thread::sleep(Duration::from_millis(10));
        }
        let held = soundcraft_vst3_host::add_search_dir(&late);
        first.join().unwrap();
        let found: Vec<PathBuf> = soundcraft_vst3_host::scan().into_iter().map(|d| d.path).collect();
        let skipped: Vec<PathBuf> = soundcraft_vst3_host::skipped().into_iter().map(|s| s.path).collect();
        let ok = held == 2 && found.len() == 2 && found.iter().all(|p| p.starts_with(&late)) && skipped.len() == 2 && skipped.contains(&slow);
        if !ok {
            eprintln!("add_search_dir: {held}; scan(): {found:?}; skipped(): {skipped:?}");
        }
        std::process::exit(if ok { 0 } else { 1 });
    }
    // A fresh process for `a_rescan_asked_for_during_a_scan_rescans`: a scan is stuck on `slow`,
    // a rescan is asked for, and the stuck scan's caller scans again as soon as it is done. Exits
    // 0 when `crashy`, cached as skipped, was scanned again (the rescan's scan was fresh).
    if let [mode, cache, crashy, slow] = args.as_slice()
        && mode == "--rescan-during-scan"
    {
        let (crashy, slow) = (PathBuf::from(crashy), PathBuf::from(slow));
        soundcraft_vst3_host::set_scan_helper(Some(ScanHelper { cache: Some(cache.into()), ..helper() }));
        let (crashed, slowed) = (launches(&crashy), launches(&slow));
        let first = std::thread::spawn(|| {
            soundcraft_vst3_host::scan();
            // Back at once, as another caller would be (automation, a project load).
            soundcraft_vst3_host::scan();
        });
        let started = Instant::now();
        while launches(&slow) == slowed && started.elapsed() < TIMEOUT {
            std::thread::sleep(Duration::from_millis(10));
        }
        let rescan = std::thread::spawn(soundcraft_vst3_host::rescan);
        rescan.join().unwrap();
        first.join().unwrap();
        let ok = launches(&crashy) > crashed && soundcraft_vst3_host::skipped().iter().any(|s| s.path == crashy && !s.from_cache);
        if !ok {
            eprintln!("the rescan was answered by a scan from the cache: {:?}", soundcraft_vst3_host::skipped());
        }
        std::process::exit(if ok { 0 } else { 1 });
    }
    // A fresh process for `failures_on_soundcrafts_side_load_only_bundles_known_to_load`: rescans
    // `dir` through `exe` with the scan cache `cache`, and prints what it found as JSON.
    if let [mode, exe, cache, dir] = args.as_slice()
        && mode == "--rescan-with"
    {
        let failing = ScanHelper { exe: exe.into(), cache: Some(cache.into()), parallel: 1, ..helper() };
        soundcraft_vst3_host::set_scan_helper(Some(failing));
        soundcraft_vst3_host::add_search_dir(Path::new(dir));
        let started = Instant::now();
        let plugins = soundcraft_vst3_host::rescan();
        let paths: Vec<&Path> = plugins.iter().map(|d| d.path.as_path()).collect();
        println!("{}", serde_json::json!({"plugins": paths, "skipped": soundcraft_vst3_host::skipped(), "secs": started.elapsed().as_secs_f64()}));
        std::process::exit(0);
    }
    if let [mode, extra, standard] = args.as_slice()
        && mode == "--add-dir-first"
    {
        // Through helpers, so the launch log shows which bundles were scanned (all fixture copies
        // share their ids, so the list alone cannot).
        soundcraft_vst3_host::set_scan_helper(Some(helper()));
        let held = soundcraft_vst3_host::add_search_dir(Path::new(extra));
        let all = soundcraft_vst3_host::scan();
        let ok = held == 2 && all.len() == 2 && launches(Path::new(standard)) == 1;
        if !ok {
            eprintln!("add_search_dir: {held}; scan(): {all:?}; standard scanned {} times", launches(Path::new(standard)));
        }
        std::process::exit(if ok { 0 } else { 1 });
    }
    // Hermetic: run the tests again in a child that sees no standard plugin folder.
    if std::env::var_os("SOUNDCRAFT_VST3_DEFAULT_PATHS").is_none() {
        let none = Path::new(env!("CARGO_TARGET_TMPDIR")).join("vst3-no-default-plugins");
        let _ = std::fs::remove_dir_all(&none);
        std::fs::create_dir_all(&none).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(&args)
            .env("SOUNDCRAFT_VST3_DEFAULT_PATHS", &none)
            .env_remove("VST3_PATH")
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(1));
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
    eight_stuck_helpers_cost_one_time_limit(&root.join("eight"));
    failures_on_soundcrafts_side_load_only_bundles_known_to_load(&root.join("known"), &root.join("lone"), &root.join("helpers"));
    failures_on_soundcrafts_side_are_no_plugin_skips(&root.join("host-failures"));
    plugin_output_cannot_spoil_or_hide_the_answer(&root.join("noisy"));
    an_answer_counts_when_the_module_exit_hangs(&root.join("hang-exit"));
    #[cfg(unix)]
    a_timeout_kills_what_the_helper_started(&root.join("with-child"));
    the_scan_cache_starts_helpers_only_for_new_or_changed_bundles(&root.join("cached"), &root.join("cache"));
    a_timeout_is_cached_only_when_it_happens_twice(&root.join("twice"), &root.join("twice-cache/vst3.json"));
    skipped_bundles_are_remembered_until_rescan(&root.join("remembered"), &root.join("remembered-cache/vst3.json"));
    the_registry_answers_while_helpers_scan(&root.join("busy"));
    a_rescan_during_a_scan_wins(&root.join("raced"));
    a_rescan_asked_for_during_a_scan_rescans(&root.join("pending"));
    a_folder_added_during_a_scan_is_kept(&root.join("during"));
    adding_a_folder_first_keeps_the_standard_ones(&root.join("add-first"));
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
    let hangs = [hang.clone(), install(dir, "Hang2", Some("hang-entry")).0, install(dir, "Hang3", Some("hang-entry")).0];
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
    assert_eq!(skipped.len(), 5, "{skipped:?}");
    for h in &hangs {
        let hung = reason(&skipped, h);
        assert!(hung.starts_with(&format!("timed out after {TIMEOUT:?}")), "{hung}");
    }
    let crashed = reason(&skipped, &crash);
    if cfg!(unix) {
        assert!(crashed.starts_with("the scan helper crashed (signal 9)"), "{crashed}");
    } else {
        assert!(crashed.contains("without an answer"), "{crashed}");
    }
    #[cfg(unix)]
    {
        // A real crash (SIGSEGV) ends the helper at once, with no crash report under its name.
        let segv = install(dir, "Segv", Some("segv-entry")).0;
        let (_, skipped) = soundcraft_vst3_host::scan_paths_with(std::slice::from_ref(&segv), &helper());
        assert!(reason(&skipped, &segv).starts_with("the scan helper crashed (a fatal signal in plugin code)"), "{skipped:?}");
    }
    let failed = reason(&skipped, &fail);
    assert!(failed.contains("Fail.vst3") && failed.contains("failed"), "{failed}");
    // The helpers ran side by side: the three hanging ones cost one time limit, not three.
    assert!(took >= TIMEOUT && took < TIMEOUT * 2, "{took:?}");
    assert!(soundcraft_vst3_host::skipped().is_empty(), "scan_paths_with leaves the registry alone");
}

/// Helpers stuck in a plugin are idle: eight of them, the default parallelism, cost one time limit.
fn eight_stuck_helpers_cost_one_time_limit(dir: &Path) {
    for i in 0..8 {
        install(dir, &format!("Stuck{i}"), Some("hang-entry"));
    }
    let started = Instant::now();
    let (found, skipped) = soundcraft_vst3_host::scan_paths_with(&[dir.to_path_buf()], &helper());
    let took = started.elapsed();
    assert!(found.is_empty() && skipped.len() == 8, "{skipped:?}");
    assert!(took >= TIMEOUT && took < TIMEOUT * 2, "{took:?}: not all eight ran at once");
    // The pool runs as many helpers as it is given, and no more: one at a time, three stuck
    // bundles cost three time limits.
    let three: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("Stuck{i}.vst3"))).collect();
    let short = Duration::from_secs(1);
    for (parallel, at_least, below) in [(1, short * 3, short * 5), (3, short, short * 2)] {
        let started = Instant::now();
        let (_, skipped) = soundcraft_vst3_host::scan_paths_with(&three, &ScanHelper { timeout: short, parallel, ..helper() });
        let took = started.elapsed();
        assert_eq!(skipped.len(), 3);
        assert!(took >= at_least && took < below, "{parallel} at a time took {took:?}");
    }
}

/// A plugin whose module exit never returns once the helper has answered: the answer counts, and
/// the helper is killed after a short grace, not at the end of its time limit.
fn an_answer_counts_when_the_module_exit_hangs(dir: &Path) {
    let (bundle, _) = install(dir, "HangExit", Some("hang-exit"));
    let generous = ScanHelper { timeout: Duration::from_secs(10), ..helper() };
    let started = Instant::now();
    let (found, skipped) = soundcraft_vst3_host::scan_paths_with(std::slice::from_ref(&bundle), &generous);
    let took = started.elapsed();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(found.len(), 2);
    assert!(took < Duration::from_secs(6), "{took:?}: the helper was waited for to the end of its time limit");
}

/// A plugin printing on standard output, even lines that look like answers or more than the host
/// reads of it, cannot spoil or hide the helper's answer (on Unix it prints into standard error).
fn plugin_output_cannot_spoil_or_hide_the_answer(dir: &Path) {
    let mut bundles = vec![install(dir, "Noisy", Some("noisy-stdout")).0];
    if cfg!(unix) {
        bundles.push(install(dir, "Chatty", Some("chatty-entry")).0);
    }
    for bundle in bundles {
        let started = Instant::now();
        let (found, skipped) = soundcraft_vst3_host::scan_paths_with(std::slice::from_ref(&bundle), &helper());
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|d| d.path == bundle));
        assert!(started.elapsed() < TIMEOUT, "{:?}", started.elapsed());
    }
}

/// A helper that cannot start, or a program that is no scan helper of this protocol (an older
/// SoundCraft after an update in place), says nothing about the plugins: no helper is started after
/// the first failure, nothing is skipped or cached, and the bundles are loaded in this process.
fn failures_on_soundcrafts_side_are_no_plugin_skips(dir: &Path) {
    let goods = [install(dir, "A", None).0, install(dir, "B", None).0, install(dir, "C", None).0];
    let impostor = dir.join(format!("not-a-scan-helper{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(std::env::current_exe().unwrap(), &impostor).unwrap();
    let cache = dir.join("cache/vst3.json");
    // Helpers started so far, in all: one impostor, then none that could start.
    for (exe, launched) in [(impostor, 1), (dir.join("no-such-helper"), 1)] {
        let failing = ScanHelper { exe, parallel: 1, cache: Some(cache.clone()), ..helper() };
        let (found, skipped) = soundcraft_vst3_host::scan_paths_with(&[dir.to_path_buf()], &failing);
        // Nothing is known to load (no cache yet), so nothing is loaded here.
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        assert!(skipped.iter().all(|s| s.helper_failed && s.reason.starts_with("not scanned: the scan helper failed")), "{skipped:?}");
        assert_eq!(goods.iter().map(|g| launches(g)).sum::<usize>(), launched, "no helper after the first failure");
        assert!(!cache.exists(), "nothing cached");
    }
}

/// Runs a rescan of `dir` through `exe` in a fresh process (see `--rescan-with`): its report, or a
/// failure when it does not end within 20 s (it hung, likely on a plugin loaded in-process).
fn rescan_in_child(exe: &Path, cache: &Path, dir: &Path) -> serde_json::Value {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--rescan-with")
        .arg(exe)
        .arg(cache)
        .arg(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("the rescan through {} hung", exe.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("no report from the rescan through {} ({:?}): {e}", exe.display(), out.status))
}

/// On a rescan whose helper fails on SoundCraft's side, only bundles the cache knows to load are
/// loaded in this process. A bundle cached as hanging or crashing, or one a stuck helper was
/// running, is listed as not scanned instead, and the rescan ends at once.
fn failures_on_soundcrafts_side_load_only_bundles_known_to_load(dir: &Path, lone: &Path, helpers: &Path) {
    let (good, _) = install(dir, "Good", None);
    let (hang, _) = install(dir, "Hang", Some("hang-entry"));
    let (crash, _) = install(dir, "Zcrash", Some("crash-entry"));
    let cache = helpers.join("cache/vst3.json");
    // Scanned twice with a working helper: Good is known to load, Hang and Zcrash are skipped.
    let working = ScanHelper { cache: Some(cache.clone()), ..helper() };
    for _ in 0..2 {
        soundcraft_vst3_host::scan_paths_with(&[dir.to_path_buf()], &working);
    }
    let copy = |name: &str| {
        let exe = helpers.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        exe
    };
    for exe in [helpers.join("no-such-helper"), copy("not-a-scan-helper")] {
        let report = rescan_in_child(&exe, &cache, dir);
        assert_eq!(report["plugins"], serde_json::json!([good, good]), "Good, loaded here: {report}");
        let skipped = report["skipped"].as_array().unwrap();
        for bundle in [&hang, &crash] {
            let s = skipped.iter().find(|s| s["path"] == bundle.to_str().unwrap()).unwrap_or_else(|| panic!("{report}"));
            assert_eq!(s["helper_failed"], true, "{report}");
            assert!(s["reason"].as_str().unwrap().starts_with("not scanned: the scan helper failed"), "{report}");
        }
        assert!(report["secs"].as_f64().unwrap() < 2.0, "{report}");
    }
    // A helper stuck before it greets may be running the bundle's code: even a bundle known to load
    // is then not loaded here.
    let (lone_bundle, _) = install(lone, "Lone", None);
    soundcraft_vst3_host::scan_paths_with(&[lone.to_path_buf()], &working);
    let report = rescan_in_child(&copy("stalling-scan-helper"), &cache, lone);
    assert_eq!(report["plugins"], serde_json::json!([]), "{report}");
    assert_eq!(report["skipped"][0]["path"], lone_bundle.to_str().unwrap(), "{report}");
    assert_eq!(report["skipped"][0]["helper_failed"], true, "{report}");
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
    // A bundle that fails to load is skipped at once and for good (timeouts: see
    // `a_timeout_is_cached_only_when_it_happens_twice`).
    let (stuck, stuck_fails) = install(dir, "Broken", Some("fail-entry"));
    let (first, first_skipped, _) = scan();
    assert_eq!(first.len(), 2, "{first:?}");
    assert_eq!(first_skipped.iter().map(|s| &s.path).collect::<Vec<_>>(), vec![&stuck]);
    assert_eq!((launches(&good), launches(&other), launches(&stuck)), (1, 1, 1));
    only_the_cache_file();

    // Unchanged: no helper at all, the same plugins, and the broken bundle still skipped with its
    // reason (even once it would load: its binary and bundle are unchanged).
    std::fs::remove_file(stuck_fails.unwrap()).unwrap();
    let (second, second_skipped, took) = scan();
    assert_eq!(second, first);
    assert!(!first_skipped[0].from_cache, "the first scan's own");
    assert_eq!(second_skipped, vec![SkippedBundle { from_cache: true, ..first_skipped[0].clone() }], "the same, from the cache");
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

    // A corrupt cache is ignored (everything is scanned again; the broken bundle loads now) and
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

/// A timeout is provisional (a slow start or a busy machine can cause one): the next scan tries the
/// bundle once more, and only a second timeout in a row is cached as a skip. A crash is cached at
/// once.
fn a_timeout_is_cached_only_when_it_happens_twice(dir: &Path, cache: &Path) {
    let (twice, _) = install(dir, "Twice", Some("hang-entry"));
    let (crashy, _) = install(dir, "Crashy", Some("crash-entry"));
    let cached = ScanHelper { cache: Some(cache.to_path_buf()), ..helper() };
    let run = || {
        let (_, skipped) = soundcraft_vst3_host::scan_paths_with(&[dir.to_path_buf()], &cached);
        let from_cache = |b: &Path| skipped.iter().find(|s| s.path == b).unwrap_or_else(|| panic!("{skipped:?}")).from_cache;
        (from_cache(&twice), from_cache(&crashy))
    };
    assert_eq!(run(), (false, false));
    assert_eq!((launches(&twice), launches(&crashy)), (1, 1));
    assert!(cached_outcome(cache, &twice)["timed_out_once"].is_string(), "only provisionally");
    assert!(cached_outcome(cache, &crashy)["skipped"].is_string(), "a crash, at once");
    assert_eq!(run(), (false, true), "the timeout is tried once more, the crash is not");
    assert_eq!((launches(&twice), launches(&crashy)), (2, 1));
    assert!(cached_outcome(cache, &twice)["skipped"].is_string(), "twice in a row: a skip");
    assert_eq!(run(), (true, true));
    assert_eq!((launches(&twice), launches(&crashy)), (2, 1), "neither is tried again");
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
    assert!(cached_outcome(cache, &slow)["timed_out_once"].as_str().is_some_and(|r| r.starts_with("timed out")), "the timeout is in the cache");
    // `rescan` tries every bundle again, the cache notwithstanding: the stuck one loads now that it
    // no longer hangs, and the cache is rewritten.
    std::fs::remove_file(hang_file.unwrap()).unwrap();
    soundcraft_vst3_host::rescan();
    assert!(mine().is_empty(), "retried: {:?}", mine());
    assert_eq!((launches(&good), launches(&slow)), (2, 2), "every bundle retried");
    assert!(cached_outcome(cache, &slow)["plugins"].is_array(), "the cache was rewritten");
    assert!(soundcraft_vst3_host::scan().iter().any(|d| d.path == good));
    let elsewhere: Vec<_> = soundcraft_vst3_host::scan().into_iter().filter(|d| !d.path.starts_with(env!("CARGO_TARGET_TMPDIR"))).collect();
    assert!(elsewhere.is_empty(), "the rescan scanned installed plugins: {elsewhere:?}");
    soundcraft_vst3_host::set_scan_helper(None);
}

/// Helpers may take their time limit: the registry answers other calls meanwhile.
fn the_registry_answers_while_helpers_scan(dir: &Path) {
    let (busy, hangs) = install(dir, "Busy", Some("hang-entry"));
    soundcraft_vst3_host::set_scan_helper(Some(helper()));
    let folder = dir.to_path_buf();
    let scanning = std::thread::spawn(move || soundcraft_vst3_host::add_search_dir(&folder));
    let started = Instant::now();
    while launches(&busy) == 0 && started.elapsed() < TIMEOUT {
        std::thread::sleep(Duration::from_millis(10));
    }
    let asked = Instant::now();
    let _ = (soundcraft_vst3_host::cached(), soundcraft_vst3_host::skipped());
    let waited = asked.elapsed();
    assert_eq!(scanning.join().unwrap(), 0);
    assert!(waited < Duration::from_millis(500), "{waited:?}: the registry was locked while a helper ran");
    // Later rescans in this process may load it.
    std::fs::remove_file(hangs.unwrap()).unwrap();
    soundcraft_vst3_host::set_scan_helper(None);
}

/// A rescan asked for while a scan runs is not answered with that scan's (older) result: it waits
/// for it to end, then scans again.
fn a_rescan_during_a_scan_wins(dir: &Path) {
    let (raced, _) = install(dir, "Raced", None);
    soundcraft_vst3_host::set_scan_helper(Some(helper()));
    assert_eq!(soundcraft_vst3_host::add_search_dir(dir), 2);
    let hang = common::install_fixture(dir, "Raced").join("hang-entry");
    std::fs::write(&hang, b"").unwrap();
    let before = launches(&raced);
    let first = std::thread::spawn(soundcraft_vst3_host::rescan);
    let started = Instant::now();
    while launches(&raced) == before && started.elapsed() < TIMEOUT {
        std::thread::sleep(Duration::from_millis(10));
    }
    // The first rescan is stuck on the bundle; it is fixed, and a second rescan is asked for.
    std::fs::remove_file(&hang).unwrap();
    soundcraft_vst3_host::rescan();
    first.join().unwrap();
    let skipped: Vec<_> = soundcraft_vst3_host::skipped().into_iter().filter(|s| s.path == raced).collect();
    assert!(skipped.is_empty(), "the first scan's result won: {skipped:?}");
    assert_eq!(launches(&raced), before + 2, "scanned again after the first scan");
    soundcraft_vst3_host::set_scan_helper(None);
}

/// A folder added while a scan of the standard folders runs keeps its plugins and its skipped
/// bundles once that scan ends (in a fresh process, see `--add-during-scan`).
fn a_folder_added_during_a_scan_is_kept(dir: &Path) {
    let (slow, _) = install(&dir.join("standard"), "Slow", Some("hang-entry"));
    let late = dir.join("late");
    install(&late, "Late", None);
    install(&late, "LateCrash", Some("crash-entry"));
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--add-during-scan")
        .arg(&slow)
        .arg(&late)
        .env("SOUNDCRAFT_VST3_DEFAULT_PATHS", dir.join("standard"))
        .output()
        .unwrap();
    assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
}

/// A rescan asked for while a scan runs is a rescan, whichever caller begins the next scan: with
/// another caller waiting too (in a fresh process, see `--rescan-during-scan`).
fn a_rescan_asked_for_during_a_scan_rescans(dir: &Path) {
    let (crashy, _) = install(dir, "Crashy", Some("crash-entry"));
    let (slow, _) = install(dir, "Slow", Some("hang-entry"));
    let cache = dir.join("cache/vst3.json");
    // Crashy is cached as skipped; Slow timed out once, so the next scan tries it again.
    soundcraft_vst3_host::scan_paths_with(&[dir.to_path_buf()], &ScanHelper { cache: Some(cache.clone()), ..helper() });
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--rescan-during-scan")
        .arg(&cache)
        .arg(&crashy)
        .arg(&slow)
        .env("SOUNDCRAFT_VST3_DEFAULT_PATHS", dir)
        .output()
        .unwrap();
    assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
}

/// A folder added before anything has scanned does not stand in for the standard folders: the
/// first scan covers both. (In a fresh process, whose standard folders hold a fixture.)
fn adding_a_folder_first_keeps_the_standard_ones(dir: &Path) {
    let (standard, _) = install(&dir.join("standard"), "Standard", None);
    let extra = dir.join("extra");
    install(&extra, "Extra", None);
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--add-dir-first")
        .arg(&extra)
        .arg(&standard)
        .env("SOUNDCRAFT_VST3_DEFAULT_PATHS", dir.join("standard"))
        .output()
        .unwrap();
    assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
}

/// A timeout kills the helper and whatever its plugin started: a child of the plugin that keeps
/// the helper's output open neither survives nor holds the scan up.
#[cfg(unix)]
fn a_timeout_kills_what_the_helper_started(dir: &Path) {
    let (bundle, mode) = install(dir, "WithChild", Some("hang-with-child"));
    let started = Instant::now();
    let (_, skipped) = soundcraft_vst3_host::scan_paths_with(std::slice::from_ref(&bundle), &helper());
    let took = started.elapsed();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    let pid = std::fs::read_to_string(mode.unwrap().with_file_name("child-pid")).unwrap();
    let alive = || Command::new("kill").args(["-0", pid.trim()]).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    let gone = Instant::now();
    while alive() && gone.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = alive();
    if survived {
        let _ = Command::new("kill").args(["-9", pid.trim()]).status();
    }
    assert!(!survived, "the plugin's child {} outlived the timeout", pid.trim());
    assert!(took < TIMEOUT + Duration::from_millis(1500), "{took:?}: its open output held the scan");
}

/// A host killed mid-scan must not leave its helper stuck in a plugin's module entry for good.
#[cfg(unix)]
fn a_helper_ends_when_its_host_is_gone(dir: &Path) {
    let (stuck, _) = install(dir, "Stuck", Some("hang-entry"));
    let host = Command::new(std::env::current_exe().unwrap()).arg("--scan-and-exit").arg(&stuck).output().unwrap();
    assert!(host.status.success(), "the helper never started: {host:?}");
    // The host waited for its helper to start, so this is a live helper, not one that never ran.
    let pid = std::fs::read_to_string(stuck.with_extension("launches")).unwrap().trim().to_string();
    let alive = || Command::new("kill").args(["-0", &pid]).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    let started = Instant::now();
    while alive() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let outlived = alive();
    if outlived {
        let _ = Command::new("kill").args(["-9", &pid]).status();
    }
    assert!(!outlived, "helper {pid} outlived its host by {:?}", started.elapsed());
}
