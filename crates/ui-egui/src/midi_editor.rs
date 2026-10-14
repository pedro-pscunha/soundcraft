//! The MIDI Editor: a piano roll with a velocity lane, docked under the Edit window.

use crate::SoundApp;
use crate::theme::{Tokens, bold, regular};
use egui::{Align2, Color32, Rangef, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde_json::json;
use soundcraft_model::{ClipContent, ClipId, Tool};
use soundcraft_time::{Range, SampleRate, Samples, TICKS_PER_QUARTER, TempoMap};

const KEY_W: f32 = 48.0;
const ROW_H: f32 = 9.0;
const VEL_H: f32 = 64.0;

#[derive(Debug, Clone, Default)]
pub struct MidiEditorState {
    pub selected: Vec<usize>,
    pub drag: Option<NoteDrag>,
    pub scroll: f32,
    pub clip: Option<ClipId>,
    /// Rubber-band selection start (screen position).
    pub band: Option<egui::Pos2>,
}

#[derive(Debug, Clone)]
pub struct NoteDrag {
    pub index: usize,
    pub resize: bool,
    pub start: egui::Pos2,
    pub dx_ticks: i64,
    pub dy_semi: i64,
}

/// The MIDI clip being edited: the first selected MIDI clip, else a MIDI clip under the insertion
/// point on a selected track.
pub fn target_clip(app: &SoundApp) -> Option<ClipId> {
    let s = app.engine.session();
    for id in &s.edit.selected_clips {
        if let Some((_, c)) = s.find_clip(*id)
            && matches!(c.content, ClipContent::Midi { .. })
        {
            return Some(*id);
        }
    }
    let at = s.edit.selection.start;
    s.tracks
        .iter()
        .filter(|t| s.edit.selected_tracks.contains(&t.id) && t.kind.is_midi())
        .flat_map(|t| t.clips().iter())
        .find(|c| c.range().contains(at) || c.range().overlaps(&s.edit.selection))
        .or_else(|| s.tracks.iter().filter(|t| s.edit.selected_tracks.contains(&t.id) && t.kind.is_midi()).flat_map(|t| t.clips().iter()).next())
        .map(|c| c.id)
}

fn is_black(p: i32) -> bool {
    matches!(p.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// The roll's x for the session position `pos`, or `None` outside the edited clip's samples
/// `clip`, which is all the roll shows. The roll fits the clip to its width `roll`: the clip's
/// first tick `base_tick` sits at its left edge and each tick advances `px_per_tick` (`x_of` in
/// [`show`]). `pos` goes to ticks on the tempo map, the conversion playback uses to schedule the
/// clip's notes, so the line meets a note as it sounds. The clip's tick span is rounded, so its
/// edges can land a fraction of a tick off the roll: they stay on it.
fn playhead_x(tempo: &TempoMap, sr: SampleRate, pos: Samples, clip: Range, base_tick: i64, roll: Rangef, px_per_tick: f32) -> Option<f32> {
    if pos < clip.start || pos > clip.end {
        return None;
    }
    let tick = tempo.samples_to_ticks_f(pos, sr) - base_tick as f64;
    let x = (f64::from(roll.min) + tick * f64::from(px_per_tick)) as f32;
    x.is_finite().then(|| x.max(roll.min).min(roll.max))
}

pub fn show(app: &mut SoundApp, ui: &mut Ui) {
    // Painter-only contents do not advance the layout. Claim the resized
    // space, including the empty state, so the dock keeps its chosen height.
    ui.take_available_space();
    let t = Tokens::current();
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, t.panel_bg);
    let header = Rect::from_min_size(full.min, vec2(full.width(), 22.0));
    ui.painter().rect_filled(header, 0.0, t.panel_bg2);
    ui.painter().text(pos2(header.min.x + 8.0, header.center().y), Align2::LEFT_CENTER, "MIDI EDITOR", bold(11.5), t.header_text);
    let Some(cid) = target_clip(app) else {
        let track = {
            let s = app.engine.session();
            s.tracks.iter().find(|t| t.kind.is_midi() && s.edit.selected_tracks.contains(&t.id)).map(|t| (t.id, t.name.clone()))
        };
        let msg = match &track {
            Some((_, name)) => format!("Click to add a MIDI clip to {name} at the insertion point."),
            None => "Select a MIDI clip (or a MIDI track) to edit its notes.".into(),
        };
        ui.painter().text(full.center(), Align2::CENTER_CENTER, msg, regular(13.0), t.text_dim);
        let body = Rect::from_min_max(pos2(full.min.x, header.max.y), full.max);
        if let Some((id, _)) = track
            && ui.interact(body, ui.id().with("new_clip"), Sense::click()).clicked()
        {
            let sel = app.engine.session().edit.selection;
            if let Ok(v) = app.run("midi.new_clip", json!({"track": id.0, "start": sel.start, "end": sel.end})) {
                let _ = app.run("edit.select", json!({"clips": [v["clip"]]}));
            }
        }
        return;
    };
    let Some((track, clip)) = app.engine.session().find_clip(cid).map(|(t, c)| (t, c.clone())) else { return };
    let ClipContent::Midi { sequence } = &clip.content else { return };
    let track_color = app.engine.session().track(track).map_or([120, 120, 200], |t| t.color);
    ui.painter().text(
        pos2(header.min.x + 110.0, header.center().y),
        Align2::LEFT_CENTER,
        format!(
            "{} · {} notes · {} selected · Pencil: add · drag: move/select · arrows: move · Delete: remove",
            clip.name,
            sequence.notes.len(),
            app.midi.selected.len()
        ),
        regular(11.0),
        t.text_dim,
    );
    let grid_ticks = match app.engine.session().edit.grid {
        soundcraft_time::GridValue::Note { value, dotted, triplet } => {
            let mut g = value.ticks();
            if dotted {
                g = g * 3 / 2;
            }
            if triplet {
                g = g * 2 / 3;
            }
            g.max(1)
        }
        _ => TICKS_PER_QUARTER / 4,
    };
    // Header toolbar: whole-clip MIDI operations.
    let bar = Rect::from_min_max(pos2(header.max.x - 330.0, header.min.y + 1.0), pos2(header.max.x - 6.0, header.max.y - 1.0));
    let mut tb = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(egui::Layout::right_to_left(egui::Align::Center)));
    tb.spacing_mut().item_spacing.x = 4.0;
    let mut op: Option<(&str, serde_json::Value)> = None;
    if tb.small_button("Legato").clicked() {
        op = Some(("event.change_duration", json!({"legato": 0})));
    }
    if tb.small_button("Vel −10").clicked() {
        op = Some(("event.change_velocity", json!({"add": -10})));
    }
    if tb.small_button("Vel +10").clicked() {
        op = Some(("event.change_velocity", json!({"add": 10})));
    }
    if tb.small_button("Quantize").on_hover_text("Quantize to the grid").clicked() {
        op = Some(("event.quantize", json!({"grid": grid_ticks, "strength": 100})));
    }
    if let Some((id, mut p)) = op {
        p["clips"] = json!([cid.0]);
        let _ = app.run(id, p);
    }
    let s = app.engine.session_arc();
    let sr = s.sample_rate;
    let base_tick = s.tempo.samples_to_ticks(clip.start, sr);
    let len_ticks = (s.tempo.samples_to_ticks(clip.end(), sr) - base_tick).max(1);
    let roll = Rect::from_min_max(pos2(full.min.x + KEY_W, header.max.y), pos2(full.max.x - 4.0, full.max.y - VEL_H - 4.0));
    let keys = Rect::from_min_max(pos2(full.min.x, roll.min.y), pos2(roll.min.x, roll.max.y));
    let vel = Rect::from_min_max(pos2(roll.min.x, roll.max.y + 4.0), pos2(roll.max.x, full.max.y - 2.0));
    // Vertical range: centre on the notes the first time.
    let rows = (roll.height() / ROW_H).floor() as i32;
    if app.midi.clip != Some(cid) {
        let hi = sequence.notes.iter().map(|n| i32::from(n.pitch)).max().unwrap_or(72);
        let lo = sequence.notes.iter().map(|n| i32::from(n.pitch)).min().unwrap_or(48);
        app.midi.scroll = (127 - (hi + lo) / 2 - rows / 2) as f32 * ROW_H;
        app.midi.clip = Some(cid);
    }
    if ui.rect_contains_pointer(roll) {
        app.midi.scroll -= ui.input(|i| i.smooth_scroll_delta.y);
    }
    app.midi.scroll = app.midi.scroll.clamp(0.0, (128.0 * ROW_H - roll.height()).max(0.0));
    let scroll = app.midi.scroll;
    let px_per_tick = roll.width() / len_ticks as f32;
    let x_of = |tick: i64| roll.min.x + tick as f32 * px_per_tick;
    let y_of = |pitch: i32| roll.min.y + (127 - pitch) as f32 * ROW_H - scroll;
    let pitch_at = |y: f32| 127 - ((y - roll.min.y + scroll) / ROW_H).floor() as i32;
    let painter = ui.painter().with_clip_rect(roll.union(keys));
    // Background rows and keys.
    let top = pitch_at(roll.min.y);
    for r in 0..=rows + 1 {
        let pitch = top - r;
        let y = y_of(pitch);
        let row = Rect::from_min_size(pos2(roll.min.x, y), vec2(roll.width(), ROW_H));
        painter.rect_filled(row, 0.0, if is_black(pitch) { Color32::from_rgb(30, 30, 32) } else { Color32::from_rgb(40, 40, 42) });
        let key = Rect::from_min_size(pos2(keys.min.x, y), vec2(KEY_W - 2.0, ROW_H - 1.0));
        painter.rect_filled(key, 1.0, if is_black(pitch) { Color32::from_rgb(20, 20, 20) } else { Color32::from_rgb(225, 225, 225) });
        if pitch.rem_euclid(12) == 0 && (0..=127).contains(&pitch) {
            painter.text(
                pos2(key.max.x - 3.0, key.center().y),
                Align2::RIGHT_CENTER,
                soundcraft_midi::note_name(pitch as u8),
                regular(8.0),
                Color32::from_rgb(40, 40, 40),
            );
            painter.line_segment([pos2(roll.min.x, y + ROW_H), pos2(roll.max.x, y + ROW_H)], Stroke::new(1.0, Color32::from_rgb(64, 64, 68)));
        }
    }
    // Grid.
    let mut tick = 0;
    while tick <= len_ticks {
        let bar = tick % (TICKS_PER_QUARTER * 4) == 0;
        let x = x_of(tick);
        if grid_ticks as f32 * px_per_tick > 4.0 || bar {
            painter.line_segment(
                [pos2(x, roll.min.y), pos2(x, roll.max.y)],
                Stroke::new(1.0, if bar { Color32::from_rgb(80, 80, 86) } else { Color32::from_rgb(50, 50, 54) }),
            );
        }
        tick += grid_ticks;
    }
    // Notes.
    let base = egui::Color32::from_rgb(track_color[0], track_color[1], track_color[2]);
    let drag = app.midi.drag.clone();
    for (i, n) in sequence.notes.iter().enumerate() {
        let (mut st, mut ln, mut p) = (n.start, n.length, i32::from(n.pitch));
        if let Some(d) = &drag
            && (d.index == i || (!d.resize && app.midi.selected.contains(&i) && app.midi.selected.contains(&d.index)))
        {
            if d.resize {
                if d.index == i {
                    ln = (ln + d.dx_ticks).max(grid_ticks.min(ln).max(1));
                }
            } else {
                st = (st + d.dx_ticks).max(0);
                p = (p + d.dy_semi as i32).clamp(0, 127);
            }
        }
        let r = Rect::from_min_max(pos2(x_of(st), y_of(p) + 1.0), pos2(x_of(st + ln).max(x_of(st) + 3.0), y_of(p) + ROW_H - 1.0));
        let sel = app.midi.selected.contains(&i);
        let k = 0.45 + 0.55 * f32::from(n.velocity) / 127.0;
        let fill = if sel {
            Color32::WHITE
        } else {
            Color32::from_rgb((f32::from(base.r()) * k) as u8, (f32::from(base.g()) * k) as u8, (f32::from(base.b()) * k) as u8)
        };
        painter.rect(r, 2.0, fill, Stroke::new(1.0, Color32::BLACK), StrokeKind::Inside);
    }
    // Interaction.
    let resp = ui.interact(roll, ui.id().with(("roll", cid.0)), Sense::click_and_drag());
    let hit = |pos: egui::Pos2| -> Option<(usize, bool)> {
        sequence.notes.iter().enumerate().rev().find_map(|(i, n)| {
            let r = Rect::from_min_max(
                pos2(x_of(n.start), y_of(i32::from(n.pitch))),
                pos2(x_of(n.start + n.length).max(x_of(n.start) + 3.0), y_of(i32::from(n.pitch)) + ROW_H),
            );
            r.contains(pos).then_some((i, pos.x > r.max.x - 5.0))
        })
    };
    let snap = |tk: f32| -> i64 { ((tk / grid_ticks as f32).round() as i64) * grid_ticks };
    if resp.drag_started()
        && let Some(p) = resp.interact_pointer_pos()
    {
        if let Some((i, edge)) = hit(p) {
            if !app.midi.selected.contains(&i) {
                app.midi.selected = vec![i];
            }
            app.midi.drag = Some(NoteDrag { index: i, resize: edge, start: p, dx_ticks: 0, dy_semi: 0 });
        } else {
            app.midi.band = Some(p);
        }
    }
    // Rubber-band selection on empty space.
    if let (Some(a), Some(b)) = (app.midi.band, ui.ctx().pointer_latest_pos()) {
        let band = Rect::from_two_pos(a, b);
        ui.painter().rect(
            band,
            0.0,
            Color32::from_rgba_unmultiplied(120, 170, 255, 40),
            Stroke::new(1.0, Color32::from_rgb(120, 170, 255)),
            StrokeKind::Inside,
        );
        if resp.drag_stopped() {
            app.midi.selected = sequence
                .notes
                .iter()
                .enumerate()
                .filter(|(_, n)| {
                    let r = Rect::from_min_max(
                        pos2(x_of(n.start), y_of(i32::from(n.pitch))),
                        pos2(x_of(n.start + n.length), y_of(i32::from(n.pitch)) + ROW_H),
                    );
                    r.intersects(band)
                })
                .map(|(i, _)| i)
                .collect();
            app.midi.band = None;
        }
    }
    if resp.dragged()
        && let (Some(d), Some(p)) = (&mut app.midi.drag, resp.interact_pointer_pos())
    {
        d.dx_ticks = snap((p.x - d.start.x) / px_per_tick);
        d.dy_semi = -((p.y - d.start.y) / ROW_H).round() as i64;
    }
    if resp.drag_stopped()
        && let Some(d) = app.midi.drag.take()
    {
        if d.resize {
            if let Some(n) = sequence.notes.get(d.index) {
                let _ = app.run("midi.note_edit", json!({"clip": cid.0, "index": d.index, "length_ticks": (n.length + d.dx_ticks).max(1)}));
            }
        } else if d.dx_ticks != 0 || d.dy_semi != 0 {
            let _ = app.run("midi.notes_move", json!({"clip": cid.0, "indices": app.midi.selected, "ticks": d.dx_ticks, "semitones": d.dy_semi}));
            app.midi.selected.clear();
        }
    }
    if resp.clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        match hit(p) {
            Some((i, _)) => {
                if ui.input(|x| x.modifiers.shift) {
                    if !app.midi.selected.contains(&i) {
                        app.midi.selected.push(i);
                    }
                } else {
                    app.midi.selected = vec![i];
                }
                // Audition on the instrument would go here (needs a live MIDI input path).
            }
            None if s.edit.tool == Tool::Pencil => {
                let pitch = pitch_at(p.y).clamp(0, 127);
                let start = (((p.x - roll.min.x) / px_per_tick) as i64 / grid_ticks) * grid_ticks;
                let _ = app.run(
                    "midi.note_add",
                    json!({"clip": cid.0, "pitch": pitch, "start_ticks": start.max(0), "length_ticks": grid_ticks, "velocity": 100}),
                );
                app.midi.selected.clear();
            }
            None => app.midi.selected.clear(),
        }
    }
    // Keyboard: Cmd+A select all; arrows transpose (Shift = octave) and move by the grid.
    if ui.rect_contains_pointer(roll) && !ui.ctx().egui_wants_keyboard_input() {
        let (all, up, down, left, right, shift) = ui.input(|i| {
            let arrows = !i.modifiers.alt && !i.modifiers.command && !i.modifiers.ctrl;
            (
                i.modifiers.command && i.key_pressed(egui::Key::A),
                arrows && i.key_pressed(egui::Key::ArrowUp),
                arrows && i.key_pressed(egui::Key::ArrowDown),
                arrows && !i.modifiers.shift && i.key_pressed(egui::Key::ArrowLeft),
                arrows && !i.modifiers.shift && i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.shift,
            )
        });
        if all {
            app.midi.selected = (0..sequence.notes.len()).collect();
        }
        let semi = if up {
            1
        } else if down {
            -1
        } else {
            0
        } * if shift { 12 } else { 1 };
        let ticks = if right {
            grid_ticks
        } else if left {
            -grid_ticks
        } else {
            0
        };
        if (semi != 0 || ticks != 0) && !app.midi.selected.is_empty() {
            let _ = app.run("midi.notes_move", json!({"clip": cid.0, "indices": app.midi.selected, "ticks": ticks, "semitones": semi}));
        }
    }
    if !app.midi.selected.is_empty()
        && !ui.ctx().egui_wants_keyboard_input()
        && ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))
    {
        let _ = app.run("midi.note_delete", json!({"clip": cid.0, "indices": app.midi.selected}));
        app.midi.selected.clear();
    }
    // Velocity lane.
    ui.painter().rect_filled(vel, 0.0, Color32::from_rgb(28, 28, 30));
    ui.painter().text(pos2(full.min.x + 6.0, vel.center().y), Align2::LEFT_CENTER, "Velocity", regular(10.0), t.text_dim);
    let vresp = ui.interact(vel, ui.id().with(("vel", cid.0)), Sense::click_and_drag());
    for (i, n) in sequence.notes.iter().enumerate() {
        let x = x_of(n.start);
        let h = (vel.height() - 4.0) * f32::from(n.velocity) / 127.0;
        let col = if app.midi.selected.contains(&i) { Color32::WHITE } else { base };
        ui.painter().line_segment([pos2(x + 1.0, vel.max.y - 2.0), pos2(x + 1.0, vel.max.y - 2.0 - h)], Stroke::new(3.0, col));
        ui.painter().circle_filled(pos2(x + 1.0, vel.max.y - 2.0 - h), 2.5, col);
    }
    if (vresp.clicked() || vresp.dragged())
        && let Some(p) = vresp.interact_pointer_pos()
        && let Some((i, n)) = sequence.notes.iter().enumerate().min_by_key(|(_, n)| ((x_of(n.start) - p.x).abs() * 10.0) as i64)
        && (x_of(n.start) - p.x).abs() < 8.0
    {
        let v = (((vel.max.y - 2.0 - p.y) / (vel.height() - 4.0)) * 127.0).round().clamp(1.0, 127.0) as i64;
        let _ = app.engine.execute_merged("midi.note_edit", &json!({"clip": cid.0, "index": i, "velocity": v}), &format!("vel:{}", cid.0));
    }
    // Over the notes and the velocity lane, as in the Edit window: the playhead (1.5, red) at the
    // play position while playing; when stopped, the insertion line (1.0, light) where playback
    // starts, for an insertion point only (a selected range has none), blinking with the Edit
    // window's.
    let (playing, sel) = (app.is_playing(), s.edit.selection);
    if (playing || sel.is_empty())
        && let Some(x) = playhead_x(&s.tempo, sr, app.position(), clip.range(), base_tick, roll.x_range(), px_per_tick)
    {
        let lanes = Rect::from_min_max(roll.min, pos2(roll.max.x, vel.max.y));
        let stroke = if playing { Stroke::new(1.5, t.playhead) } else { Stroke::new(1.0, t.insertion) };
        if playing || crate::edit_window::insertion_shown(ui.ctx(), sel.start, &s.edit.selected_tracks) {
            ui.painter().with_clip_rect(lanes).line_segment([pos2(x, lanes.min.y), pos2(x, lanes.max.y)], stroke);
        }
        if playing {
            // The roll stretches one clip across its width, so the line can move much faster than
            // the Edit window's (repainted at 30 Hz): keep it smooth at 60 Hz while it is shown.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(1000 / 60));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Shape;
    use egui_kittest::Harness;

    const SR: SampleRate = SampleRate::HZ_48000;
    /// A quarter note at 120 BPM and 48 kHz.
    const BEAT: Samples = 24_000;
    const Q: i64 = TICKS_PER_QUARTER;

    fn near(x: Option<f32>, want: f32) -> bool {
        x.is_some_and(|x| (x - want).abs() < 0.01)
    }

    /// A one-bar clip at bar 3 (two bars of 120 BPM in), as the roll's 800 px from x = 100.
    fn bar_three(tempo: &TempoMap, pos: Samples) -> Option<f32> {
        let clip = Range { start: 8 * BEAT, end: tempo.tick_to_samples(12 * Q, SR) };
        playhead_x(tempo, SR, pos, clip, 8 * Q, Rangef::new(100.0, 900.0), 800.0 / (4 * Q) as f32)
    }

    #[test]
    fn playhead_fits_the_clip_to_the_roll() {
        let tempo = TempoMap::default();
        assert!(near(bar_three(&tempo, 8 * BEAT), 100.0));
        assert!(near(bar_three(&tempo, 10 * BEAT), 500.0));
        assert!(near(bar_three(&tempo, 12 * BEAT), 900.0));
        // Before the clip (the session start too) and past its end, the roll has no place for it.
        for pos in [0, 8 * BEAT - 1, 12 * BEAT + 1, Samples::MIN, Samples::MAX] {
            assert_eq!(bar_three(&tempo, pos), None, "position {pos}");
        }
    }

    #[test]
    fn playhead_follows_a_tempo_change_inside_the_clip() {
        // 60 BPM from the clip's third beat: its second half lasts twice as long.
        let mut tempo = TempoMap::default();
        tempo.set_tempo(10 * Q, 60.0).unwrap();
        // Two beats at 120 BPM, then one at 60 BPM: three beats of four. Mapping samples straight
        // onto the width would say two thirds (633 px).
        assert!(near(bar_three(&tempo, 8 * BEAT + 2 * BEAT + 2 * BEAT), 700.0));
        assert!(near(bar_three(&tempo, 8 * BEAT + 2 * BEAT + 4 * BEAT), 900.0));
        assert_eq!(bar_three(&tempo, 8 * BEAT + 2 * BEAT + 4 * BEAT + 1), None);
    }

    #[test]
    fn playhead_meets_each_note_when_playback_sounds_it() {
        // Tempo changes before and inside a clip that starts off the tick grid, as `show` lays it
        // out: the line reaches a note's left edge at the sample the mixer starts the note.
        let mut tempo = TempoMap::new(93.0, 4, 4).unwrap();
        tempo.set_tempo(3 * Q, 141.0).unwrap();
        tempo.set_tempo(9 * Q + 17, 67.5).unwrap();
        let clip = Range { start: 123_457, end: 1_123_460 };
        let base = tempo.samples_to_ticks(clip.start, SR);
        let len = tempo.samples_to_ticks(clip.end, SR) - base;
        let roll = Rangef::new(48.0, 1282.0);
        let ppt = roll.span() / len as f32;
        for n in [1, Q / 3, 2 * Q + 5, 7 * Q, len - 1] {
            let sounds = tempo.tick_to_samples(base + n, SR);
            let x = playhead_x(&tempo, SR, sounds, clip, base, roll, ppt);
            assert!(x.is_some_and(|x| (x - (roll.min + n as f32 * ppt)).abs() < 0.05), "note at tick {n}: {x:?}");
        }
    }

    #[test]
    fn playhead_keeps_the_clip_edges_on_the_roll() {
        // A clip 10 samples (0.4 tick) wider on each side than its rounded one-bar tick span:
        // its first and last samples would land just off the roll.
        let tempo = TempoMap::default();
        let clip = Range { start: 8 * BEAT - 10, end: 12 * BEAT + 10 };
        let base = tempo.samples_to_ticks(clip.start, SR);
        assert_eq!((base, tempo.samples_to_ticks(clip.end, SR) - base), (8 * Q, 4 * Q));
        let x = |pos| playhead_x(&tempo, SR, pos, clip, base, Rangef::new(100.0, 900.0), 800.0 / (4 * Q) as f32);
        assert_eq!((x(clip.start), x(clip.end)), (Some(100.0), Some(900.0)));
        assert_eq!((x(clip.start - 1), x(clip.end + 1)), (None, None));
    }

    #[test]
    fn playhead_fits_long_and_short_clips_and_refuses_a_broken_axis() {
        let tempo = TempoMap::default();
        // A thousand bars squeezed into 300 px, and a single tick (25 samples) across 1200 px.
        let long = Range { start: 0, end: 4_000 * BEAT };
        let x = playhead_x(&tempo, SR, 2_000 * BEAT, long, 0, Rangef::new(0.0, 300.0), 300.0 / (4_000 * Q) as f32);
        assert!(near(x, 150.0), "{x:?}");
        let short = Range { start: 5 * BEAT, end: 5 * BEAT + BEAT / Q };
        let x = playhead_x(&tempo, SR, 5 * BEAT + 10, short, 5 * Q, Rangef::new(10.0, 1210.0), 1200.0);
        assert!(near(x, 490.0), "{x:?}");
        // A broken scale yields no line rather than a NaN or infinite one.
        for ppt in [f32::NAN, f32::INFINITY] {
            assert_eq!(playhead_x(&tempo, SR, 5 * BEAT + 10, short, 5 * Q, Rangef::new(10.0, 1210.0), ppt), None);
        }
    }

    /// The demo session with the MIDI editor open on the Lead track's clip, the insertion a
    /// quarter of the way into it. Returns the clip's samples too.
    fn lead_in_editor() -> (Harness<'static, SoundApp>, Range) {
        let mut engine = soundcraft_engine::demo::demo_engine();
        let clip = engine.session().track_by_name("Lead").and_then(|t| t.clips().first()).map(|c| c.range()).unwrap();
        let at = clip.start + (clip.end - clip.start) / 4;
        engine.execute("edit.select", &json!({"tracks": ["Lead"], "start": at, "end": at, "exact": true})).unwrap();
        let mut app = SoundApp::new(engine, None, crate::Services::default());
        app.run("window.midi_front", json!({})).unwrap();
        let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).with_step_dt(1.0 / 60.0).build_ui_state(
            |ui, app: &mut SoundApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            app,
        );
        h.input_mut().max_texture_side = Some(8192);
        h.run_steps(4);
        (h, clip)
    }

    /// The vertical lines drawn in `color` inside the MIDI editor's body (each must have the Edit
    /// window's `width`), and that body.
    fn editor_lines(h: &Harness<SoundApp>, color: egui::Color32, width: f32) -> (Vec<[egui::Pos2; 2]>, Rect) {
        let body = h.ctx.data(|d| d.get_temp::<Rect>(egui::Id::new("soundcraft-midi-body"))).unwrap();
        let lines = h
            .output()
            .shapes
            .iter()
            .filter_map(|c| match &c.shape {
                // The editor's own lines start at the roll's top (the Edit window's blinking
                // insertion line can fall inside the body's rect too).
                Shape::LineSegment { points, stroke }
                    if stroke.color == color
                        && points[0].x == points[1].x
                        && points[0].y == body.min.y + 22.0
                        && points.iter().all(|p| body.contains(*p)) =>
                {
                    assert_eq!(stroke.width, width, "the Edit window's stroke");
                    Some(*points)
                }
                _ => None,
            })
            .collect();
        (lines, body)
    }

    fn editor_playheads(h: &Harness<SoundApp>) -> (Vec<[egui::Pos2; 2]>, Rect) {
        editor_lines(h, Tokens::current().playhead, 1.5)
    }

    fn editor_insertions(h: &Harness<SoundApp>) -> (Vec<[egui::Pos2; 2]>, Rect) {
        editor_lines(h, Tokens::current().insertion, 1.0)
    }

    #[test]
    fn the_editor_draws_the_playhead_over_notes_and_velocities() {
        let (mut h, clip) = lead_in_editor();
        let cid = h.state().engine.session().track_by_name("Lead").and_then(|t| t.clips().first()).map(|c| c.id);
        assert_eq!(target_clip(h.state()), cid, "the editor shows the Lead clip");
        // Stopped: the insertion line (not the red playhead), a quarter of the way across the
        // roll (constant tempo).
        assert_eq!(editor_playheads(&h).0, Vec::<[egui::Pos2; 2]>::new(), "no playhead while stopped");
        let (lines, body) = editor_insertions(&h);
        let roll = Rangef::new(body.min.x + KEY_W, body.max.x - 4.0);
        let [[top, bottom]] = lines[..] else { panic!("one insertion line in the editor, got {lines:?}") };
        assert!((top.x - (roll.min + roll.span() / 4.0)).abs() < 1.0 && top.x == bottom.x, "{top:?} {bottom:?} on {roll:?}");
        // From the top of the roll down through the velocity lane.
        assert_eq!(top.y, body.min.y + 22.0);
        assert_eq!(bottom.y, body.max.y - 2.0);
        // Playing: at the transport position, not the insertion.
        h.state_mut().engine.transport.playing = true;
        h.state_mut().engine.transport.position = clip.start + (clip.end - clip.start) / 4 * 3;
        h.run_steps(2);
        let (lines, _) = editor_playheads(&h);
        let [[top, _]] = lines[..] else { panic!("one playhead line in the editor, got {lines:?}") };
        assert!((top.x - (roll.min + roll.span() * 0.75)).abs() < 1.0, "{top:?} on {roll:?}");
        // Played past the clip: the roll shows no line.
        h.state_mut().engine.transport.position = clip.end + 1;
        h.run_steps(2);
        assert_eq!(editor_playheads(&h).0, Vec::<[egui::Pos2; 2]>::new());
    }

    /// The Edit window's insertion lines: the insertion colour, outside the editor's own.
    fn edit_window_insertions(h: &Harness<SoundApp>) -> usize {
        let all =
            h.output().shapes.iter().filter(|c| matches!(&c.shape, Shape::LineSegment { stroke, .. } if stroke.color == Tokens::current().insertion));
        all.count() - editor_insertions(h).0.len()
    }

    #[test]
    fn the_insertion_line_blinks_with_the_edit_windows() {
        let (mut h, _) = lead_in_editor();
        let (mut on, mut off) = (0, 0);
        for _ in 0..90 {
            let editor = editor_insertions(&h).0.len();
            let edit = edit_window_insertions(&h);
            assert_eq!(editor == 1, edit > 0, "the editor's line ({editor}) and the Edit window's ({edit}) show together");
            if editor == 1 {
                on += 1
            } else {
                off += 1
            }
            h.step();
        }
        assert!(on > 0 && off > 0, "it blinks: on {on}, off {off} frames");
    }

    #[test]
    fn a_selected_range_has_no_insertion_line() {
        let (mut h, clip) = lead_in_editor();
        let (start, end) = (clip.start + (clip.end - clip.start) / 4, clip.start + (clip.end - clip.start) / 2);
        h.state_mut().run("edit.select", json!({"tracks": ["Lead"], "start": start, "end": end, "exact": true})).unwrap();
        for _ in 0..40 {
            h.step();
            assert_eq!(editor_insertions(&h).0, Vec::<[egui::Pos2; 2]>::new(), "Play starts at the range, but no line marks it");
        }
    }
}
