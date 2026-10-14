//! Out-of-process scanning: one helper process per bundle, a few at a time, each with a time limit.
//! A plugin that never returns from its module entry (a PACE/iLok-protected plugin without its
//! licence service) or crashes there then costs a skipped bundle, not the whole application.
//!
//! The protocol: `<exe> --soundcraft-scan-vst3 <bundle>` first greets, before any plugin code
//! runs, with the line `soundcraft-vst3-scan-protocol: 1`. It then loads that one bundle (the
//! in-process [`load_bundle`](crate::load_bundle)) and prints one answer line,
//! `soundcraft-vst3-scan: ` and then JSON, `{"ok":[descriptors]}` or `{"error":"why"}`.
//! On Unix both lines go to a private copy of standard output, and standard output itself is
//! pointed at standard error, so plugin code cannot write into them.
//!
//! The helper exits with:
//! - 0 when the bundle loaded, 2 when it did not;
//! - 3 when its host is gone and its group could not be killed (see below; normally SIGKILL);
//! - 4 when plugin code crashed it (Unix: its crash signals end it at once, so no crash report is
//!   written).
//!
//! The host reads only lines that start with a marker and parse, and keeps the last answer. A
//! run without this protocol's greeting is a failure on SoundCraft's side, not the bundle's (see
//! [`Skip`]). The helper's standard error, where plugins chatter, is drained: its last 8 KiB go to
//! the debug log, and its last line into the reason when the helper ended without an answer.
//!
//! Once the answer is in, the helper has [`EXIT_GRACE`] to exit (module exits, static
//! destructors) before it is killed; the answer counts either way. A helper still running at its
//! time limit is killed, on Unix together with whatever it started (each helper leads a process
//! group of its own).
//!
//! A helper whose host is gone (killed mid-scan) ends too:
//! - on Unix it watches its parent (the host hands it its process id in
//!   `SOUNDCRAFT_SCAN_HOST_PID`);
//! - on Windows the host's job object kills it.
//!
//! So a plugin stuck in its module entry cannot keep a helper alive forever.

use crate::{ID_PREFIX, SCAN_HELPER_FLAG, ScanHelper, Vst3Descriptor, parse_id};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

/// Starts the answer line.
const MARKER: &str = "soundcraft-vst3-scan: ";
/// Starts the greeting a helper prints before it loads anything, followed by [`PROTOCOL`].
const HELLO: &str = "soundcraft-vst3-scan-protocol: ";
/// The version of this protocol. A helper that does not greet with it (an older or newer
/// SoundCraft, or a program that is no scan helper at all) is SoundCraft's failure, not the
/// bundle's.
pub(crate) const PROTOCOL: &str = "1";
/// Most of a helper's standard output read for the answer (the rest is drained unread).
const MAX_STDOUT: u64 = 8 << 20;
/// How much of the end of a helper's standard error is kept.
const STDERR_TAIL: usize = 8 << 10;
/// Most characters of the helper's last error line put into a skip reason.
const MAX_LAST_LINE: usize = 200;
/// Most characters of a skip reason (a helper's error answer can be anything).
const MAX_REASON: usize = 300;
/// Most characters of a plugin's name, vendor, version or sub-category from a helper or the cache.
const MAX_FIELD: usize = 256;
/// Most sub-categories kept per plugin (as a scan in this process splits them).
const MAX_SUB_CATEGORIES: usize = 32;
/// How long a helper may take to exit once it has answered.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long a killed helper is waited for here before a thread of its own reaps it.
const REAP_WAIT: Duration = Duration::from_secs(1);
/// How often a running helper is checked.
const POLL: Duration = Duration::from_millis(10);
/// Most helpers at once, whatever the setting.
const MAX_PARALLEL: usize = 16;
/// The longest time limit honoured (a longer one would overflow the deadline).
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// The host's process id, handed to each helper.
const HOST_ENV: &str = "SOUNDCRAFT_SCAN_HOST_PID";
/// How often a helper checks that its host is still there.
#[cfg(unix)]
const HOST_POLL: Duration = Duration::from_millis(250);

/// Why a helper gave no plugins for a bundle.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Skip {
    /// The bundle's doing: its helper crashed, or the bundle did not load. Kept in the scan cache
    /// across runs.
    Bundle(String),
    /// The bundle's doing too, but only provisionally: its helper ran past its time limit (which
    /// a slow start, or a busy machine, can also cause). Cached as a skip only when it happens
    /// again on the next scan.
    TimedOut(String),
    /// SoundCraft's side: the helper could not be started or read, or is no scan helper of this
    /// protocol, and never ran the bundle's code. Not a verdict on the bundle: never remembered or
    /// cached.
    Host(String),
    /// SoundCraft's side too, but the helper may have been running the bundle's code: it was lost
    /// track of, or ran past its time limit before greeting. The bundle may hang: never loaded in
    /// this process instead.
    HostStuck(String),
}

/// What a helper prints after [`MARKER`].
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Answer {
    Ok(Vec<Vst3Descriptor>),
    Error(String),
}

/// The answer line for one bundle, loaded in this process, and whether it loaded.
pub(crate) fn answer(path: &Path) -> (String, bool) {
    let (answer, ok) = match crate::registry::load_bundle(path) {
        Ok(mut ds) => {
            // JSON strings are UTF-8; the host puts its own path back anyway.
            for d in &mut ds {
                if d.path.to_str().is_none() {
                    d.path = PathBuf::from(d.path.to_string_lossy().into_owned());
                }
            }
            (Answer::Ok(ds), true)
        }
        Err(e) => (Answer::Error(e.to_string()), false),
    };
    match serde_json::to_string(&answer) {
        Ok(json) => (format!("{MARKER}{json}"), ok),
        Err(e) => (error_line(&format!("cannot encode the plugins: {e}")), false),
    }
}

fn error_line(why: &str) -> String {
    format!("{MARKER}{}", serde_json::json!({ "error": why }))
}

/// The helper side: answers `--soundcraft-scan-vst3 <bundle>`, `None` for other arguments.
pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> Option<i32> {
    let mut args = args.into_iter();
    if args.next()?.to_str() != Some(SCAN_HELPER_FLAG) {
        return None;
    }
    end_with_host();
    #[cfg(target_os = "linux")]
    crate::ffi::name_scan_helper();
    #[cfg(unix)]
    crate::ffi::exit_on_crash_signals();
    let mut out = answer_channel();
    // Before any plugin code runs: proof that this is a scan helper speaking this protocol.
    if writeln!(out, "{HELLO}{PROTOCOL}").and_then(|()| out.flush()).is_err() {
        return Some(2);
    }
    let (line, ok) = match args.next() {
        Some(path) => answer(Path::new(&path)),
        None => (error_line("no bundle given"), false),
    };
    // On a line of its own, even after plugin output that did not end its line (Windows).
    let written = writeln!(out, "\n{line}").and_then(|()| out.flush()).is_ok();
    drop(out);
    // The module exits run here, on the thread that ran the entry, like the apps do at the end.
    crate::shutdown();
    Some(if ok && written { 0 } else { 2 })
}

/// Where the helper's own lines go. On Unix, a private copy of standard output, which itself now
/// leads to standard error: plugin code cannot write into the answers, nor lose them by
/// redirecting standard output. On Windows plugins share standard output with the answers; the
/// host then reads only lines that start with the marker and parse.
fn answer_channel() -> Box<dyn Write> {
    #[cfg(unix)]
    if let Some(private) = crate::ffi::private_stdout() {
        return Box::new(private);
    }
    Box::new(std::io::stdout())
}

/// Ends this helper as soon as the host named in [`HOST_ENV`] is gone: on Unix the helper then
/// has another parent. The host hands its id over because the helper cannot record its parent
/// itself: the host may be gone before the helper first looks. (On Windows the host's job object
/// ends its helpers instead.)
fn end_with_host() {
    #[cfg(unix)]
    {
        let Some(host) = std::env::var(HOST_ENV).ok().and_then(|v| v.parse::<u32>().ok()) else { return };
        let gone = move || std::os::unix::process::parent_id() != host;
        // The whole group: the helper and whatever its plugin started (no terminal signal reaches
        // them now that they have a group of their own).
        if gone() {
            crate::ffi::end_own_process_group();
        }
        let _ = std::thread::Builder::new().name("vst3-scan-host".into()).spawn(move || {
            loop {
                std::thread::sleep(HOST_POLL);
                if gone() {
                    crate::ffi::end_own_process_group();
                }
            }
        });
    }
}

/// The host side: scans each bundle in its own helper, at most `helper.parallel` at a time. One
/// result per bundle, in order: its plugins (with `path` set to the bundle) or why it was skipped.
///
/// A failure on SoundCraft's side ([`Skip::Host`]) stops the scan: no further helper is started, and
/// the bundles left are answered with that failure too.
pub(crate) fn scan_bundles(helper: &ScanHelper, bundles: &[PathBuf]) -> Vec<Result<Vec<Vst3Descriptor>, Skip>> {
    let slots: Vec<Mutex<Option<Result<Vec<Vst3Descriptor>, Skip>>>> = bundles.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let broken: Mutex<Option<String>> = Mutex::new(None);
    let work = || {
        loop {
            if broken.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
                break;
            }
            let i = next.fetch_add(1, Ordering::Relaxed);
            let (Some(path), Some(slot)) = (bundles.get(i), slots.get(i)) else { break };
            let r = scan_one(helper, path);
            if let Err(Skip::Host(why) | Skip::HostStuck(why)) = &r {
                broken.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| why.clone());
            }
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
        }
    };
    let workers = helper.parallel.clamp(1, MAX_PARALLEL).min(bundles.len());
    std::thread::scope(|s| {
        let mut started = 0;
        for _ in 0..workers {
            if std::thread::Builder::new().name("vst3-scan".into()).spawn_scoped(s, work).is_ok() {
                started += 1;
            }
        }
        if started == 0 {
            work();
        }
    });
    let why = broken.into_inner().unwrap_or_else(PoisonError::into_inner).unwrap_or_else(|| "not scanned".into());
    slots.into_iter().map(|slot| slot.into_inner().unwrap_or_else(PoisonError::into_inner).unwrap_or_else(|| Err(Skip::Host(why.clone())))).collect()
}

/// Runs one helper on `bundle` and waits for its answer, at most `helper.timeout`.
fn scan_one(helper: &ScanHelper, bundle: &Path) -> Result<Vec<Vst3Descriptor>, Skip> {
    let limit = helper.timeout.min(MAX_TIMEOUT);
    let mut running = Running::spawn(helper, bundle)?;
    let (ended, hello) = running.await_answer(limit);
    let tail = running.stderr_tail();
    if !tail.trim().is_empty() {
        log::debug!("vst3 scan helper for {}: {}", bundle.display(), tail.trim_end());
    }
    running.release();
    classify(ended, hello.as_deref(), &tail, bundle, limit, &helper.exe)
}

/// How a helper's run ended, before it is judged.
#[derive(Debug)]
enum Ended {
    /// It printed an answer, whatever happened next (the last one, if several).
    Answered(Answer),
    /// It exited without an answer.
    Exited(ExitStatus),
    /// It ran past its time limit without an answer, and was killed.
    TimedOut,
    /// Its state could not be read; it was killed.
    Lost,
}

/// A helper process and the threads reading its output.
struct Running {
    child: Child,
    /// Its job (Windows): a timeout ends it, the helper's children with it.
    #[cfg(windows)]
    job: Option<crate::ffi::HelperJob>,
    /// Killed but not reaped yet (see [`end`]).
    unreaped: bool,
    lines: mpsc::Receiver<Line>,
    err_tail: Arc<Mutex<Vec<u8>>>,
}

impl Running {
    fn spawn(helper: &ScanHelper, bundle: &Path) -> Result<Running, Skip> {
        let mut cmd = Command::new(&helper.exe);
        // A group of its own: a timeout kills the helper and whatever it started.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        // Run as /proc/self/exe on Linux, it would show as that in `ps`.
        #[cfg(target_os = "linux")]
        std::os::unix::process::CommandExt::arg0(&mut cmd, "soundcraft");
        cmd.arg(SCAN_HELPER_FLAG)
            .arg(bundle)
            .env(HOST_ENV, std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            /// No console window for a console-subsystem helper.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| Skip::Host(format!("cannot start the scan helper {}: {e}", helper.exe.display())))?;
        // Killed with this process, however it ends (Unix helpers watch their parent instead).
        #[cfg(windows)]
        crate::ffi::end_with_this_process(&child);
        // And in a job of its own (nested in that one), which a timeout ends.
        #[cfg(windows)]
        let job = crate::ffi::HelperJob::of(&child);
        let err_tail = drain_stderr(&mut child);
        let Some(lines) = read_lines(&mut child) else {
            if !end(&mut child) {
                reap_later(child);
            }
            return Err(Skip::Host("cannot read the scan helper's output".into()));
        };
        Ok(Running {
            child,
            #[cfg(windows)]
            job,
            unreaped: false,
            lines,
            err_tail,
        })
    }

    /// Waits for the answer, at most `limit`; once it is in, the helper has [`EXIT_GRACE`] to exit
    /// before it is killed (the answer counts either way). Also returns the protocol the helper
    /// greeted with, if it did.
    fn await_answer(&mut self, limit: Duration) -> (Ended, Option<String>) {
        let mut deadline = Instant::now() + limit;
        let mut answer: Option<Answer> = None;
        let mut hello: Option<String> = None;
        loop {
            while let Ok(line) = self.lines.try_recv() {
                match line {
                    Line::Hello(v) => hello = Some(v),
                    Line::Answer(a) => {
                        if answer.is_none() {
                            deadline = deadline.min(Instant::now() + EXIT_GRACE);
                        }
                        answer = Some(a);
                    }
                }
            }
            match self.child.try_wait() {
                // A helper that has just exited may still have lines in the pipe (the reader stops
                // waiting at the end of the output).
                Ok(Some(st)) => {
                    let answer = self.rest(&mut hello).or(answer);
                    return (answer.map_or(Ended::Exited(st), Ended::Answered), hello);
                }
                Ok(None) => {}
                Err(_) => {
                    self.end();
                    return (answer.map_or(Ended::Lost, Ended::Answered), hello);
                }
            }
            if Instant::now() >= deadline {
                self.end();
                // An answer that came in since the look above still counts.
                let answer = self.rest(&mut hello).or(answer);
                return (answer.map_or(Ended::TimedOut, Ended::Answered), hello);
            }
            std::thread::sleep(POLL);
        }
    }

    /// The lines still to come from a helper that has ended (until the end of its output, at most
    /// [`EXIT_GRACE`]): its last answer among them, if any.
    fn rest(&self, hello: &mut Option<String>) -> Option<Answer> {
        let until = Instant::now() + EXIT_GRACE;
        let mut answer = None;
        loop {
            match self.lines.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Line::Hello(v)) => *hello = Some(v),
                Ok(Line::Answer(a)) => answer = Some(a),
                Err(_) => return answer,
            }
        }
    }

    /// Done with the helper: one that was killed but is not reaped yet is reaped on a thread.
    /// Kills the helper, and on Windows its job (whatever it started), see [`end`].
    fn end(&mut self) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.end();
        }
        self.unreaped = !end(&mut self.child);
    }

    fn release(self) {
        if self.unreaped {
            reap_later(self.child);
        }
    }

    fn stderr_tail(&self) -> String {
        String::from_utf8_lossy(&self.err_tail.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

/// The plugins of a helper's run, or why there are none. `hello` is the protocol it greeted with,
/// `tail` the end of its standard error. A run without this protocol's greeting is SoundCraft's
/// failure ([`Skip::Host`]): an older or newer SoundCraft, or a program that is no scan helper.
fn classify(ended: Ended, hello: Option<&str>, tail: &str, bundle: &Path, limit: Duration, exe: &Path) -> Result<Vec<Vst3Descriptor>, Skip> {
    let why = |what: String| match last_line(tail) {
        Some(l) => format!("{what}; its last output: {l}"),
        None => what,
    };
    judge(ended, hello, why, bundle, limit, exe).map_err(|skip| match skip {
        Skip::Bundle(r) => Skip::Bundle(clean_reason(&r)),
        Skip::TimedOut(r) => Skip::TimedOut(clean_reason(&r)),
        Skip::Host(r) => Skip::Host(clean_reason(&r)),
        Skip::HostStuck(r) => Skip::HostStuck(clean_reason(&r)),
    })
}

fn judge(
    ended: Ended,
    hello: Option<&str>,
    why: impl Fn(String) -> String,
    bundle: &Path,
    limit: Duration,
    exe: &Path,
) -> Result<Vec<Vst3Descriptor>, Skip> {
    if hello != Some(PROTOCOL) {
        let how = match &ended {
            Ended::Answered(_) => "answered".to_string(),
            Ended::TimedOut => format!("timed out after {limit:?}"),
            Ended::Exited(st) => format!("exited ({st})"),
            Ended::Lost => "stopped".to_string(),
        };
        let greeting = hello.map_or_else(|| "no greeting".to_string(), |v| format!("protocol {v}"));
        let why = why(format!("{} is not a scan helper of protocol {PROTOCOL} ({greeting}; it {how})", exe.display()));
        // Still running at its limit, or lost: it may be running the bundle's code.
        return Err(if matches!(ended, Ended::TimedOut | Ended::Lost) { Skip::HostStuck(why) } else { Skip::Host(why) });
    }
    match ended {
        Ended::Answered(Answer::Ok(ds)) => Ok(well_formed(ds, bundle)),
        Ended::Answered(Answer::Error(e)) => Err(Skip::Bundle(e)),
        Ended::TimedOut => Err(Skip::TimedOut(why(format!("timed out after {limit:?}")))),
        Ended::Exited(st) => Err(Skip::Bundle(why(ended_reason(st)))),
        Ended::Lost => Err(Skip::HostStuck(why("the scan helper stopped without an answer".into()))),
    }
}

/// The descriptors whose id is `vst3:` and their class id, as a scan in this process makes them,
/// with `path` set to `bundle`; others are logged and dropped (they could never be created, or
/// would claim another host's ids). The fields derived from the sub-categories (category,
/// instrument or not) are derived again by this build, so a fix of that mapping reaches cached
/// plugins without a rescan. Their text is made fit for menus and the cache: control characters
/// become spaces, each field is cut to [`MAX_FIELD`] characters and the sub-categories to
/// [`MAX_SUB_CATEGORIES`].
pub(crate) fn well_formed(ds: Vec<Vst3Descriptor>, bundle: &Path) -> Vec<Vst3Descriptor> {
    let (good, bad): (Vec<_>, Vec<_>) =
        ds.into_iter().partition(|d| parse_id(&d.id).is_some() && d.id.strip_prefix(ID_PREFIX) == Some(d.class_id.as_str()));
    if let Some(d) = bad.first() {
        log::warn!("vst3: ignoring {} malformed plugin entries for {} (such as id {:?})", bad.len(), bundle.display(), clean_reason(&d.id));
    }
    good.into_iter()
        .map(|d| {
            let sub_categories: Vec<String> = d.sub_categories.iter().take(MAX_SUB_CATEGORIES).map(|s| clean_text(s, MAX_FIELD)).collect();
            Vst3Descriptor {
                path: bundle.to_path_buf(),
                category: crate::scan::category_from_sub_categories(&sub_categories),
                is_instrument: crate::scan::is_instrument(&sub_categories),
                name: clean_text(&d.name, MAX_FIELD),
                vendor: clean_text(&d.vendor, MAX_FIELD),
                version: clean_text(&d.version, MAX_FIELD),
                sdk_version: clean_text(&d.sdk_version, MAX_FIELD),
                sub_categories,
                ..d
            }
        })
        .collect()
}

/// `text` with its control characters (line breaks, terminal escapes) turned into spaces, cut to
/// `max` characters.
fn clean_text(text: &str, max: usize) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(max).collect()
}

/// A reason fit for a log line and a list: control characters (line breaks, terminal escapes)
/// become spaces, and it is cut to [`MAX_REASON`] characters.
pub(crate) fn clean_reason(reason: &str) -> String {
    clean_text(reason, MAX_REASON)
}

/// How a helper that never answered ended.
fn ended_reason(st: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            return format!("the scan helper crashed (signal {sig})");
        }
    }
    match st.code() {
        #[cfg(unix)]
        Some(crate::ffi::CRASHED_STATUS) => "the scan helper crashed (a fatal signal in plugin code)".into(),
        Some(c) => format!("the scan helper exited with status {c} without an answer"),
        None => format!("the scan helper ended without an answer ({st})"),
    }
}

/// The last non-empty line of `text`, shortened.
fn last_line(text: &str) -> Option<String> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty()).map(|l| l.chars().take(MAX_LAST_LINE).collect())
}

/// Kills a helper and, on Unix, everything it started (its process group: a plugin's own child
/// must not keep its output open), then reaps it, waiting at most [`REAP_WAIT`]. Returns whether
/// it was reaped; one that was not (stuck in the kernel, on a hung network volume) is for
/// [`reap_later`], so it cannot hold up the scan.
fn end(child: &mut Child) -> bool {
    #[cfg(unix)]
    crate::ffi::kill_process_group(child.id());
    let _ = child.kill();
    let until = Instant::now() + REAP_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) if Instant::now() >= until => return false,
            Ok(None) => std::thread::sleep(POLL),
        }
    }
}

/// Reaps a killed helper on a thread of its own, whenever it ends.
fn reap_later(mut child: Child) {
    let _ = std::thread::Builder::new().name("vst3-scan-reap".into()).spawn(move || {
        let _ = child.wait();
    });
}

/// A protocol line of a helper.
#[derive(Debug, PartialEq)]
enum Line {
    /// Its greeting: the protocol version.
    Hello(String),
    /// Its answer.
    Answer(Answer),
}

/// The protocol line `text` is: only a line that starts with a marker and, for an answer, parses.
/// (A plugin printing on standard output, on Windows where it shares it, cannot pass for one.)
fn protocol_line(text: &str) -> Option<Line> {
    if let Some(v) = text.strip_prefix(HELLO) {
        return Some(Line::Hello(v.trim_end().to_string()));
    }
    serde_json::from_str(text.strip_prefix(MARKER)?.trim_end()).ok().map(Line::Answer)
}

/// Reads the helper's standard output on a thread; its protocol lines come out of the receiver,
/// which disconnects at the end of the output.
fn read_lines(child: &mut Child) -> Option<mpsc::Receiver<Line>> {
    spawn_reader(child.stdout.take()?)
}

/// Reads protocol lines from `out` on a thread: at most [`MAX_STDOUT`] bytes are read for them;
/// the rest is drained unread, so the writer never blocks on a full pipe.
fn spawn_reader(out: impl Read + Send + 'static) -> Option<mpsc::Receiver<Line>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("vst3-scan-out".into())
        .spawn(move || {
            let mut reader = BufReader::new(out);
            let mut kept = (&mut reader).take(MAX_STDOUT);
            let mut line = Vec::new();
            loop {
                line.clear();
                match kept.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
                if let Some(line) = protocol_line(&String::from_utf8_lossy(&line)) {
                    let _ = tx.send(line);
                }
            }
            // Keep the pipe flowing, so the helper never blocks on a full one.
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        })
        .ok()?;
    Some(rx)
}

/// Drains the helper's standard error on a thread, keeping its end.
fn drain_stderr(child: &mut Child) -> Arc<Mutex<Vec<u8>>> {
    let tail = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut err) = child.stderr.take() {
        let keep = tail.clone();
        // Without the thread the pipe is closed and the helper's writes to it fail; no harm.
        let _ = std::thread::Builder::new().name("vst3-scan-err".into()).spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut t = keep.lock().unwrap_or_else(PoisonError::into_inner);
                        t.extend_from_slice(buf.get(..n).unwrap_or_default());
                        let excess = t.len().saturating_sub(STDERR_TAIL);
                        t.drain(..excess);
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
    }
    tail
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_dsp::Category;

    fn descriptor(name: &str, path: &str) -> Vst3Descriptor {
        Vst3Descriptor {
            id: "vst3:5C0A1D00111122223333444455556666".into(),
            class_id: "5C0A1D00111122223333444455556666".into(),
            name: name.into(),
            vendor: "Vendor \"quoted\" ünïcode".into(),
            version: "1.2.3".into(),
            sdk_version: "VST 3.8.0".into(),
            sub_categories: vec!["Instrument".into(), "Synth".into()],
            category: Category::Instrument,
            is_instrument: true,
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn answers_round_trip_through_json() {
        let ds = vec![descriptor("Synth", "/Library/Audio/Plug-Ins/VST3/A b.vst3"), descriptor("Other\nline", "C:\\VST3\\x.vst3")];
        let line = format!("{MARKER}{}", serde_json::to_string(&Answer::Ok(ds.clone())).unwrap());
        assert!(!line.contains('\n'), "one line: {line}");
        assert_eq!(protocol_line(&line), Some(Line::Answer(Answer::Ok(ds))), "every field, the path included");
        assert_eq!(protocol_line(&error_line("bundleEntry failed")), Some(Line::Answer(Answer::Error("bundleEntry failed".into()))));
        assert_eq!(protocol_line(&format!("{HELLO}{PROTOCOL}\n")), Some(Line::Hello(PROTOCOL.into())));
    }

    #[test]
    fn only_whole_parseable_marker_lines_are_answers() {
        let good = error_line("x");
        assert!(protocol_line(&format!("{good}\n")).is_some());
        // A plugin's chatter that mentions the marker, or imitates it badly, is no answer.
        assert_eq!(protocol_line(&format!("[plugin] {good}")), None);
        assert_eq!(protocol_line(&format!("{MARKER}{{not json")), None);
        assert_eq!(protocol_line(&format!("{MARKER}{{\"ok\":[{{\"id\":1}}]}}")), None);
        assert_eq!(protocol_line(""), None);
        assert_eq!(protocol_line(&format!(" {HELLO}{PROTOCOL}")), None);
    }

    #[test]
    fn a_missing_bundle_answers_an_error() {
        let (line, ok) = answer(Path::new("/nonexistent/Missing.vst3"));
        assert!(!ok);
        let Some(Line::Answer(Answer::Error(e))) = protocol_line(&line) else { panic!("{line}") };
        assert!(e.contains("Missing.vst3"), "{e}");
    }

    #[test]
    fn other_arguments_are_not_a_scan_request() {
        assert_eq!(run(Vec::<OsString>::new()), None);
        assert_eq!(run(["--demo".into()]), None);
        assert_eq!(run(["--soundcraft-scan-clap".into(), "x".into()]), None);
    }

    #[test]
    fn a_huge_time_limit_does_not_overflow() {
        // This test binary as the helper: it rejects the flag and exits without a greeting.
        let helper = ScanHelper { timeout: Duration::MAX, ..ScanHelper::new(std::env::current_exe().unwrap()) };
        let r = scan_bundles(&helper, &[PathBuf::from("/x.vst3")]);
        assert!(matches!(r.as_slice(), [Err(Skip::Host(e))] if e.contains("no greeting")), "{r:?}");
    }

    #[test]
    fn malformed_plugins_and_reasons_are_cleaned() {
        let bundle = Path::new("/p/A.vst3");
        let good = descriptor("Good", "/elsewhere");
        let other_host = Vst3Descriptor { id: "clap:com.x".into(), ..good.clone() };
        let mismatched = Vst3Descriptor { id: "vst3:00000000000000000000000000000000".into(), ..good.clone() };
        let not_hex = Vst3Descriptor { id: "vst3:ZZ".into(), class_id: "ZZ".into(), ..good.clone() };
        let r = classify(
            Ended::Answered(Answer::Ok(vec![other_host, good.clone(), mismatched, not_hex])),
            Some(PROTOCOL),
            "",
            bundle,
            Duration::ZERO,
            bundle,
        );
        assert_eq!(r, Ok(vec![Vst3Descriptor { path: bundle.to_path_buf(), ..good }]));
        let hostile = format!("bad\n\u{1b}[31mred{}", "x".repeat(8 << 20));
        let Err(Skip::Bundle(why)) = classify(Ended::Answered(Answer::Error(hostile)), Some(PROTOCOL), "", bundle, Duration::ZERO, bundle) else {
            panic!()
        };
        assert!(why.chars().count() <= MAX_REASON && why.starts_with("bad  [31mred"), "{why:?}");
        let Err(Skip::TimedOut(why)) = classify(Ended::TimedOut, Some(PROTOCOL), "line one\nlast \u{7}line", bundle, Duration::from_secs(1), bundle)
        else {
            panic!()
        };
        assert_eq!(why, "timed out after 1s; its last output: last  line");
    }

    #[test]
    fn plugin_text_is_capped_and_cleaned() {
        let bundle = Path::new("/p/A.vst3");
        let huge = format!("Name\n\u{1b}[2J{}", "n".repeat(8 << 20));
        let wild = Vst3Descriptor {
            name: huge.clone(),
            vendor: huge.clone(),
            version: huge.clone(),
            sdk_version: huge,
            sub_categories: (0..1000).map(|i| format!("Cat{i}\t")).collect(),
            ..descriptor("x", "/p")
        };
        let [d] = well_formed(vec![wild], bundle).try_into().unwrap();
        for field in [&d.name, &d.vendor, &d.version, &d.sdk_version] {
            assert!(field.chars().count() == MAX_FIELD && field.starts_with("Name  [2J"), "{:?}", &field[..20]);
        }
        assert_eq!(d.sub_categories.len(), MAX_SUB_CATEGORIES);
        assert_eq!(d.sub_categories[0], "Cat0 ");
    }

    #[test]
    fn the_reader_reads_up_to_its_cap_and_drains_the_rest() {
        let (out, mut writer) = std::io::pipe().unwrap();
        let lines = spawn_reader(out).unwrap();
        let writing = std::thread::spawn(move || -> std::io::Result<()> {
            writeln!(writer, "{HELLO}{PROTOCOL}")?;
            // More than the cap, in one endless line, then an answer past the cap.
            let junk = vec![b'x'; 1 << 20];
            for _ in 0..(MAX_STDOUT >> 20) + 4 {
                writer.write_all(&junk)?;
            }
            writeln!(writer, "\n{}", error_line("past the cap"))
        });
        // The writer is never left blocked on a full pipe: the reader drains past its cap.
        let started = Instant::now();
        while !writing.is_finished() && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(writing.is_finished(), "the writer blocked: the rest was not drained");
        assert!(writing.join().unwrap().is_ok());
        let read: Vec<Line> = lines.iter().collect();
        assert_eq!(read, vec![Line::Hello(PROTOCOL.into())], "nothing past the cap is read");
    }

    #[test]
    fn last_lines_are_trimmed_and_shortened() {
        assert_eq!(last_line("a\nThrift: connect()  \n\n  "), Some("Thrift: connect()".to_string()));
        assert_eq!(last_line(" \n"), None);
        assert_eq!(last_line(&"x".repeat(1000)).map(|l| l.len()), Some(MAX_LAST_LINE));
    }
}
