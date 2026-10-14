//! A click on a hosted plugin's slot runs `window.plugin`, which opens the plugin's own editor and
//! closes it on the next click. When the editor can't open, the status line says why; nothing opens
//! for a plugin SoundCraft knows nothing of (here an id no plugin has). A player without an audio
//! device holds stand-ins for the plugins' own editors.

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};
use soundcraft_model::{Insert, Track, TrackId};
use soundcraft_playback::{Player, StandInEditor};
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};

/// A VST3 id no plugin has: never looked up on disk, so SoundCraft knows nothing of it.
const HOSTED: &str = "vst3:not-a-class-id";
const UNKNOWN: &str = "vst3:not-a-class-id is not installed or could not be loaded.";
const C: usize = 2;
const INSTRUMENT: usize = soundcraft_mix::INSTRUMENT_SLOT;

/// One `kind` track, Vox, that `place` fills, with a player that opens no device when `audio`.
fn app(kind: &str, audio: bool, place: impl FnOnce(&mut Track)) -> (SoundApp, TrackId) {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.run("track.new", json!({"kind": kind, "name": "Vox"})).unwrap();
    let id = app.engine.session().track_by_name("Vox").unwrap().id;
    place(app.engine.session_mut().track_mut(id).unwrap());
    if audio {
        app.player = Some(Player::without_device(app.engine.session_arc()));
    }
    (app, id)
}

/// A stand-in for the own editor of the plugin in Vox's `slot`.
fn adopt(app: &SoundApp, id: TrackId, slot: usize, editor: &StandInEditor) {
    let adopted = app.player.as_ref().is_some_and(|p| p.adopt_editor(app.engine.session(), id, slot, Box::new(editor.clone())));
    assert!(adopted, "a hosted plugin in slot {slot}");
}

/// The Mix window, with the INSTRUMENT row.
fn mix(app: SoundApp) -> Harness<'static, SoundApp> {
    let mut app = app;
    app.ui.window = MainWindow::Mix;
    app.run("view.mix_section", json!({"section": "instruments"})).unwrap();
    let mut h = Harness::builder().with_size(vec2(1400.0, 1000.0)).with_step_dt(1.0 / 60.0).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    h
}

fn click(h: &mut Harness<'static, SoundApp>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(2);
}

#[test]
fn a_click_opens_a_hosted_plugins_editor_and_the_next_click_closes_it() {
    for (kind, slot, label) in [("audio", C, "Insert C"), ("instrument", INSTRUMENT, "Instrument")] {
        let (app, id) = app(kind, true, |t| {
            if slot == INSTRUMENT {
                t.instrument = Some(Insert::new(HOSTED));
            } else {
                t.mixer.inserts[slot] = Some(Insert::new(HOSTED));
            }
        });
        let editor = StandInEditor::default();
        adopt(&app, id, slot, &editor);
        let mut h = mix(app);
        click(&mut h, label);
        assert_eq!((editor.calls(), editor.shown()), (vec!["open"], true), "{label}");
        click(&mut h, label);
        assert_eq!((editor.calls(), editor.shown()), (vec!["open", "close"], false), "{label}");
        assert!(h.state().ui.plugin_windows.is_empty(), "{label}");
        assert!(h.state().ui.status.is_empty(), "{label}: {}", h.state().ui.status);
    }
}

/// macOS hides plugin editors while SoundCraft is in the background and shows them again when it
/// comes forward: the click that brings it forward must not close the editor it brought back.
#[test]
fn the_click_that_brings_soundcraft_forward_keeps_the_editor_open() {
    let (app, id) = app("audio", true, |t| t.mixer.inserts[C] = Some(Insert::new(HOSTED)));
    let editor = StandInEditor::default();
    adopt(&app, id, C, &editor);
    let mut h = mix(app);
    click(&mut h, "Insert C");
    assert_eq!((editor.calls(), editor.shown()), (vec!["open"], true));
    // SoundCraft comes forward with a click on the slot: the editor is raised, not closed.
    h.event(egui::Event::WindowFocused(false));
    h.run_steps(30);
    h.event(egui::Event::WindowFocused(true));
    click(&mut h, "Insert C");
    assert_eq!((editor.calls(), editor.shown()), (vec!["open", "open"], true));
    // A click a moment later closes it, as ever.
    h.run_steps(30);
    click(&mut h, "Insert C");
    assert_eq!((editor.calls(), editor.shown()), (vec!["open", "open", "close"], false));
}

#[test]
fn an_editor_that_fails_to_open_says_why() {
    const WHY: &str = "vst3:not-a-class-id: its view could not be made";
    let (mut app, id) = app("audio", true, |t| t.mixer.inserts[C] = Some(Insert::new(HOSTED)));
    let editor = StandInEditor::refusing("its view could not be made");
    adopt(&app, id, C, &editor);
    // SoundCraft knows none of its parameters: no parameter window to fall back on. (Drawing the
    // floating windows drops one it can't draw, so look before any frame.)
    assert_eq!(app.run("window.plugin", json!({"track": "Vox", "slot": C})), Err(WHY.into()));
    assert!(app.ui.plugin_windows.is_empty());
    app.ui.status.clear();
    let mut h = mix(app);
    click(&mut h, "Insert C");
    assert_eq!(editor.calls(), vec!["open", "open"]);
    assert_eq!(h.state().ui.status, WHY);
}

#[test]
fn a_hosted_plugin_soundcraft_knows_nothing_of_opens_nothing_and_says_so() {
    for audio in [false, true] {
        for (kind, label) in [("audio", "Insert C"), ("instrument", "Instrument")] {
            let (mut app, _) = app(kind, audio, |t| match kind {
                "audio" => t.mixer.inserts[C] = Some(Insert::new(HOSTED)),
                _ => t.instrument = Some(Insert::new(HOSTED)),
            });
            let slot = if kind == "audio" { json!(C) } else { json!("instrument") };
            for view in ["editor", "parameters"] {
                let r = app.run("window.plugin", json!({"track": "Vox", "slot": slot, "view": view, "value": true}));
                assert_eq!(r, Err(UNKNOWN.into()), "{label} {view}, audio {audio}");
                assert!(app.ui.plugin_windows.is_empty(), "{label} {view}, audio {audio}: a window with nothing to show");
            }
            app.ui.status.clear();
            let mut h = mix(app);
            assert!(h.state().ui.status.is_empty());
            click(&mut h, label);
            assert_eq!(h.state().ui.status, UNKNOWN, "{label}, audio {audio}");
            assert!(h.state().ui.plugin_windows.is_empty(), "{label}, audio {audio}: a window with nothing to show");
        }
    }
}

#[test]
fn a_built_in_insert_opens_its_parameters_as_before() {
    let (app, vox) = app("audio", true, |t| t.mixer.inserts[1] = Some(Insert::new("eq_7band")));
    let mut h = mix(app);
    for _ in 0..2 {
        click(&mut h, "Insert B");
        assert_eq!(h.state().ui.plugin_windows, vec![(vox, 1)], "a second click keeps it open");
    }
    assert!(h.state().ui.status.is_empty(), "{}", h.state().ui.status);
}

#[test]
fn window_plugin_opens_raises_and_closes_an_editor() {
    let (mut app, id) = app("audio", true, |t| t.mixer.inserts[C] = Some(Insert::new(HOSTED)));
    let editor = StandInEditor::default();
    adopt(&app, id, C, &editor);
    let mut run = |p: Value| app.run("window.plugin", p);
    let editor_open = |open: bool| Ok(json!({"view": "editor", "open": open}));
    assert_eq!(run(json!({"track": "Vox", "slot": "c", "value": true})), editor_open(true));
    assert_eq!(run(json!({"track": "Vox", "slot": 2, "value": true})), editor_open(true), "an open editor is raised");
    assert_eq!(run(json!({"track": id.0, "slot": "C", "view": "editor", "value": false})), editor_open(false));
    assert_eq!(run(json!({"track": "Vox", "slot": "c", "value": false})), editor_open(false), "already closed");
    assert_eq!(run(json!({"track": "Vox", "slot": "c"})), editor_open(true), "omitted: toggles");
    assert_eq!(run(json!({"track": "Vox", "slot": "c"})), editor_open(false));
    assert_eq!(editor.calls(), ["open", "open", "close", "open", "close"]);
    assert!(app.ui.plugin_windows.is_empty());
}

#[test]
fn window_plugin_toggles_a_parameter_window() {
    let (mut app, id) = app("audio", false, |t| t.mixer.inserts[1] = Some(Insert::new("eq_7band")));
    let p = json!({"track": "Vox", "slot": "b", "view": "parameters"});
    assert_eq!(app.run("window.plugin", p.clone()), Ok(json!({"view": "parameters", "open": true})));
    assert_eq!(app.ui.plugin_windows, vec![(id, 1)]);
    assert_eq!(app.run("window.plugin", p), Ok(json!({"view": "parameters", "open": false})));
    assert!(app.ui.plugin_windows.is_empty());
}

#[test]
fn window_plugin_refuses_bad_parameters() {
    let (mut app, _) = app("audio", false, |t| t.mixer.inserts[1] = Some(Insert::new("eq_7band")));
    for (p, says) in [
        (json!({"slot": 1}), "`track` required"),
        (json!({"track": "Nope", "slot": 1}), "no track named `Nope`"),
        (json!({"track": "Vox"}), "`slot` must be"),
        (json!({"track": "Vox", "slot": 10}), "`slot` must be"),
        (json!({"track": "Vox", "slot": "k"}), "`slot` must be"),
        (json!({"track": "Vox", "slot": 3}), "holds no plugin"),
        (json!({"track": "Vox", "slot": "instrument"}), "holds no plugin"),
        (json!({"track": "Vox", "slot": 1, "view": "gui"}), "`view` must be"),
        (json!({"track": "Vox", "slot": 1, "value": "yes"}), "`value` must be"),
    ] {
        let err = app.run("window.plugin", p.clone()).unwrap_err();
        assert!(err.contains(says), "{p}: {err}");
    }
    assert!(app.ui.plugin_windows.is_empty());
}

/// The insert menu alone, for `plugin` in Vox's insert C.
fn menu(plugin: &str) -> (Harness<'static, SoundApp>, TrackId) {
    let (app, id) = app("audio", false, |t| t.mixer.inserts[C] = Some(Insert::new(plugin)));
    let ins = Insert::new(plugin);
    let mut h = Harness::builder()
        .with_size(vec2(400.0, 600.0))
        .build_ui_state(move |ui, app: &mut SoundApp| soundcraft_ui_egui::mix_window::plugin_menu(app, ui, id, C, Some(&ins)), app);
    h.step();
    (h, id)
}

#[test]
fn an_unknown_hosted_inserts_menu_has_no_parameter_window_to_offer() {
    let (h, _) = menu(HOSTED);
    assert!(h.query_by_label("Edit Parameters…").is_none(), "SoundCraft knows none of its parameters");
    assert!(h.query_by_label("Bypass").is_some());
}

#[test]
fn a_built_in_inserts_menu_is_unchanged() {
    let (h, _) = menu("eq_7band");
    assert!(h.query_by_label("Edit Parameters…").is_none(), "a click already opens a built-in's parameters");
    assert!(h.query_by_label("Bypass").is_some());
}

#[test]
fn a_hosted_slots_tooltip_says_a_click_opens_or_closes_its_editor() {
    for (kind, label, tip) in [
        ("audio", "Insert C", format!("{HOSTED}: click to open or close its editor · {{cmd}}click to bypass · right-click to change")),
        ("audio", "Insert B", "EQ7: click to open · {cmd}click to bypass · right-click to change".to_string()),
        ("instrument", "Instrument", "Click: open or close its editor · right-click: change".to_string()),
    ] {
        let (app, _) = app(kind, false, |t| {
            t.mixer.inserts[1] = Some(Insert::new("eq_7band"));
            if kind == "audio" {
                t.mixer.inserts[C] = Some(Insert::new(HOSTED));
            } else {
                t.instrument = Some(Insert::new(HOSTED));
            }
        });
        let mut h = mix(app);
        let tip = tip.replace("{cmd}", if h.ctx.os().is_mac() { "⌘" } else { "Ctrl+" });
        h.get_by_label(label).hover();
        h.run_steps(60);
        assert!(h.query_by_label(&tip).is_some(), "{label}: {tip}");
    }
}
