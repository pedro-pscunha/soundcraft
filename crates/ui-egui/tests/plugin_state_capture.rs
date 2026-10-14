//! Saves and bounces store each live hosted plugin's state together with the values of the
//! overrides the plugin changed by itself, both read from the instance (here a stand-in reader,
//! on a player that opens no audio device).

use serde_json::json;
use soundcraft_dsp::{ParamReading, PluginStateReader};
use soundcraft_model::Insert;
use soundcraft_playback::Player;
use soundcraft_ui_egui::{Services, SoundApp};

/// A plugin that loaded a preset in its own editor: Mix is 0.8 where the host set 0.3; Time is
/// as the host set it.
struct PresetLoaded;

impl PluginStateReader for PresetLoaded {
    fn save_state(&mut self) -> Option<Vec<u8>> {
        Some(b"the preset's state".to_vec())
    }
    fn param_values(&mut self, ids: &[&str]) -> Vec<Option<ParamReading>> {
        let read = |id: &str| match id {
            "mix" => Some(ParamReading { live: 0.8, host: 0.3 }),
            "time" => Some(ParamReading { live: 0.5, host: 0.5 }),
            _ => None,
        };
        ids.iter().map(|id| read(id)).collect()
    }
}

#[test]
fn a_capture_stores_the_overrides_a_plugin_changed_by_itself() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.run("track.new", json!({"name": "Vox"})).unwrap();
    let vox = app.engine.session().track_by_name("Vox").unwrap().id;
    let mut delay = Insert::new("vst3:not-a-class-id");
    delay.params.insert("mix".into(), 0.3);
    delay.params.insert("time".into(), 0.5);
    app.engine.session_mut().track_mut(vox).unwrap().mixer.inserts[2] = Some(delay);
    app.player = Some(Player::without_device(app.engine.session_arc()));
    assert!(app.player.as_ref().is_some_and(|p| p.adopt_state_reader(app.engine.session(), vox, 2, Box::new(PresetLoaded))));
    app.capture_plugin_states();
    let delay = app.engine.session().track(vox).unwrap().mixer.inserts[2].clone().unwrap();
    assert_eq!(delay.state_bytes().unwrap(), b"the preset's state");
    assert_eq!(delay.params.get("mix"), Some(&0.8), "the old override would set the preset back at load");
    assert_eq!(delay.params.get("time"), Some(&0.5));
    assert_eq!(delay.params.len(), 2);
}
