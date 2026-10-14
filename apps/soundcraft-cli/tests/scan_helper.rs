//! `soundcraft-cli` as its own VST3 scan helper (`--soundcraft-scan-vst3 <bundle>`, internal), run
//! as the real binary. The helper protocol itself, with plugins that hang, crash or fail, is tested
//! in `soundcraft-vst3-host` (`tests/scan_helper.rs`).
// Integration tests: unwrapping and panicking on failure is fine here, unlike in shipped code (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_soundcraft-cli"));
    cmd.env("SOUNDCRAFT_NO_PREFS", "1");
    cmd
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-cli-scan-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An empty folder that stands in for the standard plugin folders, so no installed plugin is
/// scanned.
fn no_standard_folders(dir: &std::path::Path) -> PathBuf {
    let none = dir.join("no-standard-plugin-folders");
    std::fs::create_dir_all(&none).unwrap();
    none
}

#[test]
fn the_helper_greets_answers_and_prints_nothing_else() {
    let dir = tmpdir("answer");
    let bundle = dir.join("garbage.vst3");
    std::fs::write(&bundle, b"not a plugin").unwrap();
    let out = bin().arg("--soundcraft-scan-vst3").arg(&bundle).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "a bundle that does not load: {out:?}");
    // The protocol greeting, then the answer, and nothing else.
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = stdout
        .strip_prefix("soundcraft-vst3-scan-protocol: 1\n\nsoundcraft-vst3-scan: ")
        .and_then(|l| l.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("{stdout:?}"));
    let answer: serde_json::Value = serde_json::from_str(line).unwrap();
    let why = answer["error"].as_str().unwrap();
    assert!(why.contains("garbage.vst3"), "{why}");
    assert!(out.stderr.is_empty(), "no logging in helper mode: {}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_helper_flag_is_not_in_the_help() {
    let out = bin().arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("scan-vst3"));
}

/// The CLI's own scans go through helpers: a bundle that does not load is reported as skipped.
#[test]
fn the_cli_scans_vst3_plugins_in_helpers() {
    let dir = tmpdir("scan");
    let bundle = dir.join("garbage.vst3");
    std::fs::write(&bundle, b"not a plugin").unwrap();
    // No standard plugin folder: only this test's folder is scanned.
    let out = bin()
        .env("HOME", &dir)
        .env("SOUNDCRAFT_VST3_DEFAULT_PATHS", no_standard_folders(&dir))
        .env("VST3_PATH", &dir)
        .env_remove("RUST_LOG")
        .args(["run", "--cmd", r#"engine.vst3_plugins={"rescan":true}"#])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "engine.vst3_plugins: []", "no installed plugin scanned");
    let skipped = format!("vst3: skipped {}: cannot load VST3 plugin", bundle.display());
    assert!(stderr.contains(&skipped), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The CLI keeps its VST3 scan in its settings folder: the next run starts no helper for a bundle
/// it skipped (it says so once, at info level). Without preferences there is no cache.
#[test]
fn the_cli_caches_vst3_scans_in_its_settings_folder() {
    let dir = tmpdir("cache");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let bundle = plugins.join("garbage.vst3");
    std::fs::write(&bundle, b"not a plugin").unwrap();
    let config = dir.join("config");
    let cache = config.join("plugin-cache/vst3.json");
    let run = |no_prefs: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_soundcraft-cli"));
        cmd.env("HOME", &dir)
            .env("SOUNDCRAFT_VST3_DEFAULT_PATHS", no_standard_folders(&dir))
            .env("VST3_PATH", &plugins)
            .env("SOUNDCRAFT_CONFIG_DIR", &config)
            .env("RUST_LOG", "info")
            .env_remove("SOUNDCRAFT_NO_PREFS");
        if no_prefs {
            cmd.env("SOUNDCRAFT_NO_PREFS", "1");
        }
        let out = cmd.args(["run", "--cmd", "engine.vst3_plugins"]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let skipped = format!("vst3: skipped {}", bundle.display());
    let first = run(false);
    assert!(first.contains(&skipped), "{first}");
    assert!(std::fs::read_to_string(&cache).unwrap().contains("garbage.vst3"));
    let second = run(false);
    assert!(!second.contains(&skipped), "not tried again: {second}");
    assert!(second.contains("bundles skipped by an earlier scan (") && second.contains("garbage") && second.contains("rescan to retry"), "{second}");
    std::fs::remove_dir_all(&config).unwrap();
    let without = run(true);
    assert!(without.contains(&skipped), "{without}");
    assert!(!cache.exists(), "no cache without preferences");
    let _ = std::fs::remove_dir_all(&dir);
}
