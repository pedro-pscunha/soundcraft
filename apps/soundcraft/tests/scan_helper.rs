//! The desktop app as a VST3 scan helper (`--soundcraft-scan-vst3 <bundle>`, internal), run as the
//! real binary: it answers before logging, windows, the application object or audio exist, so it
//! writes nothing but its two protocol lines and creates nothing in the settings folder.
// Integration tests: unwrapping and panicking on failure is fine here, unlike in shipped code (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn the_app_answers_as_a_scan_helper_before_anything_else() {
    let dir = std::env::temp_dir().join(format!("sc-app-scan-helper-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("settings");
    std::fs::create_dir_all(&config).unwrap();
    let bundle = dir.join("garbage.vst3");
    std::fs::write(&bundle, b"not a plugin").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_soundcraft"))
        .arg("--soundcraft-scan-vst3")
        .arg(&bundle)
        .env("SOUNDCRAFT_CONFIG_DIR", &config)
        .env_remove("SOUNDCRAFT_NO_PREFS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A broken helper mode would be the app starting up: end it instead of waiting for it.
    let started = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("the app started instead of answering as a scan helper");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (mut stdout, mut stderr) = (String::new(), String::new());
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert_eq!(status.code(), Some(2), "a bundle that does not load");
    let line = stdout
        .strip_prefix("soundcraft-vst3-scan-protocol: 1\n\nsoundcraft-vst3-scan: ")
        .and_then(|l| l.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("{stdout:?}"));
    let answer: serde_json::Value = serde_json::from_str(line).unwrap();
    assert!(answer["error"].as_str().unwrap().contains("garbage.vst3"), "{answer}");
    assert!(stderr.is_empty(), "no logging in helper mode: {stderr}");
    let created: Vec<_> = std::fs::read_dir(&config).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(created.is_empty(), "nothing in the settings folder (no log directory): {created:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
