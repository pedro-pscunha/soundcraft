//! Third-party plugin state in the session (`mix.insert_state`): the UI reads each live plugin's
//! state right before saving and stores it on its insert (base64, `live: true`), from where the
//! mixer restores it into new instances, together with the values of the overrides the plugin
//! changed by itself (`params`, which Undo and Redo keep). Any other state (an agent's, a
//! preset's) is new: live playback makes the plugin's instance again from it.

use super::*;
use serde_json::json;
use soundcraft_model::{INSERT_SLOTS, Insert, Session, b64};
use std::sync::Arc;

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "mix.insert_state",
        label: "Store Plugin State",
        menu: &[],
        shortcut: None,
        params: "{track, slot: 0..9 | \"a\"..\"j\" | \"instrument\", state: base64 | null, live?: bool, params?: {id: number}} → {bytes, params}. A new state replaces the playing instance with one made from it; `live: true` marks the state the playing instance already holds (read from it before a save), stored without replacing it. `params` (only with `live: true`, hosted plugins only) are the values the playing instance has for parameters it changed by itself (a preset loaded in its own editor): they replace the insert's overrides of those parameters, so the old values are not applied over the state when the session loads. Ids the insert has no override for are ignored (no override is added); the result counts the ones replaced. They are facts about the playing instance, not edits: Undo and Redo steps holding the same instance with the replaced value take the new one too. Not undoable and not journaled (states can be large).",
        enabled: has_tracks,
        run: insert_state,
        journal: false,
        undoable: false,
        view: false,
    }]
}

const CMD: &str = "mix.insert_state";

/// `None` = the instrument slot.
fn slot_param(p: &Value) -> Result<Option<usize>> {
    let v = p.get("slot").ok_or_else(|| bad(CMD, "`slot` required"))?;
    if let Some(n) = v.as_u64() {
        return usize::try_from(n).ok().filter(|&n| n < INSERT_SLOTS).map(Some).ok_or_else(|| bad(CMD, "slot out of range"));
    }
    let s = v.as_str().map(|s| s.trim().to_ascii_lowercase()).ok_or_else(|| bad(CMD, "`slot` must be a number or a letter"))?;
    if s == "instrument" {
        return Ok(None);
    }
    match s.as_bytes() {
        [c @ b'a'..=b'z'] if usize::from(c - b'a') < INSERT_SLOTS => Ok(Some(usize::from(c - b'a'))),
        _ => s.parse::<usize>().ok().filter(|&n| n < INSERT_SLOTS).map(Some).ok_or_else(|| bad(CMD, "slot out of range")),
    }
}

fn insert_state(e: &mut Engine, p: &Value) -> Result<Value> {
    let t = track_param(e, CMD, p, "track")?.ok_or_else(|| bad(CMD, "`track` required"))?;
    let slot = slot_param(p)?;
    let state = match p.get("state") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(b64::decode(s).ok_or_else(|| bad(CMD, "`state` is not valid base64 (or is too large)"))?),
        Some(_) => return Err(bad(CMD, "`state` must be a base64 string or null")),
    };
    let live = match p.get("live") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| bad(CMD, "`live` must be true or false"))?,
    };
    let params: Vec<(&String, f32)> = match p.get("params") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(_)) if !live => return Err(bad(CMD, "`params` go with `live: true` (values read from the playing instance)")),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                let f = v.as_f64().map(|f| f as f32).filter(|f| f.is_finite());
                f.map(|f| (k, f)).ok_or_else(|| bad(CMD, format!("parameter `{k}` needs a finite number")))
            })
            .collect::<Result<_>>()?,
        Some(_) => return Err(bad(CMD, "`params` must be an object of parameter values")),
    };
    let s = e.session_mut();
    s.track(t).ok_or_else(|| bad(CMD, "no track"))?;
    let ins = insert_at(s, t, slot).ok_or_else(|| bad(CMD, "empty slot"))?;
    if !params.is_empty() && !soundcraft_mix::is_third_party(&ins.plugin) {
        return Err(bad(CMD, "`params` are for hosted plugins (a built-in insert holds every parameter itself)"));
    }
    let n = state.as_ref().map_or(0, Vec::len);
    match state {
        Some(bytes) if live => ins.store_live_state(&bytes),
        Some(bytes) => ins.set_state_bytes(&bytes),
        None => ins.clear_state(),
    }
    let info = if params.is_empty() { None } else { super::clap::plugin_info(&ins.plugin) };
    let mut replaced = 0;
    let mut moved = Vec::new();
    for (k, v) in params {
        if let Some(o) = ins.params.get_mut(k.as_str()) {
            let to = info.and_then(|i| i.param(k)).map_or(v, |pi| pi.clamp(v));
            if *o != to {
                moved.push((k.clone(), *o, to));
            }
            *o = to;
            replaced += 1;
        }
    }
    if !moved.is_empty() {
        let (plugin, epoch) = (ins.plugin.clone(), ins.state_epoch());
        refresh_history(e, t, slot, &plugin, epoch, &moved);
    }
    Ok(json!({"bytes": n, "params": replaced}))
}

/// The insert in `slot` of track `t` (`None` = the instrument slot).
fn insert_at(s: &mut Session, t: TrackId, slot: Option<usize>) -> Option<&mut Insert> {
    let tr = s.track_mut(t)?;
    match slot {
        Some(i) => tr.mixer.inserts.get_mut(i).and_then(Option::as_mut),
        None => tr.instrument.as_mut(),
    }
}

/// Values read from the playing instance are facts about it, not edits, so no Undo or Redo step
/// may take them back: the mixer would send the plugin the old value of a preset loaded in its
/// editor. Every step that holds the same instance (plugin and state epoch) with the value a
/// capture replaced gets the new one; a step with another value keeps it (an edit of the user's
/// own, which undoing should restore), and so does another instance in that slot.
fn refresh_history(e: &mut Engine, t: TrackId, slot: Option<usize>, plugin: &str, epoch: u64, moved: &[(String, f32, f32)]) {
    let same = |ins: &Insert| ins.plugin == plugin && ins.state_epoch() == epoch;
    for h in e.undo.iter_mut().chain(e.redo.iter_mut()) {
        let ins = h.doc.track(t).and_then(|tr| match slot {
            Some(i) => tr.mixer.inserts.get(i).and_then(Option::as_ref),
            None => tr.instrument.as_ref(),
        });
        if !ins.is_some_and(|ins| same(ins) && moved.iter().any(|(k, from, _)| ins.params.get(k) == Some(from))) {
            continue;
        }
        let Some(ins) = insert_at(Arc::make_mut(&mut h.doc), t, slot) else { continue };
        for (k, from, to) in moved {
            if let Some(v) = ins.params.get_mut(k)
                && *v == *from
            {
                *v = *to;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;
    use soundcraft_model::b64;

    #[test]
    fn stores_and_clears_state_without_undo() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        e.execute("mix.insert", &json!({"track": t, "plugin": "gain", "slot": 2})).unwrap();
        let undo_before = e.can_undo();
        let blob = b64::encode(b"\x00\x01plugin state\xff");
        let r = e.execute("mix.insert_state", &json!({"track": t, "slot": "c", "state": blob})).unwrap();
        assert_eq!(r["bytes"], 15);
        let ins = e.session().tracks[0].mixer.inserts[2].clone().unwrap();
        assert_eq!(ins.state_bytes().unwrap(), b"\x00\x01plugin state\xff");
        assert_eq!(e.can_undo(), undo_before, "not undoable");
        assert!(e.journal.iter().all(|(id, _)| id != "mix.insert_state"), "not journaled");
        // Survives save/load.
        let dir = std::env::temp_dir().join(format!("soundcraft-insert-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.scraft");
        e.execute("session.save", &json!({"path": path.to_string_lossy()})).unwrap();
        let mut f = Engine::default();
        f.execute("session.open", &json!({"path": path.to_string_lossy()})).unwrap();
        assert_eq!(f.session().tracks[0].mixer.inserts[2].as_ref().unwrap().state(), ins.state());
        let _ = std::fs::remove_dir_all(&dir);
        e.execute("mix.insert_state", &json!({"track": t, "slot": 2, "state": null})).unwrap();
        assert_eq!(e.session().tracks[0].mixer.inserts[2].as_ref().unwrap().state(), None);
    }

    /// A state the UI read from the live instance before a save is stored without asking playback
    /// for a new instance; any other state (an agent's, a preset's) is new and gets one.
    #[test]
    fn a_live_state_keeps_the_instance_and_any_other_state_replaces_it() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        e.execute("mix.insert", &json!({"track": t, "plugin": "gain", "slot": 0})).unwrap();
        let epoch = |e: &Engine| e.session().tracks[0].mixer.inserts[0].as_ref().unwrap().state_epoch();
        let made = epoch(&e);
        let blob = b64::encode(b"live");
        e.execute("mix.insert_state", &json!({"track": t, "slot": 0, "state": blob, "live": true})).unwrap();
        assert_eq!(epoch(&e), made);
        e.execute("mix.insert_state", &json!({"track": t, "slot": 0, "state": blob})).unwrap();
        assert_ne!(epoch(&e), made, "the same bytes from a command are a new state");
        let set = epoch(&e);
        e.execute("mix.insert_state", &json!({"track": t, "slot": 0, "state": null})).unwrap();
        assert_ne!(epoch(&e), set);
        assert!(e.execute("mix.insert_state", &json!({"track": t, "slot": 0, "state": blob, "live": "yes"})).is_err());
    }

    /// `params` (values read from the playing instance) go only with `live: true`, only to hosted
    /// plugins (a built-in insert holds every parameter itself), and must be finite numbers. A
    /// refused call changes nothing, its state included. (Stored params: `tests/hosted_plugins.rs`.)
    #[test]
    fn live_params_are_refused_without_live_for_built_ins_and_when_not_numbers() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id;
        e.execute("mix.insert", &json!({"track": t.0, "plugin": "gain", "slot": 1})).unwrap();
        let mut hosted = soundcraft_model::Insert::new("clap:not.installed");
        hosted.params.insert("7".into(), 1.5);
        e.session_mut().track_mut(t).unwrap().mixer.inserts[0] = Some(hosted);
        let inserts = |e: &Engine| {
            let m = &e.session().tracks[0].mixer;
            m.inserts[..2].iter().map(|i| i.clone().map(|i| (i.state_epoch(), i))).collect::<Vec<_>>()
        };
        let before = inserts(&e);
        let blob = b64::encode(b"state");
        for p in [
            json!({"track": t.0, "slot": 0, "state": blob, "params": {"7": 1.0}}),
            json!({"track": t.0, "slot": 0, "state": blob, "live": false, "params": {"7": 1.0}}),
            json!({"track": t.0, "slot": 1, "state": blob, "live": true, "params": {"gain": 1.0}}),
            json!({"track": t.0, "slot": 0, "state": blob, "live": true, "params": {"7": "loud"}}),
            json!({"track": t.0, "slot": 0, "state": blob, "live": true, "params": {"7": 1e39}}),
            json!({"track": t.0, "slot": 0, "state": blob, "live": true, "params": [1.0]}),
        ] {
            assert!(e.execute("mix.insert_state", &p).is_err(), "{p}");
        }
        assert_eq!(inserts(&e), before);
    }

    /// A value read from the playing instance (a preset loaded in its editor) is a fact about it:
    /// Undo and Redo of earlier edits keep it, or the mixer would send the plugin the old value.
    /// A step with another value (the user's own edit) or another instance keeps its own.
    #[test]
    fn undo_and_redo_keep_the_values_read_from_the_playing_instance() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id;
        let hosted = |e: &mut Engine, mix: f32| {
            let mut ins = soundcraft_model::Insert::new("clap:not.installed");
            ins.params.extend([("mix".to_string(), mix), ("drive".to_string(), 0.25)]);
            let before = e.session_arc();
            e.session_mut().track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
            e.push_undo("Insert", before);
        };
        hosted(&mut e, 0.1); // another instance, replaced next
        hosted(&mut e, 0.3);
        let before = e.session_arc();
        e.session_mut().track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("drive".into(), 0.5);
        e.push_undo("Drive", before);
        e.execute("track.new", &json!({})).unwrap();
        e.undo();
        let live = json!({"track": t.0, "slot": 0, "state": b64::encode(b"preset"), "live": true, "params": {"mix": 0.8, "drive": 0.9}});
        e.execute("mix.insert_state", &live).unwrap();
        let params = |e: &Engine| {
            let p = &e.session().tracks[0].mixer.inserts[0].as_ref().unwrap().params;
            (p["mix"], p["drive"])
        };
        e.redo(); // the track.new step, made before the capture
        assert_eq!(params(&e), (0.8, 0.9));
        e.undo();
        e.undo(); // the Drive edit: its own earlier value comes back, the plugin's mix stays
        assert_eq!(params(&e), (0.8, 0.25));
        e.undo(); // the other instance
        assert_eq!(params(&e), (0.1, 0.25));
        e.redo();
        e.redo();
        assert_eq!(params(&e), (0.8, 0.9));
    }

    #[test]
    fn hostile_params_are_errors() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        e.execute("mix.insert", &json!({"track": t, "plugin": "gain", "slot": 0})).unwrap();
        for p in [
            json!({"track": t, "slot": 0, "state": "!!!"}),
            json!({"track": t, "slot": 0, "state": 5}),
            json!({"track": t, "slot": 1, "state": "AAAA"}),
            json!({"track": t, "slot": 99, "state": "AAAA"}),
            json!({"track": t, "slot": "instrument", "state": "AAAA"}),
            json!({"track": t, "slot": -1}),
            json!({"track": t}),
            json!({"track": 999, "slot": 0}),
            json!({"slot": 0}),
        ] {
            assert!(e.execute("mix.insert_state", &p).is_err(), "{p}");
        }
    }
}
