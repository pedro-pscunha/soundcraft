//! A plugin's windows: its own editor (CLAP, VST3 and Audio Unit plugins draw one) and SoundCraft's
//! parameter window. `window.plugin` opens and closes them, and everything that does dispatches
//! it: a click on a used plugin slot, the instrument menu, the parameter window's editor button and
//! an insert's Edit Parameters….

use crate::SoundApp;
use serde_json::{Value, json};
use soundcraft_mix::INSTRUMENT_SLOT;
use soundcraft_model::{INSERT_SLOTS, TrackId};
use soundcraft_playback::EditorState;
use std::time::{Duration, Instant};

/// The parameters of `window.plugin`, as `engine.commands` lists them.
pub const PARAMS: &str = "{track, slot: 0..9 | a..j | \"instrument\", view?: \"editor\" (default: a hosted plugin's own editor, \
                          else its parameter window) | \"parameters\", value?: true opens (or raises), false closes, omitted or null \
                          toggles (except a built-in plugin's parameter window asked for as its editor, which stays open)}";

/// The fields `window.plugin` reads; it refuses any other, rather than ignore it.
const FIELDS: [&str; 4] = ["track", "slot", "view", "value"];

/// Why a hosted plugin's editor can't open while audio is off.
const AUDIO_OFF: &str = "is not loaded while audio is off, so its own editor can't open";
/// Why nothing of a plugin can open when SoundCraft knows nothing of it.
const NOT_INSTALLED: &str = "is not installed or could not be loaded";

/// Which of a plugin's windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// A hosted plugin's own editor; a built-in plugin has none, so its parameter window.
    Editor,
    Parameters,
}

/// The plugin in a slot, as `window.plugin` needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    /// A CLAP, VST3 or Audio Unit plugin.
    hosted: bool,
    instrument: bool,
    /// SoundCraft knows the plugin's parameters (it is built in, or the host found it).
    known: bool,
}

/// What `window.plugin` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Opens the plugin's own editor, or raises it when open.
    OpenEditor,
    CloseEditor,
    ShowParameters,
    HideParameters,
    /// The editor can't open, for this reason: the parameter window opens instead.
    Fallback(&'static str),
    /// Nothing opens, for this reason. A hosted instrument keeps its sounds in its own editor, so
    /// its parameter window would be a dead end.
    Refuse(&'static str),
    /// Already as asked.
    Nothing,
}

/// Why a hosted plugin's own editor can't open (`editor`: where it stands in the player, `None`
/// while audio is off), worded to follow the plugin's name; `None` when it can.
pub(crate) fn editor_unavailable(editor: Option<EditorState>) -> Option<&'static str> {
    editor.map_or(Some(AUDIO_OFF), EditorState::unavailable)
}

/// What `window.plugin` does with the plugin `slot` describes: `editor` is where its own editor
/// stands (`None` while audio is off), `want` opens (true), closes (false) or toggles (`None`), and
/// `shown` says whether its parameter window is open.
fn action(slot: Slot, editor: Option<EditorState>, view: View, want: Option<bool>, shown: bool) -> Action {
    let parameters = |want: Option<bool>| match want {
        Some(false) => Action::HideParameters,
        None if shown => Action::HideParameters,
        // The parameter window lists the parameters SoundCraft knows: none here.
        _ if !slot.known => Action::Refuse(NOT_INSTALLED),
        _ => Action::ShowParameters,
    };
    if view == View::Parameters {
        return parameters(want);
    }
    if !slot.hosted {
        // A built-in plugin's parameter window is its editor. A click opens it, and keeps it open.
        return parameters(want.or(Some(true)));
    }
    let open = editor == Some(EditorState::Open);
    match want {
        Some(false) | None if open => Action::CloseEditor,
        Some(false) => Action::Nothing,
        _ => match editor_unavailable(editor) {
            None => Action::OpenEditor,
            // A plugin the player made is installed, whatever the scan says; one it did not make,
            // and SoundCraft knows nothing of, is not.
            Some(_) if !slot.known => Action::Refuse(NOT_INSTALLED),
            Some(why) if slot.instrument => Action::Refuse(why),
            Some(why) => Action::Fallback(why),
        },
    }
}

/// `window.plugin`: opens or closes a plugin's own editor or its parameter window ([`PARAMS`]).
/// When the editor can't open, the status line says why and an insert's parameter window opens
/// instead; nothing opens for a hosted instrument (its sounds live in its own editor) or for a
/// plugin SoundCraft doesn't know, and the result is an error.
pub(crate) fn command(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    let bad = |m: &str| format!("bad parameters for `window.plugin`: {m}");
    if let Some(k) = p.as_object().and_then(|o| o.keys().find(|k| !FIELDS.contains(&k.as_str()))) {
        return Err(bad(&format!("unknown field `{k}` (it takes {})", FIELDS.join(", "))));
    }
    let track = soundcraft_engine::cmd::track_param(&app.engine, "window.plugin", p, "track")
        .map_err(|e| e.to_string())?
        .ok_or_else(|| bad("`track` required"))?;
    let slot = slot_param(p.get("slot")).ok_or_else(|| bad("`slot` must be 0..9, a..j or \"instrument\""))?;
    let view = match p.get("view") {
        None | Some(Value::Null) => View::Editor,
        Some(v) => match v.as_str() {
            Some("editor") => View::Editor,
            Some("parameters") => View::Parameters,
            _ => return Err(bad("`view` must be \"editor\" or \"parameters\"")),
        },
    };
    let want = match p.get("value") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_bool().ok_or_else(|| bad("`value` must be true or false"))?),
    };
    let plugin = {
        let t = app.engine.session().track(track);
        let ins = if slot == INSTRUMENT_SLOT { t.and_then(|t| t.instrument.as_ref()) } else { t.and_then(|t| t.mixer.inserts.get(slot)?.as_ref()) };
        ins.map(|i| i.plugin.clone()).ok_or_else(|| bad("that slot holds no plugin"))?
    };
    let info = crate::mix_window::plugin_info(&plugin);
    let name = info.map_or(plugin.as_str(), |i| i.name);
    let s = Slot { hosted: soundcraft_mix::is_third_party(&plugin), instrument: slot == INSTRUMENT_SLOT, known: info.is_some() };
    let editor = app.player.as_ref().map(|p| p.editor_state(track, slot));
    let shown = app.ui.plugin_windows.contains(&(track, slot));
    // The editor did not open, and `why` (the status line) says why: an insert's parameter window
    // opens instead, when SoundCraft knows its parameters.
    let fallback = |app: &mut SoundApp, why: String| {
        app.ui.status = why.clone();
        if s.instrument || !s.known {
            return Err(why);
        }
        show_parameters(app, track, slot);
        Ok(json!({"view": "parameters", "open": true, "reason": why}))
    };
    match action(s, editor, view, want, shown) {
        Action::OpenEditor => match app.player.as_ref().map(|p| p.open_editor(track, slot)) {
            Some(Ok(())) => Ok(json!({"view": "editor", "open": true})),
            Some(Err(e)) => fallback(app, format!("{name}: {e}")),
            None => fallback(app, format!("{name} {AUDIO_OFF}.")),
        },
        Action::CloseEditor => {
            if let Some(p) = &app.player {
                p.close_editor(track, slot);
            }
            Ok(json!({"view": "editor", "open": false}))
        }
        Action::ShowParameters => {
            show_parameters(app, track, slot);
            Ok(json!({"view": "parameters", "open": true}))
        }
        Action::HideParameters => {
            app.ui.plugin_windows.retain(|w| *w != (track, slot));
            Ok(json!({"view": "parameters", "open": false}))
        }
        Action::Fallback(why) => fallback(app, format!("{name} {why}.")),
        Action::Refuse(why) => {
            app.ui.status = format!("{name} {why}.");
            Err(app.ui.status.clone())
        }
        Action::Nothing => Ok(json!({"view": "editor", "open": false})),
    }
}

/// `slot` as `window.plugin` takes it: an insert's index (0..9) or letter (a..j), or "instrument".
fn slot_param(v: Option<&Value>) -> Option<usize> {
    let v = v?;
    let slot = match (v.as_u64(), v.as_str().map(|s| s.trim().to_ascii_lowercase())) {
        (Some(n), _) => usize::try_from(n).ok()?,
        (_, Some(s)) if s == "instrument" => return Some(INSTRUMENT_SLOT),
        (_, Some(s)) => match s.as_bytes() {
            [c @ b'a'..=b'z'] => usize::from(c - b'a'),
            _ => s.parse().ok()?,
        },
        _ => return None,
    };
    (slot < INSERT_SLOTS).then_some(slot)
}

/// How long after SoundCraft becomes the active application a press still counts as the click that
/// brought it forward.
const ACTIVATING: Duration = Duration::from_millis(150);

/// When the pointer was last pressed in a SoundCraft window, to tell the click that brings
/// SoundCraft forward from later ones ([`slot_click`]).
///
/// What counts is SoundCraft becoming the active application, not one of its windows gaining
/// focus: an editor's window becomes key when it opens or is raised, so the next click on
/// SoundCraft's window gives that window focus back while SoundCraft was active all along, and
/// that click must still close the editor.
#[derive(Debug)]
pub(crate) struct Activation {
    /// When SoundCraft last became the active application: macOS reports it
    /// (`soundcraft_macos::activation`); nothing does elsewhere, where editors don't hide with
    /// the app. Tests stand in for it.
    became_active: fn() -> Option<Instant>,
    pressed_at: Option<Instant>,
}

impl Default for Activation {
    fn default() -> Self {
        Activation::new(soundcraft_macos::activation::last_activated)
    }
}

impl Activation {
    pub(crate) fn new(became_active: fn() -> Option<Instant>) -> Self {
        Activation { became_active, pressed_at: None }
    }

    /// Call every frame of every SoundCraft window, with that window's input. Presses are timed
    /// only once SoundCraft has been activated: never off macOS, nor in the browser, where
    /// `Instant::now` panics.
    pub(crate) fn observe(&mut self, i: &egui::InputState) {
        if i.pointer.any_pressed() && (self.became_active)().is_some() {
            self.pressed_at = Some(Instant::now());
        }
    }

    /// The last press came as SoundCraft became the active application (either may come first):
    /// it brought SoundCraft forward.
    pub(crate) fn activating(&self) -> bool {
        match (self.pressed_at, (self.became_active)()) {
            (Some(pressed), Some(active)) => pressed.max(active).duration_since(pressed.min(active)) <= ACTIVATING,
            _ => false,
        }
    }
}

/// The `value` a click on a used plugin slot gives `window.plugin`: none, a toggle, as a Pro Tools
/// insert button does with its plug-in window. macOS hides plugin editors while SoundCraft is in
/// the background and shows them again when it comes forward, so the click that brings SoundCraft
/// forward lands on an editor that has just come back: that click only opens (raises), never closes.
/// A click on SoundCraft's window while an editor's window is key is no such click: SoundCraft is
/// already active, and the click toggles.
pub(crate) fn slot_click(activating: bool) -> Option<bool> {
    activating.then_some(true)
}

/// `slot` (an insert, or [`INSTRUMENT_SLOT`]) as `window.plugin`'s `slot` parameter.
pub(crate) fn slot_value(slot: usize) -> Value {
    if slot == INSTRUMENT_SLOT { json!("instrument") } else { json!(slot) }
}

/// Opens SoundCraft's parameter window for `slot` (an insert, or the instrument), once.
fn show_parameters(app: &mut SoundApp, track: TrackId, slot: usize) {
    if !app.ui.plugin_windows.contains(&(track, slot)) {
        app.ui.plugin_windows.push((track, slot));
    }
}

#[cfg(test)]
mod tests {
    use super::{AUDIO_OFF, Action, Activation, NOT_INSTALLED, Slot, View, action, slot_param};
    use crate::SoundApp;
    use serde_json::json;
    use soundcraft_mix::INSTRUMENT_SLOT;
    use soundcraft_model::{Insert, TrackId};
    use soundcraft_playback::{EditorState, StandInEditor};
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    const STATES: [Option<EditorState>; 6] = [
        None,
        Some(EditorState::NotLoaded),
        Some(EditorState::Failed),
        Some(EditorState::NoEditor),
        Some(EditorState::Closed),
        Some(EditorState::Open),
    ];
    const INSERT: Slot = Slot { hosted: true, instrument: false, known: true };
    const INSTRUMENT: Slot = Slot { hosted: true, instrument: true, known: true };
    const BUILT_IN: Slot = Slot { hosted: false, instrument: false, known: true };

    #[test]
    fn a_click_opens_a_hosted_plugins_editor_and_a_second_click_closes_it() {
        for slot in [INSERT, INSTRUMENT] {
            assert_eq!(action(slot, Some(EditorState::Closed), View::Editor, None, false), Action::OpenEditor);
            assert_eq!(action(slot, Some(EditorState::Open), View::Editor, None, false), Action::CloseEditor);
            // Asked to open, an open editor is raised; asked to close, a closed one stays closed.
            assert_eq!(action(slot, Some(EditorState::Open), View::Editor, Some(true), false), Action::OpenEditor);
            assert_eq!(action(slot, Some(EditorState::Closed), View::Editor, Some(false), false), Action::Nothing);
            assert_eq!(action(slot, Some(EditorState::Open), View::Editor, Some(false), false), Action::CloseEditor);
        }
    }

    #[test]
    fn each_reason_the_editor_cant_open_has_its_own_words() {
        let reasons = [
            (None, AUDIO_OFF),
            (Some(EditorState::NotLoaded), "is not loaded yet"),
            (Some(EditorState::Failed), "did not load"),
            (Some(EditorState::NoEditor), "has no editor of its own"),
        ];
        for (editor, why) in reasons {
            // An insert's parameter window opens instead; a hosted instrument's would be a dead end.
            assert_eq!(action(INSERT, editor, View::Editor, None, false), Action::Fallback(why), "{editor:?}");
            assert_eq!(action(INSTRUMENT, editor, View::Editor, None, false), Action::Refuse(why), "{editor:?}");
        }
    }

    #[test]
    fn a_plugin_soundcraft_knows_nothing_of_opens_nothing() {
        let unknown = Slot { known: false, ..INSERT };
        for editor in STATES {
            let want = match editor {
                // The player made it after all: its editor works.
                Some(EditorState::Closed) => Action::OpenEditor,
                Some(EditorState::Open) => Action::CloseEditor,
                _ => Action::Refuse(NOT_INSTALLED),
            };
            assert_eq!(action(unknown, editor, View::Editor, None, false), want, "{editor:?}");
            assert_eq!(action(unknown, editor, View::Parameters, Some(true), false), Action::Refuse(NOT_INSTALLED), "{editor:?}");
        }
    }

    #[test]
    fn a_built_in_plugin_opens_its_parameters_and_a_click_keeps_them_open() {
        for editor in STATES {
            assert_eq!(action(BUILT_IN, editor, View::Editor, None, false), Action::ShowParameters, "{editor:?}");
            assert_eq!(action(BUILT_IN, editor, View::Editor, None, true), Action::ShowParameters, "{editor:?}");
            assert_eq!(action(BUILT_IN, editor, View::Editor, Some(false), true), Action::HideParameters, "{editor:?}");
        }
    }

    #[test]
    fn the_parameter_window_opens_closes_and_toggles() {
        for slot in [INSERT, INSTRUMENT, BUILT_IN] {
            assert_eq!(action(slot, Some(EditorState::Open), View::Parameters, None, false), Action::ShowParameters);
            assert_eq!(action(slot, Some(EditorState::Open), View::Parameters, None, true), Action::HideParameters);
            assert_eq!(action(slot, None, View::Parameters, Some(true), true), Action::ShowParameters);
            assert_eq!(action(slot, None, View::Parameters, Some(false), false), Action::HideParameters);
        }
    }

    /// Hosted plugins SoundCraft knows (as a scan would make them known).
    const DELAY: &str = "vst3:fake-delay";
    const SYNTH: &str = "vst3:fake-synth";

    /// Vox, an audio track with the hosted Fake Delay in insert C, and Keys, an instrument track
    /// playing the hosted Fake Synth; with a player that opens no device when `audio`. The harness
    /// draws the app with its floating windows.
    fn session(audio: bool) -> (egui_kittest::Harness<'static, SoundApp>, TrackId, TrackId) {
        crate::mix_window::test_plugins::know(DELAY, "Fake Delay", "mod_delay");
        crate::mix_window::test_plugins::know(SYNTH, "Fake Synth", "subtractive_synth");
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
        app.run("track.new", json!({"kind": "audio", "name": "Vox"})).unwrap();
        app.run("track.new", json!({"kind": "instrument", "name": "Keys"})).unwrap();
        let (vox, keys) = (app.engine.session().track_by_name("Vox").unwrap().id, app.engine.session().track_by_name("Keys").unwrap().id);
        app.engine.session_mut().track_mut(vox).unwrap().mixer.inserts[2] = Some(Insert::new(DELAY));
        app.engine.session_mut().track_mut(keys).unwrap().instrument = Some(Insert::new(SYNTH));
        if audio {
            app.player = Some(soundcraft_playback::Player::without_device(app.engine.session_arc()));
        }
        let h = egui_kittest::Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
            |ui, app: &mut SoundApp| {
                app.logic(&ui.ctx().clone());
                app.ui(ui);
            },
            app,
        );
        (h, vox, keys)
    }

    /// The parameter window of `slot` is drawn.
    fn drawn(h: &egui_kittest::Harness<'static, SoundApp>, track: TrackId, slot: usize) -> bool {
        h.ctx.memory(|m| m.area_rect(egui::Id::new(("plugin", track.0, slot))).is_some())
    }

    #[test]
    fn an_editor_that_fails_to_open_leaves_an_inserts_parameter_window_with_the_reason() {
        let (mut h, vox, _) = session(true);
        let editor = StandInEditor::refusing("its view could not be made");
        let app = h.state_mut();
        assert!(app.player.as_ref().unwrap().adopt_editor(app.engine.session(), vox, 2, Box::new(editor.clone())));
        let why = "Fake Delay: its view could not be made";
        assert_eq!(app.run("window.plugin", json!({"track": "Vox", "slot": "c"})), Ok(json!({"view": "parameters", "open": true, "reason": why})));
        assert_eq!((editor.calls(), app.ui.status.as_str()), (vec!["open"], why));
        h.run_steps(3);
        assert!(drawn(&h, vox, 2));
    }

    #[test]
    fn while_audio_is_off_an_insert_opens_its_parameter_window_with_the_reason() {
        let (mut h, vox, _) = session(false);
        let r = h.state_mut().run("window.plugin", json!({"track": "Vox", "slot": "c"}));
        let why = format!("Fake Delay {AUDIO_OFF}.");
        assert_eq!(r, Ok(json!({"view": "parameters", "open": true, "reason": why})));
        assert_eq!(h.state().ui.status, why);
        h.run_steps(3);
        assert!(drawn(&h, vox, 2));
    }

    #[test]
    fn a_hosted_instrument_whose_editor_cant_open_says_why_and_opens_nothing() {
        for (audio, why) in [(false, AUDIO_OFF), (true, "did not load")] {
            let (mut h, _, keys) = session(audio);
            let why = format!("Fake Synth {why}.");
            let r = h.state_mut().run("window.plugin", json!({"track": "Keys", "slot": "instrument"}));
            assert_eq!(r, Err(why.clone()), "audio {audio}");
            assert_eq!(h.state().ui.status, why);
            h.run_steps(3);
            assert!(h.state().ui.plugin_windows.is_empty() && !drawn(&h, keys, INSTRUMENT_SLOT), "audio {audio}: no dead end");
        }
    }

    #[test]
    fn a_hosted_instrument_whose_editor_fails_to_open_says_why_and_opens_nothing() {
        let (mut h, _, keys) = session(true);
        let editor = StandInEditor::refusing("its view could not be made");
        let app = h.state_mut();
        assert!(app.player.as_ref().unwrap().adopt_editor(app.engine.session(), keys, INSTRUMENT_SLOT, Box::new(editor.clone())));
        let why = "Fake Synth: its view could not be made";
        assert_eq!(app.run("window.plugin", json!({"track": "Keys", "slot": "instrument"})), Err(why.into()));
        assert_eq!((editor.calls(), app.ui.status.as_str()), (vec!["open"], why));
        assert!(app.ui.plugin_windows.is_empty());
    }

    #[test]
    fn the_parameter_windows_button_opens_and_closes_the_editor_or_says_why_it_cant() {
        use egui_kittest::kittest::Queryable;
        let (mut h, vox, _) = session(true);
        let editor = StandInEditor::default();
        let app = h.state_mut();
        assert!(app.player.as_ref().unwrap().adopt_editor(app.engine.session(), vox, 2, Box::new(editor.clone())));
        app.run("window.plugin", json!({"track": "Vox", "slot": "c", "view": "parameters"})).unwrap();
        h.run_steps(3);
        h.get_by_label("Open Plugin Editor").click();
        h.run_steps(2);
        h.get_by_label("Close Plugin Editor").click();
        h.run_steps(2);
        assert_eq!(editor.calls(), ["open", "close"]);

        // Audio off: the button can't work, and its tooltip says why in the status line's words.
        let (mut h, _, _) = session(false);
        h.state_mut().run("window.plugin", json!({"track": "Vox", "slot": "c", "view": "parameters"})).unwrap();
        h.run_steps(3);
        h.get_by_label("Open Plugin Editor").hover();
        h.run_steps(60);
        assert!(h.query_by_label(&format!("This plugin {AUDIO_OFF}")).is_some());
    }

    #[test]
    fn the_instrument_menu_opens_and_closes_a_hosted_instruments_editor() {
        use egui_kittest::kittest::Queryable;
        let (h, _, keys) = session(true);
        let app = h.into_state();
        let editor = StandInEditor::default();
        assert!(app.player.as_ref().unwrap().adopt_editor(app.engine.session(), keys, INSTRUMENT_SLOT, Box::new(editor.clone())));
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 600.0))
            .build_ui_state(move |ui, app: &mut SoundApp| crate::mix_window::instrument_menu(app, ui, keys), app);
        h.step();
        h.get_by_label("Open Plugin Editor").click();
        h.run_steps(2);
        h.get_by_label("Close Plugin Editor").click();
        h.run_steps(2);
        assert_eq!(editor.calls(), ["open", "close"]);
        assert!(h.query_by_label("Edit Parameters…").is_none(), "a hosted instrument's sounds live in its own editor");
    }

    #[test]
    fn a_known_hosted_inserts_menu_opens_its_parameter_window() {
        use egui_kittest::kittest::Queryable;
        crate::mix_window::test_plugins::know(DELAY, "Fake Delay", "mod_delay");
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
        app.run("track.new", json!({"kind": "audio", "name": "Vox"})).unwrap();
        let vox = app.engine.session().track_by_name("Vox").unwrap().id;
        app.engine.session_mut().track_mut(vox).unwrap().mixer.inserts[2] = Some(Insert::new(DELAY));
        let ins = Insert::new(DELAY);
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 600.0))
            .build_ui_state(move |ui, app: &mut SoundApp| crate::mix_window::plugin_menu(app, ui, vox, 2, Some(&ins)), app);
        h.step();
        h.get_by_label("Edit Parameters…").click();
        h.step();
        assert_eq!(h.state().ui.plugin_windows, vec![(vox, 2)]);
        assert!(h.state().ui.status.is_empty(), "{}", h.state().ui.status);
    }

    thread_local! {
        /// When SoundCraft last became the active application, as a test has it.
        static BECAME_ACTIVE: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    /// The tests' stand-in for `soundcraft_macos::activation::last_activated`.
    fn became_active() -> Option<Instant> {
        BECAME_ACTIVE.get()
    }

    /// SoundCraft became the active application `ago` before now.
    fn activated(ago: Duration) {
        BECAME_ACTIVE.set(Instant::now().checked_sub(ago));
    }

    #[test]
    fn only_a_press_as_soundcraft_becomes_the_active_application_is_activating() {
        assert_eq!(super::slot_click(true), Some(true));
        assert_eq!(super::slot_click(false), None, "any other click toggles");
        let frame = |events: Vec<egui::Event>| {
            let ctx = egui::Context::default();
            let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |_| {});
            out.textures_delta.clear();
            ctx
        };
        let press = || egui::Event::PointerButton {
            pos: egui::pos2(5.0, 5.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        // Nothing reports an activation (off macOS): no press is activating, whatever the focus.
        let mut a = Activation::new(|| None);
        assert!(!a.activating(), "nothing yet");
        frame(vec![egui::Event::WindowFocused(true), press()]).input(|i| a.observe(i));
        assert!(!a.activating(), "no activation to come with");

        let mut a = Activation::new(became_active);
        activated(Duration::from_secs(60));
        frame(vec![press()]).input(|i| a.observe(i));
        assert!(!a.activating(), "SoundCraft was already in front");
        // An editor's window was key: SoundCraft's window regains focus with the press, but
        // SoundCraft was the active application all along.
        frame(vec![egui::Event::WindowFocused(true), press()]).input(|i| a.observe(i));
        assert!(!a.activating(), "a window regaining focus is no activation");
        activated(Duration::from_millis(20));
        frame(vec![press()]).input(|i| a.observe(i));
        assert!(a.activating(), "pressed as SoundCraft became active");
        activated(Duration::from_secs(1));
        assert!(!a.activating(), "pressed long after");
        // The activation may be reported just after the press is seen.
        frame(vec![press()]).input(|i| a.observe(i));
        activated(Duration::ZERO);
        assert!(a.activating(), "either order");
        // Each SoundCraft window (the separate Mix window too) has its own egui clock: it plays
        // no part.
        for time in [0.0, 1.0e6] {
            let ctx = egui::Context::default();
            let mut out = ctx.run_ui(egui::RawInput { time: Some(time), events: vec![press()], ..Default::default() }, |_| {});
            out.textures_delta.clear();
            activated(Duration::from_millis(20));
            ctx.input(|i| a.observe(i));
            assert!(a.activating(), "egui time {time}");
        }
    }

    #[test]
    fn the_click_that_brings_soundcraft_forward_raises_the_editor_and_a_later_click_closes_it() {
        use egui_kittest::kittest::Queryable;
        let (mut h, vox, _) = session(true);
        let editor = StandInEditor::default();
        let app = h.state_mut();
        assert!(app.player.as_ref().unwrap().adopt_editor(app.engine.session(), vox, 2, Box::new(editor.clone())));
        app.activation = Activation::new(became_active);
        app.ui.window = crate::MainWindow::Mix;
        activated(Duration::from_secs(60));
        h.run_steps(4);
        // A click on Vox's slot (the first strip); an editor's window takes the focus from
        // SoundCraft's when it opens or is raised, as a real one does.
        let click = |h: &mut egui_kittest::Harness<'static, SoundApp>| {
            h.get_all_by_label("Insert C").next().unwrap().click();
            h.run_steps(2);
            if editor.took_focus() {
                h.event(egui::Event::WindowFocused(false));
                h.run_steps(2);
            }
        };
        click(&mut h);
        assert_eq!((editor.calls(), editor.shown()), (vec!["open"], true));
        // SoundCraft goes to the background (macOS hides the editor), then comes forward with a
        // click on the slot, which lands on the editor shown again: it is raised, not closed.
        h.run_steps(10);
        activated(Duration::ZERO);
        h.event(egui::Event::WindowFocused(true));
        click(&mut h);
        assert_eq!((editor.calls(), editor.shown()), (vec!["open", "open"], true));
        // A click a moment later, SoundCraft's window regaining the focus from the editor's,
        // closes it, as ever.
        activated(Duration::from_secs(1));
        h.event(egui::Event::WindowFocused(true));
        click(&mut h);
        assert_eq!((editor.calls(), editor.shown()), (vec!["open", "open", "close"], false));
    }

    #[test]
    fn slots_are_numbers_letters_or_the_instrument() {
        assert_eq!(slot_param(Some(&json!(0))), Some(0));
        assert_eq!(slot_param(Some(&json!(9))), Some(9));
        assert_eq!(slot_param(Some(&json!("c"))), Some(2));
        assert_eq!(slot_param(Some(&json!(" J "))), Some(9));
        assert_eq!(slot_param(Some(&json!("4"))), Some(4));
        assert_eq!(slot_param(Some(&json!("Instrument"))), Some(INSTRUMENT_SLOT));
        for bad in [json!(10), json!("k"), json!(-1), json!(u64::MAX), json!(""), json!("ab"), json!(1.5), json!(null), json!([0])] {
            assert_eq!(slot_param(Some(&bad)), None, "{bad}");
        }
        assert_eq!(slot_param(None), None);
    }
}
