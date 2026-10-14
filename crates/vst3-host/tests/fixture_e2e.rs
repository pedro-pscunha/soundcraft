//! End to end: build the test VST3 plugin in `tests/fixture` (a cdylib outside the workspace),
//! install it as a `.vst3` bundle in a temp folder, then scan → instantiate → process → params →
//! notes.

use soundcraft_dsp::{Category, RenderMode, Taper, Unit};
use std::sync::{Mutex, Once};

mod common;
use common::fixture_dir;

fn id_of(name: &str) -> String {
    let found = soundcraft_vst3_host::scan_paths(&[fixture_dir().to_path_buf()]);
    found.into_iter().find(|d| d.name == name).unwrap().id
}

#[test]
fn scan_lists_both_audio_modules_but_not_the_controller_class() {
    let dir = fixture_dir();
    let found = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]);
    assert_eq!(found.len(), 2, "{found:?}");
    let g = found.iter().find(|d| d.name == "Fixture Gain").unwrap();
    // `uid()` lays out the first 8 bytes differently on Windows (COM GUID order).
    let want = if cfg!(windows) { "vst3:001D0A5C111122223333444455556666" } else { "vst3:5C0A1D00111122223333444455556666" };
    assert_eq!(g.id, want);
    assert_eq!(g.vendor, "SoundCraft tests", "factory vendor fills an empty class vendor");
    assert_eq!(g.version, "1.2.3");
    assert_eq!(g.sdk_version, "VST 3.8.0");
    assert_eq!(g.sub_categories, vec!["Fx"]);
    assert_eq!(g.category, Category::Other);
    assert!(!g.is_instrument);
    assert!(g.path.ends_with("Fixture.vst3"));
    let s = found.iter().find(|d| d.name == "Fixture Synth").unwrap();
    assert_eq!(s.category, Category::Instrument);
    assert!(s.is_instrument);
    let global = soundcraft_vst3_host::scan();
    assert!(global.iter().any(|d| d.id == g.id) && global.iter().any(|d| d.id == s.id));
}

#[test]
fn info_exposes_visible_params_with_plain_ranges() {
    let gain = id_of("Fixture Gain");
    let info = soundcraft_vst3_host::plugin_info(&gain).unwrap();
    assert_eq!(info.id, gain);
    assert_eq!(info.short_name, "Fixture ");
    let ids: Vec<&str> = info.params.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec!["7", "3", "12"], "hidden 9 and bypass 11 are skipped");
    let g = info.param("7").unwrap();
    assert_eq!((g.name, g.min, g.max, g.default, g.unit, g.taper), ("Gain", 0.0, 2.0, 1.0, Unit::Db, Taper::Linear));
    let mode = info.param("3").unwrap();
    assert_eq!((mode.unit, mode.min, mode.max), (Unit::Choice, 0.0, 2.0));
    assert_eq!(mode.choices, &["A", "B", "C"]);
    let f = info.param("12").unwrap();
    assert_eq!((f.unit, f.taper), (Unit::Hz, Taper::Log));
    assert!((f.min - 20.0).abs() < 1e-3 && (f.max - 20_000.0).abs() < 1e-1, "{f:?}");
    // Cached: the same leaked info every time.
    assert!(std::ptr::eq(info, soundcraft_vst3_host::plugin_info(&gain).unwrap()));
}

#[test]
fn gain_processes_audio_and_follows_params() {
    let mut p = soundcraft_vst3_host::create(&id_of("Fixture Gain")).unwrap();
    p.prepare(44_100.0, 256, 2);
    assert_eq!(p.latency(), 3);
    // The controller was synced from the component state (gain 0.5), not the declared default.
    assert_eq!(p.param("7"), Some(0.5));
    let mut io = vec![vec![0.5f32; 256], vec![-0.25f32; 256]];
    p.process(&mut io, 256);
    assert!(io[0].iter().all(|&x| (x - 0.25).abs() < 1e-6), "{:?}", &io[0][..4]);
    assert!(p.set_param("7", 2.0));
    assert!(!p.set_param("9", 1.0), "hidden params are not exposed");
    assert!(!p.set_param("Gain", 1.0), "ids are VST3 ParamIDs");
    assert!(p.set_param("7", 5.0), "clamped");
    assert_eq!(p.param("7"), Some(2.0));
    assert!(p.set_param("12", 2000.0));
    assert!((p.param("12").unwrap() - 2000.0).abs() < 1e-2);
    assert!(p.set_param("3", 1.6));
    assert_eq!(p.param("3"), Some(2.0), "choices round");
    // A block larger than max_block is chunked.
    let mut io = vec![vec![0.5f32; 700], vec![-0.25f32; 700]];
    p.process(&mut io, 700);
    assert!(io[0].iter().all(|&x| (x - 1.0).abs() < 1e-6));
    assert!(io[1].iter().all(|&x| (x + 0.5).abs() < 1e-6));
    // Mono strip: the plugin accepts a mono arrangement and keeps the gain set above (prepare
    // re-sends nothing).
    p.prepare(48_000.0, 128, 1);
    let mut mono = vec![vec![0.25f32; 128]];
    p.process(&mut mono, 128);
    assert!(mono[0].iter().all(|&x| (x - 0.5).abs() < 1e-6), "{:?}", &mono[0][..4]);
    // Hostile shapes don't crash.
    p.process(&mut [], 64);
    let mut short = vec![vec![1.0f32; 4]];
    p.process(&mut short, 1_000_000);
    assert!(p.set_param("7", f32::NAN), "NaN becomes the default");
    assert_eq!(p.param("7"), Some(1.0));
    p.reset();
    p.prepare(f32::NAN, 0, 0);
    let mut io = vec![vec![0.1f32; 8]];
    p.process(&mut io, 8);
}

/// Offline renders reach the plugin as the `kOffline` process mode, in `setupProcessing` and in
/// every `process` call (the fixture checks they agree, and looks further ahead offline).
#[test]
fn offline_rendering_sets_the_offline_process_mode() {
    let mut p = soundcraft_vst3_host::create(&id_of("Fixture Gain")).unwrap();
    p.prepare(48_000.0, 64, 2);
    assert_eq!(p.latency(), 3, "real time by default");
    p.set_render_mode(RenderMode::Offline);
    assert_eq!(p.latency(), 3, "a change takes effect at the next prepare");
    p.prepare(48_000.0, 64, 2);
    assert_eq!(p.latency(), 5, "set up with kOffline");
    let mut io = vec![vec![0.5f32; 64]; 2];
    p.process(&mut io, 64);
    assert!(io[0].iter().all(|&x| (x - 0.25).abs() < 1e-6), "{:?}", &io[0][..4]);
    p.set_render_mode(RenderMode::Realtime);
    p.prepare(48_000.0, 64, 2);
    assert_eq!(p.latency(), 3, "back in real time");
    p.process(&mut io, 64);
}

/// Warnings this test binary logged (the host's `log` output).
fn warnings() -> Vec<String> {
    struct Warnings(Mutex<Vec<String>>);
    impl log::Log for Warnings {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Warn
        }
        fn log(&self, r: &log::Record) {
            if self.enabled(r.metadata()) {
                self.0.lock().unwrap().push(r.args().to_string());
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: Warnings = Warnings(Mutex::new(Vec::new()));
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Warn);
    });
    LOGGER.0.lock().unwrap().clone()
}

/// A plugin that refuses `kOffline` (the fixture gain in mode C) still renders in an offline
/// render, set up with `kRealtime` (the fixture checks every `process` call carries that mode);
/// the refusal is logged once per instance, however often it is prepared.
#[test]
fn a_plugin_that_refuses_the_offline_mode_renders_in_real_time() {
    let refusals = || warnings().iter().filter(|w| w.contains("refused the kOffline process mode")).count();
    let before = refusals();
    let mut p = soundcraft_vst3_host::create(&id_of("Fixture Gain")).unwrap();
    p.prepare(48_000.0, 64, 2);
    assert!(p.set_param("3", 2.0));
    let mut io = vec![vec![0.5f32; 64]; 2];
    p.process(&mut io, 64);
    p.set_render_mode(RenderMode::Offline);
    for _ in 0..3 {
        p.prepare(48_000.0, 64, 2);
        assert_eq!(p.latency(), 3, "set up with kRealtime");
        let mut io = vec![vec![0.5f32; 64]; 2];
        p.process(&mut io, 64);
        assert!(io[0].iter().all(|&x| (x - 0.25).abs() < 1e-6), "it still renders: {:?}", &io[0][..4]);
    }
    assert_eq!(refusals() - before, 1, "logged once: {:?}", warnings());
}

#[test]
fn synth_plays_notes_at_their_offsets() {
    let id = id_of("Fixture Synth");
    let info = soundcraft_vst3_host::plugin_info(&id).unwrap();
    assert!(info.is_instrument);
    assert_eq!(info.params.iter().map(|p| p.id).collect::<Vec<_>>(), ["2"], "Level only (1 is hidden)");
    let mut p = soundcraft_vst3_host::create(&id).unwrap();
    // `prepare` sends the processor nothing it did not ask for: the synth's controller reports
    // Level 0 while its processor plays at 1, and replaying the controller's values would mute it.
    p.prepare(48_000.0, 64, 2);
    let mut io = vec![vec![9.0f32; 64], vec![9.0f32; 64]];
    p.note_on(10, 60, 127);
    p.note_off(40, 60);
    p.process(&mut io, 64);
    for (i, &x) in io[0].iter().enumerate() {
        let want = if (10..40).contains(&i) { 1.0 } else { 0.0 };
        assert!((x - want).abs() < 1e-6, "frame {i}: {x}");
    }
    assert_eq!(io[0], io[1]);
    // Offsets past the block are clamped into it; all_notes_off releases held notes.
    p.note_on(1000, 61, 64);
    let mut io = vec![vec![0.0f32; 64], vec![0.0f32; 64]];
    p.process(&mut io, 64);
    assert!(io[0][62].abs() < 1e-6 && (io[0][63] - 64.0 / 127.0).abs() < 1e-6);
    p.all_notes_off();
    p.process(&mut io, 64);
    assert!(io[0].iter().all(|x| x.abs() < 1e-6));
    // MIDI CC 7 reaches the parameter the plugin maps it to, and poly pressure arrives as an event,
    // each at its offset; an unmapped controller is dropped.
    p.midi(10, [0xA0, 60, 32]);
    p.midi(20, [0xB0, 7, 64]);
    p.midi(30, [0xB0, 1, 127]);
    p.process(&mut io, 64);
    assert!(io[0][9].abs() < 1e-6 && (io[0][10] - 32.0 / 127.0).abs() < 1e-6);
    assert!((io[0][20] - 64.0 / 127.0).abs() < 1e-6 && (io[0][63] - 64.0 / 127.0).abs() < 1e-6);
    p.midi(0, [0xB0, 7, 0]);
    p.process(&mut io, 64);
    // Notes in a later sub-block of a chunked call land at the right frame.
    p.prepare(48_000.0, 32, 2);
    let mut io = vec![vec![0.0f32; 96], vec![0.0f32; 96]];
    p.note_on(50, 62, 127);
    p.process(&mut io, 96);
    assert!(io[0][49].abs() < 1e-6 && (io[0][50] - 1.0).abs() < 1e-6 && (io[0][95] - 1.0).abs() < 1e-6);
    // A mono strip downmixes the stereo output.
    p.prepare(48_000.0, 64, 1);
    let mut mono = vec![vec![0.0f32; 64]];
    p.process(&mut mono, 64);
    assert!(mono[0].iter().all(|&x| (x - 1.0).abs() < 1e-6), "the note is still held");
}

#[test]
fn many_instances_create_and_drop_cleanly() {
    let (g, s) = (id_of("Fixture Gain"), id_of("Fixture Synth"));
    for _ in 0..50 {
        let mut a = soundcraft_vst3_host::create(&g).unwrap();
        let b = soundcraft_vst3_host::create(&s).unwrap();
        let mut io = vec![vec![0.0f32; 32]; 2];
        a.process(&mut io, 32);
        drop(b);
    }
}

/// Smoke test against whatever real VST3 plugins are installed (none are required): scan, then
/// instantiate, prepare and process each one briefly. Set `SOUNDCRAFT_VST3_SMOKE=1` to run it.
#[test]
fn installed_plugins_smoke() {
    if std::env::var_os("SOUNDCRAFT_VST3_SMOKE").is_none() {
        return;
    }
    for d in soundcraft_vst3_host::scan() {
        eprintln!("found {} ({}) {:?} [{}] at {}", d.name, d.vendor, d.category, d.sub_categories.join("|"), d.path.display());
        let info = soundcraft_vst3_host::plugin_info(&d.id);
        eprintln!(
            "  info: {:?}",
            info.map(|i| (i.params.len(), i.params.iter().take(5).map(|p| (p.id, p.name, p.min, p.max, p.unit)).collect::<Vec<_>>()))
        );
        match soundcraft_vst3_host::instantiate(&d.id) {
            Ok(mut p) => {
                p.prepare(48_000.0, 512, 2);
                let mut io = vec![vec![0.1f32; 512], vec![0.1f32; 512]];
                p.process(&mut io, 512);
                eprintln!("  processed; latency {} out[0] {}", p.latency(), io[0][0]);
            }
            Err(e) => eprintln!("  instantiate: {e}"),
        }
    }
}

#[test]
fn state_saves_component_and_controller_and_restores_them() {
    let id = id_of("Fixture Gain");
    let mut p = soundcraft_vst3_host::create(&id).unwrap();
    p.prepare(48_000.0, 64, 2);
    assert!(p.set_param("7", 1.5));
    let mut io = vec![vec![0.5f32; 64]; 2];
    p.process(&mut io, 64);
    let blob = p.save_state().expect("component state");
    assert!(blob.starts_with(b"SCV3"));
    let mut q = soundcraft_vst3_host::create(&id).unwrap();
    q.prepare(48_000.0, 64, 2);
    assert_eq!(q.param("7"), Some(0.5));
    assert!(q.load_state(&blob));
    assert_eq!(q.param("7"), Some(1.5), "controller synced from the restored component state");
    let mut io = vec![vec![0.5f32; 64]; 2];
    q.process(&mut io, 64);
    assert!(io[0].iter().all(|&x| (x - 0.75).abs() < 1e-6), "{:?}", &io[0][..4]);
    // The controller part (Freq) round-trips too: write one by hand.
    let mut custom = b"SCV3".to_vec();
    custom.extend_from_slice(&8u32.to_le_bytes());
    custom.extend_from_slice(&0.25f64.to_le_bytes());
    custom.extend_from_slice(&8u32.to_le_bytes());
    custom.extend_from_slice(&1.0f64.to_le_bytes());
    assert!(q.load_state(&custom));
    assert_eq!(q.param("7"), Some(0.5));
    assert!((q.param("12").unwrap() - 20_000.0).abs() < 1.0, "{:?}", q.param("12"));
    // A bare component state is accepted; hostile blobs are refused.
    assert!(q.load_state(&0.5f64.to_le_bytes()));
    assert_eq!(q.param("7"), Some(1.0));
    for bad in [&b""[..], b"SCV3", b"SCV3\xff\xff\xff\xff", &[0xffu8; 8][..], b"abc"] {
        assert!(!q.load_state(bad), "{bad:?}");
    }
    assert_eq!(q.param("7"), Some(1.0));
}

#[test]
fn state_reader_reads_while_another_thread_processes() {
    let id = id_of("Fixture Gain");
    let mut p = soundcraft_vst3_host::create(&id).unwrap();
    p.prepare(48_000.0, 64, 2);
    assert!(p.set_param("7", 1.5));
    p.process(&mut vec![vec![0.5f32; 64]; 2], 64);
    let mut reader = p.state_reader().expect("every VST3 component has a state");
    let blob = p.save_state().unwrap();
    let audio = std::thread::spawn(move || {
        for _ in 0..2_000 {
            p.process(&mut vec![vec![0.5f32; 64]; 2], 64);
        }
        p
    });
    while !audio.is_finished() {
        assert_eq!(reader.save_state().as_ref(), Some(&blob));
    }
    let p = audio.join().unwrap();
    assert_eq!(reader.save_state(), Some(blob));
    drop(p);
    assert_eq!(reader.save_state(), None);
}

#[test]
fn host_set_controller_param_survives_state_round_trip() {
    // Freq lives only in the controller (the processor ignores it), so processing never carries
    // the host's value there: the state save has to.
    let id = id_of("Fixture Gain");
    let mut p = soundcraft_vst3_host::create(&id).unwrap();
    p.prepare(48_000.0, 64, 2);
    assert!(p.set_param("7", 1.5));
    assert!(p.set_param("12", 200.0));
    let mut p = std::thread::spawn(move || {
        let mut io = vec![vec![0.5f32; 64]; 2];
        p.process(&mut io, 64);
        assert!(io.iter().flatten().all(|&x| (x - 0.75).abs() < 1e-4));
        p
    })
    .join()
    .unwrap();
    assert!((p.param("12").unwrap() - 200.0).abs() < 0.01);
    let blob = p.save_state().expect("state");
    let mut q = soundcraft_vst3_host::create(&id).unwrap();
    assert!(q.load_state(&blob));
    assert!((q.param("7").unwrap() - 1.5).abs() < 1e-4, "{:?}", q.param("7"));
    assert!((q.param("12").unwrap() - 200.0).abs() < 0.01, "{:?}", q.param("12"));
    assert!(q.set_param("12", 300.0));
    assert!(q.load_state(&blob));
    let mut r = soundcraft_vst3_host::create(&id).unwrap();
    assert!(r.load_state(&q.save_state().unwrap()));
    assert!((r.param("12").unwrap() - 200.0).abs() < 0.01, "a loaded state replaces earlier edits: {:?}", r.param("12"));
}

/// A preset loaded in the plugin's own editor changes its values without telling the host per
/// parameter (`restartComponent(kParamValuesChanged)`). The state reader reads the controller's
/// values, in SoundCraft units, next to the ones the host last set, so a save can tell the
/// plugin's own changes from the session's overrides.
#[test]
fn state_reader_reads_the_values_the_plugin_set_itself() {
    use soundcraft_dsp::{ParamReading, Plugin};
    let mut p = soundcraft_vst3_host::instantiate_plugin(&id_of("Fixture Gain")).unwrap();
    assert!(p.set_param("7", 1.5));
    assert!(p.set_param("12", 200.0));
    let mut reader = p.state_reader().expect("every VST3 component has a state");
    let read = reader.param_values(&["7", "12", "3", "9", "nope"]);
    assert_eq!(read[0], Some(ParamReading { live: 1.5, host: 1.5 }), "the host's value reached the controller");
    let freq = read[1].unwrap();
    assert!((freq.live - 200.0).abs() < 0.01 && freq.host == 200.0, "{freq:?}");
    assert_eq!(read[2], Some(ParamReading { live: 0.0, host: 0.0 }), "never set: as read when it was made");
    assert_eq!(read[3..], [None, None], "hidden and unknown ids");
    // The "preset": Gain 0.25 normalized (0.5 plain) in the component, Freq 1.0 (20 kHz) in the
    // controller.
    let mut preset = b"SCV3".to_vec();
    preset.extend_from_slice(&8u32.to_le_bytes());
    preset.extend_from_slice(&0.25f64.to_le_bytes());
    preset.extend_from_slice(&8u32.to_le_bytes());
    preset.extend_from_slice(&1.0f64.to_le_bytes());
    assert!(p.load_state_unseen(&preset));
    let read = reader.param_values(&["7", "12"]);
    assert_eq!(read[0], Some(ParamReading { live: 0.5, host: 1.5 }));
    let freq = read[1].unwrap();
    assert!((freq.live - 20_000.0).abs() < 1.0 && freq.host == 200.0, "{freq:?}");
    assert!(p.set_param("7", 2.0));
    assert_eq!(reader.param_values(&["7"]), vec![Some(ParamReading { live: 2.0, host: 2.0 })]);
    drop(p);
    assert_eq!(reader.param_values(&["7"]), vec![None], "the instance is gone");
}

/// A Fixture Gain state ("preset"): Gain `gain` normalized in the component, Freq `freq`
/// normalized in the controller.
fn gain_preset(gain: f64, freq: f64) -> Vec<u8> {
    let mut preset = b"SCV3".to_vec();
    preset.extend_from_slice(&8u32.to_le_bytes());
    preset.extend_from_slice(&gain.to_le_bytes());
    preset.extend_from_slice(&8u32.to_le_bytes());
    preset.extend_from_slice(&freq.to_le_bytes());
    preset
}

/// The session's overrides reach the controller on the next UI frame (the state reader's
/// `idle`), as VST3 hosts hand the values they set to the controller on the UI thread. A preset
/// the plugin loads in its own editor afterwards, which it announces only as "values changed", is
/// then read whole at the next save: before, the overrides waited for the save and were handed
/// over on top of the preset, setting the editor's knobs back and hiding the preset from the save.
#[test]
fn a_preset_loaded_after_the_overrides_reached_the_controller_is_read_whole() {
    use soundcraft_dsp::{ParamReading, Plugin};
    let mut p = soundcraft_vst3_host::instantiate_plugin(&id_of("Fixture Gain")).unwrap();
    let mut reader = p.state_reader().expect("every VST3 component has a state");
    // The mixer sends the session's overrides to a new instance and renders.
    assert!(p.set_param("7", 1.5));
    assert!(p.set_param("12", 200.0));
    p.process(&mut vec![vec![0.5f32; 64]; 2], 64);
    // One UI frame.
    reader.idle();
    assert!(p.load_state_unseen(&gain_preset(0.25, 1.0)));
    let read = reader.param_values(&["7", "12"]);
    assert_eq!(read[0], Some(ParamReading { live: 0.5, host: 1.5 }), "the preset's gain");
    let freq = read[1].unwrap();
    assert!((freq.live - 20_000.0).abs() < 1.0 && freq.host == 200.0, "the preset's frequency: {freq:?}");
}

/// An edit made in the plugin's editor comes back from the session as a host set a frame or an
/// audio block later. That echo is not handed to the controller: by then a drag may have moved
/// the knob on (it would jump back), or a preset been loaded (it would be set back to the edit).
/// A value of the host's own is handed over.
#[test]
fn the_editors_own_edits_coming_back_are_not_handed_to_its_controller() {
    use soundcraft_dsp::{ParamReading, Plugin};
    let mut p = soundcraft_vst3_host::instantiate_plugin(&id_of("Fixture Gain")).unwrap();
    let mut ed = Plugin::editor(&mut p).expect("editor handle");
    let mut reader = p.state_reader().expect("every VST3 component has a state");
    // The fixture "edits" Gain to 0.9 (normalized) when it gets the component handler.
    assert_eq!(ed.idle(), vec![("7".to_string(), 1.8)]);
    // Then loads a preset (Gain 0.25 normalized, 0.5 plain).
    assert!(p.load_state_unseen(&gain_preset(0.25, 1.0)));
    // The session sends the edit back.
    assert!(p.set_param("7", 1.8));
    reader.idle();
    assert_eq!(reader.param_values(&["7"]), vec![Some(ParamReading { live: 0.5, host: 1.8 })], "the preset stays");
    assert!(p.set_param("7", 1.0));
    reader.idle();
    assert_eq!(reader.param_values(&["7"]), vec![Some(ParamReading { live: 1.0, host: 1.0 })], "the host's own value");
}

/// Some controllers report a value the host sets (`setParamNormalized`) back as an edit
/// (`performEdit`), as the fixture does for Freq. The state reader hands the session's values to
/// the controller every UI frame, automation included: that is not the user editing, and the
/// editor reports no edit for it (the session would take it as one: modified, an undo step, a new
/// base value). An edit made afterwards is reported.
#[test]
fn a_host_set_the_controller_reports_back_is_no_edit() {
    use soundcraft_dsp::Plugin;
    let mut p = soundcraft_vst3_host::instantiate_plugin(&id_of("Fixture Gain")).unwrap();
    let mut ed = Plugin::editor(&mut p).expect("editor handle");
    let mut reader = p.state_reader().expect("every VST3 component has a state");
    assert_eq!(ed.idle(), vec![("7".to_string(), 1.8)], "the fixture's setup edit");
    // Automation moves Freq; the next UI frame hands it to the controller.
    assert!(p.set_param("12", 200.0));
    reader.idle();
    assert_eq!(ed.idle(), vec![], "the host's own value is no edit");
    p.edit_as_view(12, 0.0);
    assert_eq!(ed.idle(), vec![("12".to_string(), 20.0)], "the user's edit after it is");
}

#[test]
fn editor_view_is_probed_and_edits_come_back_as_plain_values() {
    let id = id_of("Fixture Gain");
    let mut p = soundcraft_vst3_host::instantiate_plugin(&id).unwrap();
    // Editor windows are hosted on macOS and Windows; elsewhere the probe reports "unsupported".
    if cfg!(any(target_os = "macos", windows)) {
        assert_eq!(p.editor_size(), Ok((320, 240)));
    } else {
        assert!(p.editor_size().is_err());
    }
    let mut ed = soundcraft_dsp::Plugin::editor(&mut p).expect("editor handle");
    // The fixture "edits" Gain to 0.9 (normalized) when it gets the component handler.
    assert_eq!(ed.idle(), vec![("7".to_string(), 1.8)]);
    assert!(ed.idle().is_empty());
    if cfg!(any(target_os = "macos", windows)) {
        assert!(ed.open().unwrap_err().contains("main thread"), "test threads are not the main thread");
    } else {
        assert!(ed.open().is_err(), "unsupported elsewhere");
    }
    assert!(!ed.is_open());
    // The synth's controller has no view.
    let synth = soundcraft_vst3_host::instantiate_plugin(&id_of("Fixture Synth")).unwrap();
    assert!(synth.editor_size().is_err());
}
