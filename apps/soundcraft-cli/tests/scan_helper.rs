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

#[test]
fn the_helper_answers_one_line_and_nothing_else() {
    let dir = tmpdir("answer");
    let bundle = dir.join("garbage.vst3");
    std::fs::write(&bundle, b"not a plugin").unwrap();
    let out = bin().arg("--soundcraft-scan-vst3").arg(&bundle).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "a bundle that does not load: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = stdout.strip_prefix("\nsoundcraft-vst3-scan: ").and_then(|l| l.strip_suffix('\n')).unwrap_or_else(|| panic!("{stdout:?}"));
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
    // The home folder's plugins are left out; the system-wide folders are still scanned.
    let out = bin()
        .env("HOME", &dir)
        .env("VST3_PATH", &dir)
        .env_remove("RUST_LOG")
        .args(["run", "--cmd", r#"engine.vst3_plugins={"rescan":true}"#])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("engine.vst3_plugins: ["));
    let skipped = format!("vst3: skipped {}: cannot load VST3 plugin", bundle.display());
    assert!(stderr.contains(&skipped), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
