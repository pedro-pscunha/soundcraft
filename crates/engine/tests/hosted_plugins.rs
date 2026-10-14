//! Hosted plugins keep their settings in their own state, so the session stores only the
//! parameters set since (`Insert::params`). These run on every OS with the CLAP test plugin built
//! from `crates/clap-host/tests/fixture` (a gain whose param "7" is linear gain, 0..2, default 1,
//! and an instrument).

use serde_json::json;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_engine::Engine;
use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackId, TrackKind};
use soundcraft_time::Range;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

const GAIN: &str = "clap:org.soundcraft.test.gain";
const SYNTH: &str = "clap:org.soundcraft.test.synth";

fn install_fixture() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(install_fixture_now);
}

fn install_fixture_now() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/fixture/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clap-fixture-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .env_remove("RUSTFLAGS")
        .status()
        .unwrap();
    assert!(st.success());
    let lib = if cfg!(target_os = "macos") {
        "libclap_fixture.dylib"
    } else if cfg!(windows) {
        "clap_fixture.dll"
    } else {
        "libclap_fixture.so"
    };
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("engine-hosted-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target.join("debug").join(lib), dir.join("Fixture.clap")).unwrap();
    assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 2);
}

/// A mono track playing a constant 0.25.
fn dc_track(e: &mut Engine, source: u64) -> TrackId {
    let s = e.session_mut();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    s.pool.insert(SourceId(source), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 4800]] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "dc", SourceId(source), 0, 0, 4800));
    t
}

fn insert_of(e: &Engine, t: TrackId) -> soundcraft_model::Insert {
    e.session().track(t).unwrap().mixer.inserts[0].clone().unwrap()
}

#[test]
fn a_hosted_insert_or_instrument_starts_with_only_the_params_given() {
    install_fixture();
    let mut e = Engine::default();
    let t = dc_track(&mut e, 80);
    e.execute("mix.insert", &json!({"track": t.0, "slot": 0, "plugin": GAIN})).unwrap();
    assert!(insert_of(&e, t).params.is_empty(), "{:?}", insert_of(&e, t).params);
    e.execute("mix.insert", &json!({"track": t.0, "slot": 0, "plugin": GAIN, "params": {"7": 0.5}})).unwrap();
    assert_eq!(insert_of(&e, t).params.into_iter().collect::<Vec<_>>(), vec![("7".to_string(), 0.5)]);

    let inst = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    e.execute("mix.instrument", &json!({"track": inst.0, "plugin": SYNTH})).unwrap();
    let params = e.session().track(inst).and_then(|tr| tr.instrument.as_ref()).map(|i| i.params.clone()).unwrap();
    assert!(params.is_empty(), "{params:?}");
}

/// The mixer sends a lane's values only for parameters the insert stores. A hosted plugin stores
/// none until one is set, so a lane written on its own was accepted and never played.
#[test]
fn automation_on_a_hosted_parameter_plays() {
    install_fixture();
    let mut e = Engine::default();
    let t = dc_track(&mut e, 81);
    let at = |e: &Engine, i: usize| soundcraft_mix::render_range(e.session(), Range::new(0, 4800), 512)[0][i];
    let dry = at(&e, 2000);
    e.execute("mix.insert", &json!({"track": t.0, "slot": 0, "plugin": GAIN})).unwrap();
    e.execute("automation.set_point", &json!({"track": t.0, "param": "plugin:a:7", "at": 0, "value": 2.0})).unwrap();
    let wet = at(&e, 2000);
    assert!((wet - 2.0 * dry).abs() < 1e-4, "dry {dry} automated {wet}");

    // Outside a written range the lane returns to the parameter's value: the declared default
    // (gain 1), not 0 (silence).
    let t2 = dc_track(&mut e, 82);
    e.execute("mix.insert", &json!({"track": t2.0, "slot": 0, "plugin": GAIN})).unwrap();
    e.execute("automation.write_range", &json!({"track": t2.0, "param": "plugin:a:7", "start": 3000, "end": 4000, "value": 0.5})).unwrap();
    let mut solo = e.session().clone();
    solo.tracks.retain(|tr| tr.id == t2);
    let out = soundcraft_mix::render_range(&solo, Range::new(0, 4800), 512);
    assert!((out[0][1000] - dry).abs() < 1e-4, "before the range: {} vs dry {dry}", out[0][1000]);
    assert!((out[0][3500] - 0.5 * dry).abs() < 1e-4, "in the range: {} vs half {}", out[0][3500], 0.5 * dry);
}

/// `<factory default>` for a hosted plugin: no overrides and no state, so a new instance starts
/// at the plugin's own defaults. Sending every declared default instead overwrote its state.
#[test]
fn factory_settings_drop_a_hosted_plugins_overrides_and_state() {
    install_fixture();
    let mut e = Engine::default();
    let t = dc_track(&mut e, 83);
    e.execute("mix.insert", &json!({"track": t.0, "slot": 0, "plugin": GAIN, "params": {"7": 2.0}})).unwrap();
    e.execute("mix.insert_state", &json!({"track": t.0, "slot": 0, "state": soundcraft_model::b64::encode(b"settings")})).unwrap();
    let before = insert_of(&e, t);
    e.execute("mix.insert_params", &json!({"track": t.0, "slot": 0, "params": {}, "factory": true, "preset": "<factory default>"})).unwrap();
    let now = insert_of(&e, t);
    assert!(now.params.is_empty(), "{:?}", now.params);
    assert_eq!(now.state(), None);
    assert_ne!(now.state_epoch(), before.state_epoch(), "the live instance must be made again");
    assert_eq!(now.preset, "<factory default>");
    e.run("edit.undo").unwrap();
    assert_eq!(insert_of(&e, t), before);
}

/// Sample players load their samples on threads of their own after their state arrives, so an
/// offline render that made a hosted instrument, or a hosted plugin from a stored state, waits
/// before its first block. One without them, or with only a stateless hosted effect (nothing to
/// load), does not.
#[test]
fn offline_renders_wait_for_hosted_plugins_to_load() {
    install_fixture();
    let mut e = Engine::default();
    let t = dc_track(&mut e, 82);
    let range = Range::new(0, 4800);
    let loading = |e: &Engine| soundcraft_mix::RangeRender::new(e.session(), None, range, 512).loading();
    assert_eq!(loading(&e), None);
    e.execute("mix.insert", &json!({"track": t.0, "slot": 0, "plugin": GAIN})).unwrap();
    assert_eq!(loading(&e), None, "a stateless hosted effect has nothing to load");
    let inst = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    e.execute("mix.instrument", &json!({"track": inst.0, "plugin": SYNTH})).unwrap();
    assert!(loading(&e).is_some(), "a hosted instrument may load its sounds");
    e.execute("track.delete", &json!({"tracks": [inst.0]})).unwrap();
    assert_eq!(loading(&e), None);

    e.execute("mix.insert_state", &json!({"track": t.0, "slot": 0, "state": soundcraft_model::b64::encode(b"settings")})).unwrap();
    let mut r = soundcraft_mix::RangeRender::new(e.session(), None, range, 512);
    let left = r.loading().expect("waits for the hosted insert given a state");
    assert!(left <= soundcraft_mix::HOSTED_LOAD_WAIT && left > soundcraft_mix::HOSTED_LOAD_WAIT / 2, "{left:?}");
    let started = std::time::Instant::now();
    assert!(r.next_block(e.session()).is_some());
    assert!(started.elapsed() >= left.saturating_sub(std::time::Duration::from_millis(5)));
    assert_eq!(r.loading(), None);
}
