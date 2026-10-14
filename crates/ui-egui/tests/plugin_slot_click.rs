//! A click on a hosted plugin's slot opens the plugin's own editor. Without one (here audio is
//! off, so nothing is loaded) its parameter window opens and the status line says why; the
//! parameter window also stays in the insert's menu.

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_model::{Insert, Track, TrackId};
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};

/// A VST3 id no plugin has: never looked up on disk.
const HOSTED: &str = "vst3:not-a-class-id";
const WHY: &str = "vst3:not-a-class-id is not loaded while audio is off, so its own editor can't open.";

/// The Mix window, with the INSTRUMENT row, of one `kind` track that `place` fills.
fn mix(kind: &str, place: impl FnOnce(&mut Track)) -> (Harness<'static, SoundApp>, TrackId) {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.run("track.new", json!({"kind": kind, "name": "Vox"})).unwrap();
    let id = app.engine.session().track_by_name("Vox").unwrap().id;
    place(app.engine.session_mut().track_mut(id).unwrap());
    app.ui.window = MainWindow::Mix;
    app.run("view.mix_section", json!({"section": "instruments"})).unwrap();
    let mut h = Harness::builder().with_size(vec2(1400.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    (h, id)
}

#[test]
fn a_hosted_insert_that_is_not_loaded_says_why_its_editor_did_not_open() {
    let (mut h, _) = mix("audio", |t| t.mixer.inserts[2] = Some(Insert::new(HOSTED)));
    assert!(h.state().ui.status.is_empty());
    h.get_by_label("Insert C").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.status, WHY);
}

#[test]
fn a_hosted_instrument_that_is_not_loaded_says_why_its_editor_did_not_open() {
    let (mut h, _) = mix("instrument", |t| t.instrument = Some(Insert::new(HOSTED)));
    h.get_by_label("Instrument").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.status, WHY);
}

#[test]
fn a_built_in_insert_opens_its_parameters_as_before() {
    let (mut h, vox) = mix("audio", |t| t.mixer.inserts[1] = Some(Insert::new("eq_7band")));
    h.get_by_label("Insert B").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.plugin_windows, vec![(vox, 1)]);
    assert!(h.state().ui.status.is_empty(), "{}", h.state().ui.status);
}

/// The insert menu alone, for `plugin` in Vox's insert C.
fn menu(plugin: &str) -> (Harness<'static, SoundApp>, TrackId) {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.run("track.new", json!({"kind": "audio", "name": "Vox"})).unwrap();
    let id = app.engine.session().track_by_name("Vox").unwrap().id;
    app.engine.session_mut().track_mut(id).unwrap().mixer.inserts[2] = Some(Insert::new(plugin));
    let mut h = Harness::builder()
        .with_size(vec2(400.0, 600.0))
        .build_ui_state(move |ui, app: &mut SoundApp| soundcraft_ui_egui::mix_window::plugin_menu(app, ui, id, 2, true), app);
    h.step();
    (h, id)
}

#[test]
fn a_hosted_inserts_menu_opens_its_parameters() {
    let (mut h, vox) = menu(HOSTED);
    assert!(h.state().ui.plugin_windows.is_empty());
    h.get_by_label("Edit Parameters…").click();
    h.step();
    assert_eq!(h.state().ui.plugin_windows, vec![(vox, 2)]);
}

#[test]
fn a_built_in_inserts_menu_is_unchanged() {
    let (h, _) = menu("eq_7band");
    assert!(h.query_by_label("Edit Parameters…").is_none(), "a click already opens a built-in's parameters");
    assert!(h.query_by_label("Bypass").is_some());
}
