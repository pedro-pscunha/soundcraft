//! Audio Unit editors on the process main thread (`harness = false`: the standard harness never
//! runs tests there). Nothing may reach the screen: every view is made in a window that is never
//! ordered front (`editor_size` makes one and closes it at once, `open_editor_hidden` keeps it),
//! and no window of the process may be visible at any point. Units missing here are skipped. The
//! shown window is the same one ordered front (checked by hand in the app).

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::MainThreadMarker;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{NSDate, NSRunLoop, NSSize};
    use soundcraft_au_host::{units_awaiting_disposal, units_leaked};
    use soundcraft_dsp::Plugin;

    /// AUDelay (Apple, a view of its own), AUMatrixReverb (Apple, none: the generic view) and
    /// FabFilter Pro-L 2 (a third-party view).
    const UNITS: [&str; 3] = ["au:aufx:dely:appl", "au:aufx:mrev:appl", "au:aumf:FL2p:FabF"];
    /// Open/close rounds per unit.
    const ROUNDS: usize = 40;
    /// Rounds before memory is measured: views fill caches of their own at first (Pro-L 2 grows
    /// by some 40 MB over its first 20 windows).
    const WARM_UP: usize = 20;
    /// Most the process may grow over the rounds after the warm-up: a coarse backstop (the
    /// allocator grows in steps of megabytes). The exact checks are the window count after each
    /// close and the disposal wait at the end, which fails while any view closed in the rounds is
    /// still alive.
    const MAX_GROWTH_KIB: u64 = 64 << 10;

    /// The process's windows: (all, visible).
    fn windows(mtm: MainThreadMarker) -> (usize, usize) {
        // In a pool: the window list retains every window until it is drained.
        autoreleasepool(|_| {
            let all = NSApplication::sharedApplication(mtm).windows();
            let visible = (0..all.count()).filter(|&i| all.objectAtIndex(i).isVisible()).count();
            (all.count(), visible)
        })
    }

    /// How many of the process's windows are SoundCraft's plugin window (shared with VST3).
    fn plugin_windows(mtm: MainThreadMarker) -> usize {
        autoreleasepool(|_| {
            let all = NSApplication::sharedApplication(mtm).windows();
            (0..all.count()).filter(|&i| all.objectAtIndex(i).class().name().to_bytes().ends_with(b"SoundCraftPluginWindow")).count()
        })
    }

    /// Runs the main run loop for `secs`, as the app does between frames.
    fn spin(secs: f64) {
        autoreleasepool(|_| NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(secs)));
    }

    /// Runs the run loop until every unit awaiting its views is disposed; how long that took (s).
    fn settle() -> f64 {
        let start = std::time::Instant::now();
        while units_awaiting_disposal() > 0 && start.elapsed().as_secs() < 5 {
            spin(0.01);
        }
        assert_eq!(units_awaiting_disposal(), 0, "views outlived their plugin by 5 s");
        assert_eq!(units_leaked(), 0, "a unit was given up");
        start.elapsed().as_secs_f64()
    }

    /// Resident memory in KiB.
    fn rss_kib() -> u64 {
        let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().expect("ps");
        String::from_utf8_lossy(&out.stdout).trim().parse().expect("rss")
    }

    /// The view resizes itself: the editor's idle fits the window to it, keeping the window's
    /// top-left corner. The view gets its size back at the end: some plugins (Pro-L 2) remember
    /// their editor size for every later instance.
    fn view_resizes(mtm: MainThreadMarker, id: &str) {
        let mut p = soundcraft_au_host::instantiate_plugin(id).expect("instantiate");
        let mut ed = p.editor().expect("an editor");
        p.open_editor_hidden().expect("open");
        autoreleasepool(|_| {
            let view = p.editor_view().expect("the view");
            let window = view.window().expect("in its window");
            let before = window.frame();
            let old = window.contentRectForFrameRect(before).size;
            let original = view.frame().size;
            view.setFrameSize(NSSize::new(old.width + 40.0, old.height + 30.0));
            let want = view.frame().size;
            if (want.width, want.height) == (old.width, old.height) {
                println!("{id}: the view keeps its size");
                view.setFrameSize(original);
                return;
            }
            assert!(ed.idle().is_empty());
            let after = window.frame();
            let content = window.contentRectForFrameRect(after).size;
            assert_eq!((content.width, content.height), (want.width.round(), want.height.round()), "{id}: the window follows the view");
            assert_eq!(after.origin.x, before.origin.x, "{id}: the left edge stays");
            assert_eq!(after.origin.y + after.size.height, before.origin.y + before.size.height, "{id}: the top edge stays");
            println!("{id}: the view grew to {}x{}; the window followed", want.width, want.height);
            view.setFrameSize(original);
            assert!(ed.idle().is_empty());
        });
        ed.close();
        drop(p);
        settle();
        assert_eq!(windows(mtm).1, 0);
    }

    /// A view held alive (as a plugin's own timer may hold it) 10 s after its plugin went: the unit
    /// is given up, uninitialized and counted as leaked, and no longer awaited. Last: it leaks.
    fn a_view_that_never_goes(id: &str) {
        let leaked = units_leaked();
        let mut p = soundcraft_au_host::instantiate_plugin(id).expect("instantiate");
        p.open_editor_hidden().expect("open");
        let view = p.editor_view().expect("the view");
        drop(p);
        assert_eq!(units_awaiting_disposal(), 1, "{id}: the unit waits for its view");
        let start = std::time::Instant::now();
        while units_awaiting_disposal() > 0 && start.elapsed().as_secs() < 15 {
            spin(0.05);
        }
        let waited = start.elapsed().as_secs_f64();
        assert_eq!(units_awaiting_disposal(), 0, "{id}: still awaited after {waited:.1} s");
        assert_eq!(units_leaked(), leaked + 1, "{id}: given up");
        assert!(waited > 9.0, "{id}: given up after {waited:.1} s, not 10");
        println!("{id}: a view held alive; the unit was given up after {waited:.1} s");
        drop(view);
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("the main thread");
        let installed = soundcraft_au_host::scan();
        let mut checked = 0;
        for id in UNITS {
            if !installed.iter().any(|d| d.id == id) {
                println!("{id}: not installed, skipped");
                continue;
            }
            let mut p = soundcraft_au_host::instantiate_plugin(id).expect("instantiate");
            let (w, h) = p.editor_size().expect("a view");
            let kind = if p.has_custom_editor() { "its own" } else { "the generic" };
            println!("{id}: {kind} view, {w}x{h}");
            assert!(w > 0 && h > 0, "{id}: the view has a size");
            spin(0.05);
            // Frameworks may keep windows of their own (never shown) once a view was made.
            let (base, visible) = windows(mtm);
            assert_eq!(visible, 0, "{id}: nothing on screen");

            let mut ed = p.editor().expect("an editor");
            assert!(!ed.is_open());
            let mut rss = 0;
            for round in 0..ROUNDS {
                if round == WARM_UP {
                    rss = rss_kib();
                }
                p.open_editor_hidden().expect("open");
                assert!(ed.is_open(), "{id}: open");
                assert_eq!(windows(mtm), (base + 1, 0), "{id}: one hidden window");
                assert_eq!(plugin_windows(mtm), 1, "{id}: SoundCraft's plugin window");
                spin(0.02);
                // The unit is re-initialized and renders while its view is up.
                p.prepare(if round % 2 == 0 { 96_000.0 } else { 48_000.0 }, 512, 2);
                let mut io = vec![vec![0.25f32; 512], vec![-0.25f32; 512]];
                p.process(&mut io, 512);
                assert!(io.iter().flatten().all(|s| s.is_finite()), "{id}: renders with its view up");
                spin(0.02);
                p.open_editor_hidden().expect("raise");
                assert_eq!(windows(mtm), (base + 1, 0), "{id}: opening again keeps the window");
                assert!(ed.idle().is_empty(), "{id}: no edits without a gesture");
                assert!(ed.is_open(), "{id}: idle keeps a hidden editor");
                ed.close();
                assert!(!ed.is_open(), "{id}: closed");
                assert_eq!(windows(mtm), (base, 0), "{id}: the window is gone");
                assert_eq!(plugin_windows(mtm), 0);
                spin(0.02);
            }
            let grown = rss_kib().saturating_sub(rss);
            println!("{id}: {} open/close rounds after the warm-up grew the process by {grown} KiB", ROUNDS - WARM_UP);
            assert!(grown < MAX_GROWTH_KIB, "{id}: {grown} KiB");

            // Closed, then dropped at once: the unit waits for a view that outlives its window.
            p.open_editor_hidden().expect("open");
            spin(0.02);
            ed.close();
            drop(p);
            let waited = units_awaiting_disposal();
            println!("{id}: dropped right after closing; {waited} unit(s) waited {:.2} s for their view", settle());

            // Dropped while open: the instance closes its editor, then the unit goes once the view
            // has.
            let mut p = soundcraft_au_host::instantiate_plugin(id).expect("instantiate");
            let mut ed = p.editor().expect("an editor");
            p.open_editor_hidden().expect("open");
            spin(0.02);
            drop(p);
            assert!(!ed.is_open(), "{id}: closed with its plugin");
            assert_eq!(windows(mtm), (base, 0), "{id}: the window went with the plugin");
            let waited = units_awaiting_disposal();
            println!("{id}: dropped while open; {waited} unit(s) waited {:.2} s for their view", settle());
            assert!(ed.open().is_err(), "{id}: the plugin is gone");
            assert!(ed.idle().is_empty());
            ed.set_param("0", 0.5);
            assert_eq!(windows(mtm).1, 0, "{id}: nothing on screen");

            // Dropped on another thread while open (the audio thread drops retired instances):
            // the editor is closed on the main thread, then the unit goes once the view has.
            let mut p = soundcraft_au_host::instantiate_plugin(id).expect("instantiate");
            let ed = p.editor().expect("an editor");
            p.open_editor_hidden().expect("open");
            spin(0.02);
            assert_eq!(windows(mtm), (base + 1, 0));
            std::thread::spawn(move || drop(p)).join().expect("dropped on another thread");
            assert_eq!(units_awaiting_disposal(), 1, "{id}: waits for the main thread");
            let waited = settle();
            assert!(!ed.is_open(), "{id}: closed with its plugin");
            assert_eq!(windows(mtm), (base, 0), "{id}: the window went on the main thread");
            println!("{id}: dropped on another thread while open; disposed after {waited:.2} s");

            view_resizes(mtm, id);
            checked += 1;
        }
        assert!(checked > 0, "no Audio Unit to test");
        if installed.iter().any(|d| d.id == UNITS[0]) {
            a_view_that_never_goes(UNITS[0]);
        }
        println!("editor_main_thread: ok ({checked} units)");
    }
}
