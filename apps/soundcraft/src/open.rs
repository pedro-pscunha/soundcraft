//! Files SoundCraft is asked to open: dropped on its window, opened with it from Finder or the Dock
//! (macOS), or named on the command line. A session opens (asking to save the open one first), MIDI
//! and audio files import.

use serde_json::{Value, json};
use soundcraft_ui_egui::{SoundApp, dialogs};
use std::collections::VecDeque;
use std::path::Path;

/// Files waiting to open while a dialog is up, so none replaces it (a Save Changes question, say)
/// and none lands in a session that is about to be replaced.
#[derive(Default)]
pub struct Opener {
    pending: VecDeque<Waiting>,
    /// Batches so far, to tell them apart.
    batches: u64,
}

/// A file waiting to open.
struct Waiting {
    path: String,
    /// The `Opener::open` call it came with.
    batch: u64,
    /// The session its batch opens first, which it goes into, or nowhere.
    into: Option<String>,
}

impl Opener {
    /// Opens one batch of files (dropped together, or opened together from Finder). Only the first
    /// session opens, first, and the status line names the others; MIDI and audio files then
    /// import in order, into that session: when it does not open, they don't either. While a
    /// dialog is up, what is left of the batch waits for it to close, and the status line says so.
    pub fn open(&mut self, app: &mut SoundApp, batch: Vec<String>) {
        let before = app.ui.status.clone();
        let (sessions, files): (Vec<String>, Vec<String>) = batch.into_iter().filter(|p| !p.is_empty()).partition(|p| is_session(p));
        let mut sessions = sessions.into_iter();
        let session = sessions.next();
        let skipped: Vec<String> = sessions.map(|p| file_name(&p)).collect();
        self.batches = self.batches.wrapping_add(1);
        let batch = self.batches;
        self.pending.extend(session.clone().map(|path| Waiting { path, batch, into: None }));
        self.pending.extend(files.into_iter().map(|path| Waiting { path, batch, into: session.clone() }));
        self.drain(app);
        if !skipped.is_empty() {
            say(app, &before, &format!("One session opens at a time: {} did not open.", skipped.join(", ")));
        }
        let waiting: Vec<String> = self.pending.iter().filter(|w| w.batch == batch).map(|w| file_name(&w.path)).collect();
        if !waiting.is_empty() {
            say(app, &before, &format!("Waiting for the dialog to close: {}.", waiting.join(", ")));
        }
    }

    /// Opens what waits, unless a dialog is up. Call every frame.
    pub fn drain(&mut self, app: &mut SoundApp) {
        if self.pending.is_empty() || app.dialogs.open.is_some() {
            return;
        }
        let before = app.ui.status.clone();
        while app.dialogs.open.is_none()
            && let Some(w) = self.pending.pop_front()
        {
            match &w.into {
                // Its session did not open (its save question was cancelled, or the open was
                // refused or failed): the rest of the batch was meant for it.
                Some(session) if !soundcraft_engine::io::is_open_session(&app.engine, session) => {
                    let rest = self.pending.iter().take_while(|next| next.batch == w.batch).count();
                    let mut names = vec![file_name(&w.path)];
                    names.extend(self.pending.drain(..rest).map(|next| file_name(&next.path)));
                    say(app, &before, &format!("Not imported, as {} did not open: {}.", file_name(session), names.join(", ")));
                }
                _ => open_path(app, &w.path),
            }
        }
    }
}

/// Puts `msg` in the status line, after what opening the files said there (whatever is not
/// `before`, the line as it was).
fn say(app: &mut SoundApp, before: &str, msg: &str) {
    app.ui.status = if app.ui.status.is_empty() || app.ui.status == before { msg.to_string() } else { format!("{} {msg}", app.ui.status) };
}

fn is_session(p: &str) -> bool {
    p.to_ascii_lowercase().ends_with(".scraft")
}

fn file_name(p: &str) -> String {
    Path::new(p).file_name().map_or_else(|| p.to_string(), |n| n.to_string_lossy().into_owned())
}

fn open_path(app: &mut SoundApp, p: &str) {
    let lower = p.to_ascii_lowercase();
    let at = app.engine.session().edit.selection.start;
    let r = if is_session(p) {
        // The open session itself stays as it is (`SoundApp::run`).
        dialogs::run_or_ask(app, "session.open", json!({"path": p})).map(drop)
    } else if lower.ends_with(".mid") || lower.ends_with(".midi") || lower.ends_with(".smf") {
        app.run("file.import_midi", json!({"path": p})).map(|v| imported(app, p, &v["tracks"], 0))
    } else {
        app.run("file.import_audio", json!({"path": p, "at": at})).map(|v| imported(app, p, &v["imported"], at))
    };
    if let Err(e) = r {
        log::warn!("{p}: {e}");
    }
}

/// Says in the status line where the file `p` went: `tracks` lists the track ids it went to
/// (numbers, or objects with a `track`), from `at`.
fn imported(app: &mut SoundApp, p: &str, tracks: &Value, at: i64) {
    let s = app.engine.session();
    let ids = tracks.as_array().into_iter().flatten().filter_map(|t| t.as_u64().or_else(|| t.get("track")?.as_u64()));
    let names: Vec<&str> = ids.filter_map(|id| s.tracks.iter().find(|t| t.id.0 == id)).map(|t| t.name.as_str()).collect();
    let to = match names[..] {
        [] => String::new(),
        [one] => format!(" to {one}"),
        _ => format!(" to {} tracks", names.len()),
    };
    app.ui.status = format!("Imported {}{to} at {}.", file_name(p), s.format_position(at, s.edit.main_counter));
}

#[cfg(test)]
mod tests {
    use super::Opener;
    use serde_json::json;
    use soundcraft_ui_egui::{Services, SoundApp};
    use std::path::PathBuf;

    /// A fresh folder for one test.
    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundcraft-open-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn app() -> SoundApp {
        SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default())
    }

    fn path(p: &std::path::Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn opening_the_open_session_again_keeps_it_as_it_is() {
        let d = dir("again");
        let mut app = app();
        let file = d.join("Song.scraft");
        app.run("session.save_as", json!({"path": path(&d.join("Song"))})).unwrap();
        app.run("track.new", json!({"name": "Unsaved"})).unwrap();
        let undo = app.engine.undo_label().map(str::to_string);
        Opener::default().open(&mut app, vec![path(&file)]);
        assert!(app.engine.session().track_by_name("Unsaved").is_some(), "not reloaded");
        assert_eq!((app.engine.undo_label().map(str::to_string), app.engine.is_dirty()), (undo, true));
        assert!(app.dialogs.open.is_none(), "nothing to save first");
        assert_eq!(app.ui.status, format!("{} is already open.", app.engine.session().name));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_batch_opens_one_session_and_waits_for_its_save_question() {
        let d = dir("batch");
        let (a, b, c) = (d.join("A.scraft"), d.join("B.scraft"), d.join("Lead.mid"));
        // Two sessions on disk, and a MIDI file.
        let mut maker = app();
        maker.run("file.export_midi", json!({"path": path(&c), "tracks": ["Lead"]})).unwrap();
        maker.run("session.save_as", json!({"path": path(&a)})).unwrap();
        maker.run("session.save_as", json!({"path": path(&b)})).unwrap();
        let mut app = app();
        app.run("track.new", json!({"name": "Unsaved"})).unwrap();
        let tracks = app.engine.session().tracks.len();
        let mut opener = Opener::default();
        opener.open(&mut app, vec![path(&c), path(&a), path(&b)]);
        // A asks to save the modified session first; the MIDI file waits, so it neither replaces
        // that question nor lands in the session being replaced.
        assert!(matches!(&app.dialogs.open, Some(soundcraft_ui_egui::dialogs::Dialog::SaveChanges { then }) if then.1["path"] == path(&a)));
        assert_eq!(app.ui.status, "One session opens at a time: B.scraft did not open. Waiting for the dialog to close: Lead.mid.");
        assert_eq!(app.engine.session().tracks.len(), tracks, "the MIDI file waits");
        opener.drain(&mut app);
        assert_eq!(app.engine.session().tracks.len(), tracks, "still waiting");
        // Don't Save: A opens, then the MIDI file imports into it.
        app.dialogs.open = None;
        app.run("session.open", json!({"path": path(&a)})).unwrap();
        opener.drain(&mut app);
        assert_eq!(app.engine.path.as_deref(), Some(path(&a).as_str()));
        assert!(app.engine.session().track_by_name("Unsaved").is_none());
        assert!(app.ui.status.starts_with("Imported Lead.mid to "), "{}", app.ui.status);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A session on disk, a MIDI file and a modified open session (Unsaved), in `d`.
    fn modified_with_files(d: &std::path::Path) -> (SoundApp, String, String) {
        let (a, mid) = (d.join("A.scraft"), d.join("Lead.mid"));
        let mut maker = app();
        maker.run("file.export_midi", json!({"path": path(&mid), "tracks": ["Lead"]})).unwrap();
        maker.run("session.save_as", json!({"path": path(&a)})).unwrap();
        let mut app = app();
        app.run("track.new", json!({"name": "Unsaved"})).unwrap();
        (app, path(&a), path(&mid))
    }

    #[test]
    fn cancelling_the_save_question_drops_the_rest_of_the_batch() {
        let d = dir("cancel");
        let (mut app, a, mid) = modified_with_files(&d);
        let tracks = app.engine.session().tracks.len();
        let mut opener = Opener::default();
        opener.open(&mut app, vec![a, mid]);
        assert!(app.dialogs.open.is_some(), "save first?");
        // Cancel: the session stays, so the MIDI file meant for A does not land in it.
        app.dialogs.open = None;
        opener.drain(&mut app);
        assert_eq!(app.engine.session().tracks.len(), tracks);
        assert!(app.engine.session().track_by_name("Unsaved").is_some());
        assert_eq!(app.ui.status, "Not imported, as A.scraft did not open: Lead.mid.");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_session_that_fails_to_open_takes_the_rest_of_its_batch_with_it() {
        let d = dir("fails");
        let (mut app, a, mid) = modified_with_files(&d);
        app.run("session.save_as", json!({"path": path(&d.join("Mine.scraft"))})).unwrap();
        let tracks = app.engine.session().tracks.len();
        std::fs::write(&a, "not a session").unwrap();
        Opener::default().open(&mut app, vec![mid.clone(), a.clone(), path(&d.join("B.scraft"))]);
        assert_eq!(app.engine.session().tracks.len(), tracks, "the MIDI file did not land in Mine");
        // Why A did not open, then what became of the rest of the batch.
        let rest = " Not imported, as A.scraft did not open: Lead.mid. One session opens at a time: B.scraft did not open.";
        let why = app.ui.status.strip_suffix(rest).unwrap_or_else(|| panic!("{}", app.ui.status));
        assert!(why.starts_with("not a SoundCraft session"), "{why}");
        // Imported on its own, it goes into the open session.
        Opener::default().open(&mut app, vec![mid]);
        assert_eq!(app.engine.session().tracks.len(), tracks + 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_import_says_where_the_file_went() {
        let d = dir("import");
        let wav = d.join("Mix.wav");
        let mut app = app();
        app.run("file.bounce_mix", json!({"path": path(&wav), "start": 0, "end": 4800})).unwrap();
        app.run("edit.select", json!({"start": 96_000, "end": 96_000})).unwrap();
        let at = app.engine.session().format_position(96_000, app.engine.session().edit.main_counter);
        Opener::default().open(&mut app, vec![path(&wav)]);
        // A new track, named after the file.
        assert!(app.engine.session().track_by_name("Mix").is_some_and(|t| t.clips().first().is_some_and(|c| c.start == 96_000)));
        assert_eq!(app.ui.status, format!("Imported Mix.wav to Mix at {at}."));
        let _ = std::fs::remove_dir_all(&d);
    }
}
