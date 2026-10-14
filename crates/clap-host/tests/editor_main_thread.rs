//! Plugin editors must be driven from the process main thread, which the standard test harness
//! never uses, so this test binary has its own `main` (`harness = false`). The fixture's GUI is
//! window-less: nothing appears on screen.

mod common;

use soundcraft_dsp::{ParamReading, Plugin};

const GAIN: &str = "clap:org.soundcraft.test.gain";

fn block(p: &mut Box<dyn Plugin>) -> Vec<Vec<f32>> {
    let mut io = vec![vec![1.0f32; 64]; 2];
    p.process(&mut io, 64);
    io
}

fn main() {
    common::fixture_dir();
    let mut p = soundcraft_clap_host::create(GAIN).expect("fixture");
    p.prepare(48_000.0, 64, 2);
    let mut ed = p.editor().expect("editor handle");
    assert!(!ed.is_open());
    ed.open().expect("open on the main thread");
    assert!(ed.is_open());
    ed.open().expect("opening again just raises it");
    // The fixture "edits" the gain in its GUI; the edit arrives as an output event of the next
    // block and is reported (in SoundCraft units) by idle.
    let io = block(&mut p);
    assert!((io[0][10] - 0.25).abs() < 1e-6, "{}", io[0][10]);
    assert_eq!(ed.idle(), vec![("7".to_string(), 0.25)]);
    assert!(ed.idle().is_empty(), "drained");
    // While its GUI exists the fixture reads Gain from a main-thread copy that `on_main_thread`
    // brings up to date, asked for by the block that changed it. The state reader has that call
    // made before it reads, so the value the block handed over is not taken for the plugin's old
    // one, which a save would store as set by the plugin itself.
    let mut reader = p.state_reader().expect("the gain fixture has clap.state");
    assert!(p.set_param("7", 1.5));
    let _ = block(&mut p);
    assert_eq!(reader.param_values(&["7"]), vec![Some(ParamReading { live: 1.5, host: 1.5 })]);
    ed.close();
    assert!(!ed.is_open());
    reader_idle_answers_callbacks();
    // Through the Plugin trait as well.
    p.open_editor().expect("open via the trait");
    assert!(ed.is_open(), "the handle sees the same editor");
    p.close_editor();
    assert!(!ed.is_open());
    // Dropping the instance with its editor open destroys the GUI first (the fixture asserts
    // that); the handle then refuses politely.
    ed.open().expect("reopen");
    drop(p);
    assert!(!ed.is_open());
    assert!(ed.open().is_err());
    assert!(ed.idle().is_empty());
    ed.close();
    /// The state reader's `idle` (every UI frame) makes a requested `on_main_thread` call: for a
    /// plugin with no editor, or with one that is not idled, nothing else would make it until a save.
    fn reader_idle_answers_callbacks() {
        let mut p = soundcraft_clap_host::instantiate_plugin(GAIN).expect("fixture");
        p.prepare(48_000.0, 64, 2);
        // The fixture asks for the call while its GUI exists, from the block that changed the gain.
        p.editor().expect("editor handle").open().expect("open on the main thread");
        let mut reader = p.state_reader().expect("the gain fixture has clap.state");
        assert!(p.set_param("7", 1.5));
        p.process(&mut vec![vec![1.0f32; 64]; 2], 64);
        assert!(p.callback_requested(), "the block asked for on_main_thread");
        reader.idle();
        assert!(!p.callback_requested(), "the reader's idle made the call");
    }

    #[cfg(target_os = "macos")]
    macos::dropped_elsewhere();
    println!("editor_main_thread: ok");
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSDate, NSRunLoop};

    /// Dropped on another thread with its editor open while processing (the audio thread drops
    /// instances when the mixer is torn down): only processing stops there; on the main thread the
    /// GUI is destroyed, then the plugin deactivated and destroyed. The fixture checks that its GUI
    /// calls, and `deactivate` while the GUI exists, come on the main thread, and that it is never
    /// destroyed with its GUI alive or while active (any of these would abort this test).
    pub fn dropped_elsewhere() {
        let mut p = soundcraft_clap_host::create(super::GAIN).expect("fixture");
        p.prepare(48_000.0, 64, 2);
        let _ = super::block(&mut p);
        let mut ed = p.editor().expect("editor handle");
        ed.open().expect("open on the main thread");
        std::thread::spawn(move || drop(p)).join().expect("dropped on another thread");
        assert_eq!(soundcraft_clap_host::instances_awaiting_main_thread(), 1, "waits for the main thread");
        assert!(!ed.is_open(), "the handle reports the editor closed at once");
        assert!(ed.open().is_err(), "the plugin is going");
        let start = std::time::Instant::now();
        while soundcraft_clap_host::instances_awaiting_main_thread() > 0 && start.elapsed().as_secs() < 5 {
            autoreleasepool(|_| NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01)));
        }
        assert_eq!(soundcraft_clap_host::instances_awaiting_main_thread(), 0, "destroyed on the main thread");
    }
}
