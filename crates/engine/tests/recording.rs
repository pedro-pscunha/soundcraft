//! A take being recorded is never replaced or quit away from, and the transport recording it is
//! never moved or restarted.
use serde_json::{Value, json};
use soundcraft_engine::{Engine, EngineError};

fn refused(e: &mut Engine, id: &str, params: &Value) -> bool {
    matches!(e.execute(id, params), Err(EngineError::Disabled(_, why)) if why == "stop recording first")
}

#[test]
fn nothing_replaces_or_quits_a_session_while_it_records() {
    let mut e = Engine::default();
    e.transport.recording = true;
    for (id, params) in [
        ("session.new", json!({})),
        ("session.open", json!({"path": "Song.scraft"})),
        ("session.close", json!({})),
        ("session.revert", json!({})),
        ("app.quit", json!({})),
    ] {
        assert!(refused(&mut e, id, &params), "{id}");
    }
    assert!(e.execute("track.new", &json!({})).is_ok(), "editing goes on");
    e.transport.recording = false;
    assert!(e.execute("session.close", &json!({})).is_ok());
}

/// A take is placed from where recording started: every command that would move the transport or
/// restart playback is refused while recording, before it changes the selection or the playhead.
#[test]
fn nothing_moves_or_restarts_the_transport_while_it_records() {
    let mut e = soundcraft_engine::demo::demo_engine();
    let moves = [
        ("transport.play", json!({"from": 480_000})),
        ("transport.locate", json!({"at": 480_000})),
        ("transport.rtz", json!({})),
        ("transport.go_to_end", json!({})),
        ("transport.rewind", json!({"seconds": 2})),
        ("transport.fast_forward", json!({"seconds": 2})),
        ("transport.play_selection", json!({})),
        ("transport.half_speed", json!({})),
        ("edit.play_timeline", json!({})),
        ("markers.recall", json!({"number": 3})),
        ("markers.next", json!({})),
        ("markers.previous", json!({})),
        ("edit.tab", json!({})),
        ("edit.tab", json!({"back": true})),
    ];
    e.transport.playing = true;
    e.transport.recording = true;
    let before = (e.session().edit.selection, e.session().edit.playhead);
    for (id, params) in &moves {
        assert!(refused(&mut e, id, params), "{id} {params}");
        assert!(e.transport_requests.is_empty(), "{id} {params}: {:?}", e.transport_requests);
        assert_eq!((e.session().edit.selection, e.session().edit.playhead), before, "{id} {params}");
    }
    // Stopping goes on, Play while playing changes nothing, and Shift+Tab only extends the selection.
    for (id, params) in [("transport.play", json!({})), ("edit.tab", json!({"extend": true})), ("transport.record", json!({}))] {
        assert!(e.execute(id, &params).is_ok(), "{id} {params}");
    }
    // Once recording stopped, each moves the transport again.
    e.transport.recording = false;
    for (id, params) in &moves {
        e.transport_requests.clear();
        assert!(e.execute(id, params).is_ok(), "{id} {params}");
        assert!(!e.transport_requests.is_empty(), "{id} {params}");
    }
}
