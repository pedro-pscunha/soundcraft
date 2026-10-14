//! `AuPlugin`: a loaded Audio Unit behind the safe `soundcraft_dsp::Plugin` trait.

use crate::ffi::{Edits, Instance, UnitLink};
use soundcraft_dsp::{Plugin, PluginEditor, PluginInfo, RenderMode, Transport};
use std::sync::Arc;

/// Most MIDI events queued between two blocks (extra events are dropped).
const EVENT_CAP: usize = 1024;
/// Most channels a host strip may ask for.
const MAX_CHANNELS: usize = 64;

/// A queued MIDI channel message at a frame offset within the next `process` call.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MidiEvent {
    time: u32,
    status: u8,
    data1: u8,
    data2: u8,
}

/// A hosted Audio Unit. Parameter ids are global-scope `AudioUnitParameterID`s as decimal
/// strings; values are the unit's own values, with names and ranges in `info().params`.
pub struct AuPlugin {
    inst: Instance,
    info: &'static PluginInfo,
    /// Audio Unit id of each `info.params` entry (same order).
    ids: Vec<u32>,
    values: Vec<f32>,
    /// Pending MIDI, kept sorted by time. Never grows past `EVENT_CAP`.
    events: Vec<MidiEvent>,
    held: [bool; 128],
    /// Takes audio input (effects); instruments replace the buffer contents.
    has_input: bool,
    /// Accepts MIDI (instruments and music effects).
    takes_midi: bool,
    /// The mode to render in from the next `prepare` on (see `Plugin::set_render_mode`).
    mode: RenderMode,
    /// Already logged that the unit refused `mode`.
    mode_warned: bool,
    channels: usize,
    latency: usize,
    tail: usize,
    /// Parameter edits made in the unit's view, taken by its editor handles.
    edits: Arc<Edits>,
}

impl AuPlugin {
    pub(crate) fn new(inst: Instance, info: &'static PluginInfo, has_input: bool, takes_midi: bool) -> AuPlugin {
        let ids: Vec<u32> = info.params.iter().map(|p| p.id.parse::<u32>().unwrap_or(u32::MAX)).collect();
        let values: Vec<f32> = info.params.iter().zip(&ids).map(|(p, &id)| inst.get_param(id).map_or(p.default, |v| p.clamp(v))).collect();
        AuPlugin {
            edits: Arc::new(Edits::new(&ids, &values)),
            inst,
            info,
            ids,
            values,
            events: Vec::with_capacity(EVENT_CAP),
            held: [false; 128],
            has_input,
            takes_midi,
            mode: RenderMode::Realtime,
            mode_warned: false,
            channels: 2,
            latency: 0,
            tail: 0,
        }
    }

    /// The size in pixels of the unit's view (its own, or the generic one): the open editor's, or
    /// one made in a window that is never shown and closed at once. Main thread only; for
    /// diagnostics and tests.
    pub fn editor_size(&self) -> Result<(u32, u32), String> {
        self.inst.link().editor_size()
    }

    /// True when the unit publishes a Cocoa view of its own; otherwise its editor is Apple's
    /// generic view.
    pub fn has_custom_editor(&self) -> bool {
        self.inst.link().has_custom_editor()
    }

    /// Like [`Plugin::open_editor`], but the window is never ordered on screen: the view is made,
    /// attached and listened to as for a shown one. For tests and diagnostics.
    pub fn open_editor_hidden(&mut self) -> Result<(), String> {
        self.inst.link().open_editor(self.info.name, &self.ids, &self.edits, false)
    }

    /// Re-reads every parameter value from the unit (after a state load).
    fn refresh_values(&mut self) {
        self.edits.forget_reported();
        for (i, ((slot, p), &id)) in self.values.iter_mut().zip(self.info.params).zip(&self.ids).enumerate() {
            if let Some(v) = self.inst.get_param(id) {
                *slot = p.clamp(v);
                self.edits.know(i, *slot);
            }
        }
    }

    /// Inserts keeping the queue time-sorted; drops the event when the queue is full (never
    /// allocates on the audio thread).
    fn push_event(&mut self, e: MidiEvent) {
        if !self.takes_midi || self.events.len() >= self.events.capacity() {
            return;
        }
        let pos = self.events.iter().rposition(|q| q.time <= e.time).map_or(0, |i| i + 1);
        self.events.insert(pos, e);
    }
}

impl Plugin for AuPlugin {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { f64::from(sample_rate) } else { 48_000.0 };
        self.channels = channels.clamp(1, MAX_CHANNELS);
        match self.inst.configure(sr, max_block, self.channels, self.mode) {
            Ok(_) => {
                self.latency = self.inst.latency();
                self.tail = self.inst.tail();
            }
            Err(e) => {
                log::warn!("{}: {e}", self.info.id);
                self.latency = 0;
                self.tail = 0;
            }
        }
        // A unit without an offline mode renders the same either way: nothing to tell.
        if let Some(actual) = self.inst.render_mode()
            && actual != self.mode
            && !self.mode_warned
        {
            self.mode_warned = true;
            log::warn!("{}: renders in its {actual} mode, not {} (it refused the OfflineRender property)", self.info.id, self.mode);
        }
        // Nothing is re-sent: a unit keeps its parameters across re-initialization, and `values`
        // only knows what the host set, so re-sending it would replay stale values over changes
        // made in the unit's own view or by the unit itself. The session's overrides reach the
        // instance through `set_param` (the mixer applies them before the first block).
    }

    fn reset(&mut self) {
        self.events.clear();
        self.held = [false; 128];
        self.inst.reset();
    }

    fn set_render_mode(&mut self, mode: RenderMode) {
        self.mode = mode;
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(i) = self.info.params.iter().position(|p| p.id == id) else { return false };
        let (Some(pi), Some(&aid)) = (self.info.params.get(i), self.ids.get(i)) else { return false };
        let v = pi.clamp(value);
        if let Some(slot) = self.values.get_mut(i) {
            *slot = v;
        }
        // The session sending back the view's own edit: by now a drag may have moved the unit on,
        // and setting it would step the unit back until the next echo.
        let echo = self.edits.take_echo(i, v);
        self.edits.know(i, v);
        if !echo {
            self.inst.set_param(aid, v);
        }
        true
    }

    fn param(&self, id: &str) -> Option<f32> {
        let i = self.info.params.iter().position(|p| p.id == id)?;
        self.values.get(i).copied()
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.channels.min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        if frames == 0 || ch == 0 {
            return;
        }
        if !self.inst.is_ready() {
            // Effects pass through; instruments are silent.
            if !self.has_input {
                for c in io.iter_mut().take(ch) {
                    c.iter_mut().take(frames).for_each(|s| *s = 0.0);
                }
            }
            self.events.clear();
            return;
        }
        let max = self.inst.max_frames();
        let au_ch = self.inst.channels();
        let last = u32::try_from(frames - 1).unwrap_or(u32::MAX);
        for e in &mut self.events {
            e.time = e.time.min(last);
        }
        let mut start = 0usize;
        while start < frames {
            let n = (frames - start).min(max);
            if self.has_input {
                for c in 0..au_ch {
                    let src = io.get(c.min(ch - 1)).and_then(|s| s.get(start..start + n));
                    if let (Some(src), Some(dst)) = (src, self.inst.input_mut(c).and_then(|d| d.get_mut(..n))) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            let end = start + n;
            let k = self.events.iter().position(|e| e.time as usize >= end).unwrap_or(self.events.len());
            for e in self.events.iter().take(k) {
                let offset = u32::try_from((e.time as usize).saturating_sub(start)).unwrap_or(0);
                self.inst.midi(e.status, e.data1, e.data2, offset);
            }
            self.events.drain(..k);
            match self.inst.render(n) {
                Ok(()) => {
                    for (c, dst) in io.iter_mut().take(ch).enumerate() {
                        let Some(dst) = dst.get_mut(start..end) else { continue };
                        if ch == 1 && au_ch >= 2 {
                            if let (Some(l), Some(r)) = (self.inst.output(0, n), self.inst.output(1, n)) {
                                for ((d, a), b) in dst.iter_mut().zip(l).zip(r) {
                                    *d = 0.5 * (a + b);
                                }
                            }
                        } else if let Some(src) = self.inst.output(c.min(au_ch.saturating_sub(1)), n) {
                            for (d, &s) in dst.iter_mut().zip(src) {
                                *d = s;
                            }
                        }
                    }
                }
                Err(e) => {
                    log::debug!("{}: render failed ({e})", self.info.id);
                    if !self.has_input {
                        for c in io.iter_mut().take(ch) {
                            if let Some(d) = c.get_mut(start..end) {
                                d.iter_mut().for_each(|s| *s = 0.0);
                            }
                        }
                    }
                }
            }
            start = end;
        }
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn set_transport(&mut self, transport: &Transport) {
        self.inst.set_transport(transport);
    }

    fn tail_samples(&self) -> usize {
        self.tail
    }

    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        if note > 127 {
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        // Velocity 0 would be a note-off.
        self.push_event(MidiEvent { time, status: 0x90, data1: note, data2: velocity.clamp(1, 127) });
        if let Some(h) = self.held.get_mut(note as usize) {
            *h = true;
        }
    }

    fn note_off(&mut self, offset: usize, note: u8) {
        if note > 127 {
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        self.push_event(MidiEvent { time, status: 0x80, data1: note, data2: 0 });
        if let Some(h) = self.held.get_mut(note as usize) {
            *h = false;
        }
    }

    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        let [status, data1, data2] = msg;
        if status & 0xF0 >= 0xA0 && status & 0xF0 < 0xF0 {
            self.push_event(MidiEvent { time: u32::try_from(offset).unwrap_or(u32::MAX), status, data1: data1 & 0x7F, data2: data2 & 0x7F });
        }
    }

    fn all_notes_off(&mut self) {
        for k in 0..128u8 {
            if self.held.get(k as usize).copied().unwrap_or(false) {
                self.note_off(0, k);
            }
        }
        // CC 123: All Notes Off (catches anything the unit still sustains).
        self.push_event(MidiEvent { time: 0, status: 0xB0, data1: 123, data2: 0 });
    }

    fn load_state(&mut self, data: &[u8]) -> bool {
        let ok = self.inst.load_state(data);
        if ok {
            self.refresh_values();
        }
        ok
    }

    fn open_editor(&mut self) -> Result<(), String> {
        self.inst.link().open_editor(self.info.name, &self.ids, &self.edits, true)
    }

    fn close_editor(&mut self) {
        self.inst.link().close_editor();
    }

    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        let link = Arc::clone(self.inst.link());
        Some(Box::new(AuEditor { link, edits: Arc::clone(&self.edits), info: self.info, ids: self.ids.clone() }))
    }

    fn state_reader(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginStateReader>> {
        let link = Arc::clone(self.inst.link());
        Some(Box::new(move || link.save_state()))
    }
}

/// An Audio Unit's editor (its view in a host window), driven from the main thread.
struct AuEditor {
    link: Arc<UnitLink>,
    edits: Arc<Edits>,
    info: &'static PluginInfo,
    ids: Vec<u32>,
}

impl PluginEditor for AuEditor {
    /// The same path as [`Plugin::open_editor`]. Edits not yet taken are kept: the listener only
    /// exists while a window is open, so they were made in the last one (in the frame it closed)
    /// and are no less real; values the host already knows are left out at `idle`.
    fn open(&mut self) -> Result<(), String> {
        self.link.open_editor(self.info.name, &self.ids, &self.edits, true)
    }

    fn close(&mut self) {
        self.link.close_editor();
    }

    fn is_open(&self) -> bool {
        self.link.editor_open()
    }

    /// The view's edits since the last call, leaving out values the host already knows: a preset
    /// loaded in the view notifies every parameter, the unchanged ones too (see `Edits`).
    fn idle(&mut self) -> Vec<(String, f32)> {
        self.link.editor_idle();
        let mut out = Vec::new();
        for (i, v) in self.edits.take() {
            let Some(p) = self.info.params.get(i) else { continue };
            let v = p.clamp(v);
            if self.edits.known(i) != Some(v) {
                self.edits.know(i, v);
                self.edits.reported(i, v);
                out.push((p.id.to_string(), v));
            }
        }
        out
    }

    fn set_param(&mut self, id: &str, value: f32) {
        let Some(i) = self.info.params.iter().position(|p| p.id == id) else { return };
        if let (Some(p), Some(&aid)) = (self.info.params.get(i), self.ids.get(i)) {
            let v = p.clamp(value);
            self.edits.know(i, v);
            self.link.show_param(aid, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `prepare` re-initializes the unit without re-sending the values the host last set: the
    /// unit keeps them itself, and a change the host never made (in the unit's own view, here
    /// through the same `AudioUnitSetParameter` its view's `AUParameterSet` calls) used to be
    /// overwritten with the host's stale value. AUDelay ships with macOS; skipped where missing.
    #[test]
    fn prepare_keeps_the_values_the_unit_has() {
        let Ok(mut p) = crate::instantiate_plugin("au:aufx:dely:appl") else { return };
        let (Some(host), Some(view)) = (p.info.params.first(), p.info.params.get(1)) else { panic!("AUDelay has parameters") };
        let (Some(&host_id), Some(&view_id)) = (p.ids.first(), p.ids.get(1)) else { panic!("ids") };
        let mid = |q: &soundcraft_dsp::ParamInfo, f: f32| q.min + (q.max - q.min) * f;
        let (host_value, view_value) = (mid(host, 0.3), mid(view, 0.7));
        assert!(p.set_param(host.id, host_value));
        assert_ne!(p.param(view.id), Some(view_value));
        assert!(p.inst.set_param(view_id, view_value), "an edit in the unit's view");
        for (sr, block) in [(96_000.0, 256), (44_100.0, 1024)] {
            p.prepare(sr, block, 2);
            assert!(p.inst.is_ready());
            assert_eq!(p.inst.get_param(host_id), Some(host_value), "{sr}: the unit keeps what the host set");
            assert_eq!(p.inst.get_param(view_id), Some(view_value), "{sr}: and what its view set");
        }
    }

    /// A value set in the unit's view (with `AUParameterSet`, as views do) reaches the host
    /// through the editor's `idle`, once and with the latest value. The host's own changes (its
    /// `set_param`, the editor's `set_param`, a state load) are no edits. The state read for
    /// saving keeps the view's value too: an instrument's edits reach the session that way. The
    /// view itself is covered by `tests/editor_main_thread.rs`. AUDelay ships with macOS; skipped
    /// where missing.
    #[test]
    fn view_edits_reach_the_editor_and_the_saved_state() {
        let Ok(mut p) = crate::instantiate_plugin("au:aufx:dely:appl") else { return };
        let mut ed = p.editor().expect("every unit has an editor");
        let (Some(mix), Some(time)) = (p.info.params.first(), p.info.params.get(1)) else { panic!("AUDelay has parameters") };
        let (Some(&mix_id), Some(&time_id)) = (p.ids.first(), p.ids.get(1)) else { panic!("ids") };
        // What `open_editor` attaches next to the window.
        p.inst.link().listen(&p.ids, &p.edits);
        assert!(ed.idle().is_empty());
        assert!(p.inst.set_as_view(time_id, 1.25));
        assert!(p.inst.set_as_view(time_id, 1.5));
        assert_eq!(ed.idle(), vec![(time.id.to_string(), 1.5)]);
        assert!(ed.idle().is_empty(), "each edit is reported once");

        assert!(p.set_param(mix.id, 30.0));
        ed.set_param(mix.id, 40.0);
        assert_eq!(p.inst.get_param(mix_id), Some(40.0), "the editor's value reaches the unit (and its view)");
        let state = p.save_state().expect("a state");
        assert!(p.load_state(&state));
        assert!(ed.idle().is_empty(), "the host's own changes are no edits");

        let mut q = crate::instantiate_plugin("au:aufx:dely:appl").expect("a second instance");
        assert!(q.load_state(&state));
        assert_eq!(q.param(time.id), Some(1.5), "the saved state keeps the view's edit");

        drop(p);
        assert!(ed.idle().is_empty());
        ed.set_param(mix.id, 50.0);
        assert!(!ed.is_open());
    }

    /// A view that loads a preset changes the unit's values without a notice per parameter and
    /// then tells listeners that any parameter may have changed, which reaches the editor's
    /// listener as a call for every parameter with its current value. The editor's idle reports
    /// only the values that differ from what the host knows: here the delay time the "preset"
    /// changed, not the dry/wet mix the host set itself nor the untouched rest. AUDelay ships with
    /// macOS; skipped where missing.
    #[test]
    fn a_preset_loaded_in_the_view_reaches_the_editor() {
        let Ok(mut p) = crate::instantiate_plugin("au:aufx:dely:appl") else { return };
        let mut ed = p.editor().expect("every unit has an editor");
        let (Some(mix), Some(time)) = (p.info.params.first(), p.info.params.get(1)) else { panic!("AUDelay has parameters") };
        let (Some(&mix_id), Some(&time_id)) = (p.ids.first(), p.ids.get(1)) else { panic!("ids") };
        p.inst.link().listen(&p.ids, &p.edits);
        assert!(p.set_param(mix.id, 30.0), "the host's own value");
        ed.set_param(mix.id, 35.0);
        // The "preset": values changed behind every listener's back, then one notice.
        assert!(p.inst.set_param(time_id, 1.5));
        assert!(ed.idle().is_empty(), "no notice yet");
        p.inst.notify_any();
        let mut got = Vec::new();
        for _ in 0..200 {
            got = ed.idle();
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(got, vec![(time.id.to_string(), 1.5)], "only what the preset changed");
        assert_eq!(p.inst.get_param(mix_id), Some(35.0));
        assert!(ed.idle().is_empty(), "reported once");
        // The host's own changes are known: a notice after them reports nothing.
        assert!(p.set_param(time.id, 0.5));
        p.inst.notify_any();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(ed.idle().is_empty());
    }

    /// Edits made in the view's last frame are reported after the window closed, also when the
    /// editor is opened again before the next idle (that open used to drop them as stale).
    /// AUDelay ships with macOS; skipped where missing.
    #[test]
    fn the_last_edits_of_a_closed_view_survive_opening_it_again() {
        let Ok(mut p) = crate::instantiate_plugin("au:aufx:dely:appl") else { return };
        let mut ed = p.editor().expect("every unit has an editor");
        let Some(time) = p.info.params.get(1) else { panic!("AUDelay has parameters") };
        let Some(&time_id) = p.ids.get(1) else { panic!("ids") };
        p.inst.link().listen(&p.ids, &p.edits);
        assert!(p.inst.set_as_view(time_id, 0.75), "an edit in the window's last frame");
        ed.close();
        // Off the main thread no window opens, but the open path runs up to that point.
        assert!(ed.open().is_err());
        assert_eq!(ed.idle(), vec![(time.id.to_string(), 0.75)]);
    }

    /// A view's edit comes back as the host's `set_param` once the session holds it (the mixer
    /// applies new values on the audio thread). A drag may have moved the view on by then: the
    /// echo must not set the unit back. Only the first set after a report can be its echo.
    /// AUDelay ships with macOS; skipped where missing.
    #[test]
    fn a_views_edit_echoed_back_does_not_step_the_unit_back() {
        let Ok(mut p) = crate::instantiate_plugin("au:aufx:dely:appl") else { return };
        let mut ed = p.editor().expect("every unit has an editor");
        let Some(time) = p.info.params.get(1) else { panic!("AUDelay has parameters") };
        let Some(&time_id) = p.ids.get(1) else { panic!("ids") };
        p.inst.link().listen(&p.ids, &p.edits);
        assert!(p.inst.set_as_view(time_id, 0.75));
        assert_eq!(ed.idle(), vec![(time.id.to_string(), 0.75)]);
        assert!(p.inst.set_as_view(time_id, 1.25), "the drag goes on");
        assert!(p.set_param(time.id, 0.75), "the echo of the first edit");
        assert_eq!(p.inst.get_param(time_id), Some(1.25), "the unit keeps the view's newer value");
        assert_eq!(ed.idle(), vec![(time.id.to_string(), 1.25)]);
        assert!(p.set_param(time.id, 1.25));
        assert_eq!(p.inst.get_param(time_id), Some(1.25));
        // Taken once: the same value set again later reaches the unit.
        assert!(p.inst.set_param(time_id, 0.5));
        assert!(p.set_param(time.id, 1.25));
        assert_eq!(p.inst.get_param(time_id), Some(1.25));
    }

    /// The mode reaches a unit that has one (AURoundTripAAC reports its `OfflineRender`
    /// property); AUDelay has none, so there is nothing it could refuse (and nothing to log). Both
    /// ship with macOS; skipped where missing.
    #[test]
    fn the_render_mode_reaches_units_that_have_one() {
        let Ok(mut aac) = crate::instantiate_plugin("au:aufx:raac:appl") else { return };
        aac.prepare(48_000.0, 512, 2);
        assert_eq!(aac.inst.unit_reports_offline(), Some(false), "real time by default");
        aac.set_render_mode(RenderMode::Offline);
        assert_eq!(aac.inst.unit_reports_offline(), Some(false), "a change takes effect at the next prepare");
        aac.prepare(48_000.0, 512, 2);
        assert_eq!(aac.inst.unit_reports_offline(), Some(true));
        assert_eq!(aac.inst.render_mode(), Some(RenderMode::Offline));
        aac.set_render_mode(RenderMode::Realtime);
        aac.prepare(48_000.0, 512, 2);
        assert_eq!(aac.inst.unit_reports_offline(), Some(false), "back in real time");
        let Ok(mut delay) = crate::instantiate_plugin("au:aufx:dely:appl") else { return };
        delay.set_render_mode(RenderMode::Offline);
        delay.prepare(48_000.0, 512, 2);
        assert_eq!(delay.inst.unit_reports_offline(), None);
        assert_eq!(delay.inst.render_mode(), None, "no offline mode: nothing refused");
    }
}
