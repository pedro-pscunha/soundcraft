//! `ui.audio_device`: the audio device choice is kept only once the devices reopen with it.
use serde_json::json;
use soundcraft_model::Session;
use soundcraft_playback::{AudioConfig, Player};
use soundcraft_ui_egui::{Services, SoundApp};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The configurations the app reopened the devices with.
static OPENED: AtomicUsize = AtomicUsize::new(0);

/// The app's player factory here: a player on the silent clock, as without a device. Tests never
/// open the real audio device.
fn no_device(session: Arc<Session>, cfg: &AudioConfig) -> Player {
    assert_eq!(cfg.buffer_size, Some(256), "opened with the chosen configuration");
    OPENED.fetch_add(1, Ordering::SeqCst);
    Player::without_device(session)
}

#[test]
fn the_audio_device_choice_is_kept_only_when_it_is_applied() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.open_player = no_device;
    let before = app.ui.audio.clone();

    let r = app.run("ui.audio_device", json!({})).unwrap();
    assert_eq!(r["config"], json!(before), "no change: just the choice and the device");

    assert!(app.run("ui.audio_device", json!({"sample_rate": 12})).is_err());
    assert_eq!(app.ui.audio, before, "an out-of-range rate changes nothing");

    app.engine.transport.recording = true;
    let err = app.run("ui.audio_device", json!({"buffer_size": 256, "output": "Elsewhere"})).unwrap_err();
    assert!(err.contains("stop recording"), "{err}");
    assert_eq!(app.ui.audio, before, "refused while recording: the saved choice stays the open device's");
    app.engine.transport.recording = false;
    assert_eq!(OPENED.load(Ordering::SeqCst), 0, "nothing reopened yet");

    // Reopens the devices with the choice, through the app's player factory.
    let r = app.run("ui.audio_device", json!({"buffer_size": 256})).unwrap();
    assert_eq!(app.ui.audio.buffer_size, Some(256));
    assert_eq!(r["config"]["buffer_size"], 256);
    assert_eq!(OPENED.load(Ordering::SeqCst), 1, "reopened once");
    assert_eq!(r["device"]["silent"], true, "{r}");
}
