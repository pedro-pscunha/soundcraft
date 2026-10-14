//! Shared by the integration tests: builds the fixture plugin (a cdylib outside the workspace)
//! and installs it as a `.vst3` bundle in a temp folder.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Builds the fixture once and returns the built library.
pub fn fixture_library() -> &'static Path {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("vst3-fixture-target");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let st = Command::new(cargo)
            .args(["build", "--quiet", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .status()
            .expect("run cargo");
        assert!(st.success(), "building the VST3 fixture failed");
        let lib = if cfg!(target_os = "macos") {
            "libvst3_fixture.dylib"
        } else if cfg!(windows) {
            "vst3_fixture.dll"
        } else {
            "libvst3_fixture.so"
        };
        target.join("debug").join(lib)
    })
}

/// Installs the fixture as the bundle `<dir>/<name>.vst3` (a real bundle, to exercise bundle
/// resolution and the module entry). Returns the folder holding its binary.
pub fn install_fixture(dir: &Path, name: &str) -> PathBuf {
    let (folder, exe) = if cfg!(target_os = "macos") {
        ("MacOS", name.to_string())
    } else if cfg!(windows) {
        ("x86_64-win", format!("{name}.vst3"))
    } else {
        (if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, format!("{name}.so"))
    };
    let bin = dir.join(format!("{name}.vst3/Contents")).join(folder);
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(fixture_library(), bin.join(exe)).unwrap();
    bin
}

/// Builds the fixture once and returns a folder containing `Fixture.vst3`.
pub fn fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vst3-fixture-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        install_fixture(&dir.join("vendor"), "Fixture");
        assert_eq!(soundcraft_vst3_host::add_search_dir(&dir), 2);
        dir
    })
}
