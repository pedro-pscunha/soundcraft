use serde_json::json;
use soundcraft_model::Insert;
use soundcraft_playback::{Player, StandInEditor};
use soundcraft_ui_egui::{Services, SoundApp};

#[test]
fn hide_floating_closes_automation() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let main_window = app.ui.window;
    app.run("window.automation", json!({"value": true})).unwrap();
    app.run("window.transport", json!({"value": true})).unwrap();
    assert!(app.ui.show_automation && app.ui.show_transport);

    app.run("window.hide_floating", json!({})).unwrap();
    assert!(!app.ui.show_transport);
    assert!(!app.ui.show_automation, "Automation must be hidden");
    assert_eq!(app.ui.window, main_window);

    app.run("window.automation", json!({"value": true})).unwrap();
    assert!(app.ui.show_automation);
}

/// With audio on, hiding the floating windows also closes the plugins' own editors (here a
/// stand-in for one, on a player that opens no audio device).
#[test]
fn hide_floating_closes_plugin_windows_and_editors() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.run("track.new", json!({"name": "Vox"})).unwrap();
    let vox = app.engine.session().track_by_name("Vox").unwrap().id;
    app.engine.session_mut().track_mut(vox).unwrap().mixer.inserts[2] = Some(Insert::new("vst3:not-a-class-id"));
    app.player = Some(Player::without_device(app.engine.session_arc()));
    let editor = StandInEditor::default();
    assert!(app.player.as_ref().is_some_and(|p| p.adopt_editor(app.engine.session(), vox, 2, Box::new(editor.clone()))));
    for command in ["window.hide_floating", "window.close"] {
        app.ui.plugin_windows.push((vox, 0));
        app.run("window.plugin", json!({"track": "Vox", "slot": "c", "value": true})).unwrap();
        assert!(editor.shown(), "{command}");
        app.run(command, json!({})).unwrap();
        assert!(app.ui.plugin_windows.is_empty(), "{command}");
        assert!(!editor.shown(), "{command} closes the editor");
    }
    assert_eq!(editor.calls(), ["open", "close", "open", "close"]);
}
