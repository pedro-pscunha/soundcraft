//! VST3 editors on the process main thread (`harness = false`: the standard harness never runs
//! tests there). Nothing may reach the screen: the view probe and the parameter round trip need no
//! window, and on macOS the editor opens in SoundCraft's plugin window without ever being ordered
//! front (`open_editor_hidden`). The shown window is the same one ordered front (checked by hand in
//! the app; see `installed_plugins_smoke`).

mod common;

fn main() {
    let dir = common::fixture_dir();
    let gain = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]).into_iter().find(|d| d.name == "Fixture Gain").expect("fixture").id;
    let mut p = soundcraft_vst3_host::instantiate_plugin(&gain).expect("instantiate");
    // Editor windows are hosted on macOS and Windows; elsewhere the probe reports "unsupported".
    if cfg!(any(target_os = "macos", windows)) {
        assert_eq!(p.editor_size(), Ok((320, 240)));
    } else {
        assert!(p.editor_size().is_err());
    }
    let mut ed = soundcraft_dsp::Plugin::editor(&mut p).expect("editor handle");
    assert!(!ed.is_open());
    assert_eq!(ed.idle().len(), 1, "the fixture's setup edit");
    // Session → editor: the controller follows (seen through the component handler round trip
    // below and through the editor's own value).
    ed.set_param("7", 1.5);
    ed.set_param("nope", 1.0);
    assert!(ed.idle().is_empty(), "set_param is not reported back as an edit");
    generic_panel_drag(&mut p, ed.as_mut());
    #[cfg(target_os = "macos")]
    macos::shared_window(&mut p, ed.as_mut());
    #[cfg(target_os = "macos")]
    macos::dropped_elsewhere(&gain);
    drop(p);
    assert!(!ed.is_open());
    assert!(ed.open().is_err(), "the plugin is gone");
    println!("editor_main_thread: ok");
}

/// A drag in the generic plugin panel sets each value in the session and, at once, on the
/// controller through the editor handle. The audio thread applies the session's values a block
/// later, and the next UI frame hands what it applied to the controller: an older value than the
/// one the controller already has must not be handed over (the knob would jump back for a frame).
fn generic_panel_drag(p: &mut soundcraft_vst3_host::Vst3Plugin, ed: &mut dyn soundcraft_dsp::PluginEditor) {
    use soundcraft_dsp::Plugin;
    let mut reader = p.state_reader().expect("every VST3 component has a state");
    let live = |reader: &mut Box<dyn soundcraft_dsp::PluginStateReader>| reader.param_values(&["7"])[0].expect("Gain").live;
    ed.set_param("7", 0.4);
    ed.set_param("7", 0.6);
    // The audio thread has applied only the first value when the UI frame comes.
    assert!(p.set_param("7", 0.4));
    reader.idle();
    assert!((live(&mut reader) - 0.6).abs() < 1e-6, "the controller keeps the newer value: {}", live(&mut reader));
    assert!(p.set_param("7", 0.6));
    reader.idle();
    assert!((live(&mut reader) - 0.6).abs() < 1e-6);
    // A value of the host's own (Undo, automation) is handed over.
    assert!(p.set_param("7", 1.0));
    reader.idle();
    assert!((live(&mut reader) - 1.0).abs() < 1e-6, "the host's own value: {}", live(&mut reader));
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::MainThreadMarker;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{NSDate, NSRunLoop};
    use soundcraft_dsp::PluginEditor;
    use soundcraft_macos::plugin_window::testing;
    use soundcraft_vst3_host::Vst3Plugin;

    /// The process's windows: (all, visible, how many are SoundCraft's plugin window).
    fn windows(mtm: MainThreadMarker) -> (usize, usize, usize) {
        autoreleasepool(|_| {
            let all = NSApplication::sharedApplication(mtm).windows();
            let n = all.count();
            let visible = (0..n).filter(|&i| all.objectAtIndex(i).isVisible()).count();
            let plugin = (0..n).filter(|&i| all.objectAtIndex(i).class().name().to_bytes().ends_with(b"SoundCraftPluginWindow")).count();
            (n, visible, plugin)
        })
    }

    /// The view attaches to SoundCraft's plugin window, which idle keeps while hidden and close
    /// takes down.
    pub fn shared_window(p: &mut Vst3Plugin, ed: &mut dyn PluginEditor) {
        let mtm = MainThreadMarker::new().expect("the main thread");
        let (base, visible, plugin) = windows(mtm);
        assert_eq!((visible, plugin), (0, 0));
        for _ in 0..3 {
            autoreleasepool(|_| p.open_editor_hidden()).expect("open");
            assert!(ed.is_open());
            assert_eq!(windows(mtm), (base + 1, 0, 1), "one hidden plugin window");
            autoreleasepool(|_| p.open_editor_hidden()).expect("again");
            assert_eq!(windows(mtm), (base + 1, 0, 1), "opening again keeps the window");
            assert!(ed.idle().is_empty());
            assert!(ed.is_open(), "idle keeps a hidden editor");
            autoreleasepool(|_| ed.close());
            assert!(!ed.is_open());
            assert_eq!(windows(mtm), (base, 0, 0), "the window is gone");
        }
        // Left open: the plugin's drop (in `main`) closes it.
        autoreleasepool(|_| p.open_editor_hidden()).expect("open");
        assert_eq!(windows(mtm).2, 1);

        // An edit made in the open editor, then the editor raised (a click on its slot, the click
        // that activates SoundCraft) before the next idle: the edit is still reported. The raise
        // takes the main-thread path (the window is open and was not closed by the user), with
        // the window kept off screen: `show` marks it as shown without ordering it front.
        testing::set_order_front(Some(|_| {}));
        testing::set_app_active(Some(false));
        p.edit_as_view(7, 0.25);
        autoreleasepool(|_| ed.open()).expect("raised");
        testing::set_order_front(None);
        assert_eq!(windows(mtm), (base + 1, 0, 1), "raised, not reopened, and nothing on screen");
        assert_eq!(ed.idle(), [("7".to_string(), 0.5)], "the edit made before the raise (0.25 of 0..2)");
        assert!(ed.is_open(), "shown, then hidden with the app in the background: open");
        // Hidden while SoundCraft is active: the user closed it; idle takes it down.
        testing::set_app_active(Some(true));
        assert!(!ed.is_open());
        assert!(ed.idle().is_empty());
        assert_eq!(windows(mtm), (base, 0, 0), "the window is gone");
        testing::set_app_active(None);
        // Left open: the plugin's drop (in `main`) closes it.
        autoreleasepool(|_| p.open_editor_hidden()).expect("open");
        assert_eq!(windows(mtm).2, 1);
        // Off the main thread nothing opens.
        std::thread::scope(|s| s.spawn(|| ed.open()).join()).expect("joined").expect_err("windows open on the main thread only");
    }

    /// Dropped on another thread with its editor open (the audio thread drops instances when the
    /// mixer is torn down): nothing is touched there; on the main thread the view is removed, the
    /// window closed and the plugin terminated, and nothing of it is left. At process exit the
    /// fixture checks that none of its objects is alive when its module exit runs.
    pub fn dropped_elsewhere(id: &str) {
        let mtm = MainThreadMarker::new().expect("the main thread");
        let (base, _, plugins) = windows(mtm);
        let mut p = soundcraft_vst3_host::instantiate_plugin(id).expect("instantiate");
        let ed = soundcraft_dsp::Plugin::editor(&mut p).expect("editor handle");
        autoreleasepool(|_| p.open_editor_hidden()).expect("open");
        assert_eq!(windows(mtm), (base + 1, 0, plugins + 1));
        std::thread::spawn(move || drop(p)).join().expect("dropped on another thread");
        assert_eq!(soundcraft_vst3_host::instances_awaiting_main_thread(), 1, "waits for the main thread");
        assert_eq!(windows(mtm), (base + 1, 0, plugins + 1), "not touched off the main thread");
        let start = std::time::Instant::now();
        while soundcraft_vst3_host::instances_awaiting_main_thread() > 0 && start.elapsed().as_secs() < 5 {
            autoreleasepool(|_| NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01)));
        }
        assert_eq!(soundcraft_vst3_host::instances_awaiting_main_thread(), 0, "torn down on the main thread");
        assert_eq!(windows(mtm), (base, 0, plugins), "the window is gone");
        assert!(!ed.is_open());
    }
}
