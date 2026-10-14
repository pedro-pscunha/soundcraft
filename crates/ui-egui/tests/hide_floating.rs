use serde_json::json;
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

/// With audio on, hiding the floating windows also asks the player to close the plugins' own
/// editors. None is open here (that takes a real hosted plugin): this runs the path with a player.
#[test]
fn hide_floating_closes_plugin_windows_with_audio_on() {
    let engine = soundcraft_engine::Engine::default();
    let player = soundcraft_playback::Player::new(engine.session_arc());
    let mut app = SoundApp::new(engine, Some(player), Services::default());
    app.run("track.new", json!({"name": "Vox"})).unwrap();
    let vox = app.engine.session().track_by_name("Vox").unwrap().id;
    for command in ["window.hide_floating", "window.close"] {
        app.ui.plugin_windows.push((vox, 0));
        app.run(command, json!({})).unwrap();
        assert!(app.ui.plugin_windows.is_empty(), "{command}");
        assert!(app.player.as_ref().is_some_and(|p| !p.any_editor_open()), "{command}");
    }
}
