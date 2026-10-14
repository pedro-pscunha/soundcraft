//! Out-of-process scanning: one helper process per bundle, a few at a time, each with a time limit.
//! A plugin that never returns from its module entry (a PACE/iLok-protected plugin without its
//! licence service) or crashes there then costs a skipped bundle, not the whole application.
//!
//! The protocol: `<exe> --soundcraft-scan-vst3 <bundle>` loads that one bundle in the helper (the
//! in-process [`load_bundle`](crate::load_bundle)), prints one line, `soundcraft-vst3-scan: ` and
//! then JSON, `{"ok":[descriptors]}` or `{"error":"why"}`, and exits: 0 when the bundle loaded, 2
//! when not. The host finds that line wherever it is in standard output (plugins may print there
//! too). The helper's standard error, where plugins chatter, is drained; its last line goes into
//! the reason a bundle is skipped, the rest to the debug log. Once the answer is in, the helper has
//! [`EXIT_GRACE`] to exit (module exits, static destructors) before it is killed; the answer
//! counts either way. A helper whose host is gone (killed mid-scan) ends itself on Unix (the host
//! hands it its process id in `SOUNDCRAFT_SCAN_HOST_PID`), so a plugin stuck in its module entry
//! cannot keep it alive forever.

use crate::{SCAN_HELPER_FLAG, ScanHelper, Vst3Descriptor};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

/// Starts the answer line.
const MARKER: &str = "soundcraft-vst3-scan: ";
/// Most of a helper's standard output read for the answer (the rest is drained unread).
const MAX_STDOUT: u64 = 8 << 20;
/// How much of the end of a helper's standard error is kept.
const STDERR_TAIL: usize = 8 << 10;
/// Most characters of the helper's last error line put into a skip reason.
const MAX_LAST_LINE: usize = 300;
/// How long a helper may take to exit once it has answered.
const EXIT_GRACE: Duration = Duration::from_secs(2);
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
    /// The bundle's doing: its helper timed out, crashed, or the bundle did not load. Kept in the
    /// scan cache across runs.
    Bundle(String),
    /// SoundCraft's side: the helper could not be started or read. Not kept across runs.
    Host(String),
}

impl Skip {
    pub(crate) fn reason(&self) -> &str {
        match self {
            Skip::Bundle(r) | Skip::Host(r) => r,
        }
    }
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
    let (line, ok) = match args.next() {
        Some(path) => answer(Path::new(&path)),
        None => (error_line("no bundle given"), false),
    };
    let mut out = std::io::stdout().lock();
    // On a line of its own, even after plugin output that did not end its line.
    let written = writeln!(out, "\n{line}").and_then(|()| out.flush()).is_ok();
    drop(out);
    // The module exits run here, on the thread that ran the entry, like the apps do at the end.
    crate::shutdown();
    Some(if ok && written { 0 } else { 2 })
}

/// Ends this helper as soon as the host named in [`HOST_ENV`] is gone: on Unix the helper then
/// has another parent. (The host may be gone before the helper first looks, so its parent then
/// cannot tell it apart.) On Windows a helper whose host died runs until its bundle is scanned.
fn end_with_host() {
    #[cfg(unix)]
    {
        let Some(host) = std::env::var(HOST_ENV).ok().and_then(|v| v.parse::<u32>().ok()) else { return };
        let gone = move || std::os::unix::process::parent_id() != host;
        if gone() {
            crate::ffi::exit_now(3);
        }
        let _ = std::thread::Builder::new().name("vst3-scan-host".into()).spawn(move || {
            loop {
                std::thread::sleep(HOST_POLL);
                if gone() {
                    crate::ffi::exit_now(3);
                }
            }
        });
    }
}

/// The host side: scans each bundle in its own helper, at most `helper.parallel` at a time. One
/// result per bundle, in order: its plugins (with `path` set to the bundle) or why it was skipped.
pub(crate) fn scan_bundles(helper: &ScanHelper, bundles: &[PathBuf]) -> Vec<Result<Vec<Vst3Descriptor>, Skip>> {
    let slots: Vec<Mutex<Option<Result<Vec<Vst3Descriptor>, Skip>>>> = bundles.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let work = || {
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let (Some(path), Some(slot)) = (bundles.get(i), slots.get(i)) else { break };
            let r = scan_one(helper, path);
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
    slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap_or_else(PoisonError::into_inner).unwrap_or_else(|| Err(Skip::Host("not scanned".into()))))
        .collect()
}

/// Runs one helper on `bundle` and waits for its answer, at most `helper.timeout`.
fn scan_one(helper: &ScanHelper, bundle: &Path) -> Result<Vec<Vst3Descriptor>, Skip> {
    let mut cmd = Command::new(&helper.exe);
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
    let err_tail = drain_stderr(&mut child);
    let Some(answers) = read_answers(&mut child) else {
        end(&mut child);
        return Err(Skip::Host("cannot read the scan helper's output".into()));
    };
    let mut deadline = Instant::now() + helper.timeout.min(MAX_TIMEOUT);
    let mut answer: Option<String> = None;
    let mut timed_out = false;
    let status = loop {
        if answer.is_none()
            && let Ok(a) = answers.try_recv()
        {
            answer = Some(a);
            deadline = deadline.min(Instant::now() + EXIT_GRACE);
        }
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(_) => {
                end(&mut child);
                break None;
            }
        }
        if Instant::now() >= deadline {
            timed_out = answer.is_none();
            end(&mut child);
            break None;
        }
        std::thread::sleep(POLL);
    };
    // A helper that has just exited may still have its answer in the pipe (the reader stops
    // waiting at the end of the output).
    if answer.is_none() && !timed_out {
        answer = answers.recv_timeout(EXIT_GRACE).ok();
    }
    let tail = String::from_utf8_lossy(&err_tail.lock().unwrap_or_else(PoisonError::into_inner)).into_owned();
    if !tail.trim().is_empty() {
        log::debug!("vst3 scan helper for {}: {}", bundle.display(), tail.trim_end());
    }
    let why = |what: String| match last_line(&tail) {
        Some(l) => format!("{what}; its last output: {l}"),
        None => what,
    };
    match (answer, status) {
        (Some(a), _) => {
            parse_answer(&a).map(|ds| ds.into_iter().map(|d| Vst3Descriptor { path: bundle.to_path_buf(), ..d }).collect()).map_err(Skip::Bundle)
        }
        (None, _) if timed_out => Err(Skip::Bundle(why(format!("timed out after {:?}", helper.timeout)))),
        (None, Some(st)) => Err(Skip::Bundle(why(ended(st)))),
        (None, None) => Err(Skip::Host(why("the scan helper stopped without an answer".into()))),
    }
}

/// The plugins in an answer (the text after [`MARKER`]), or why there are none.
fn parse_answer(json: &str) -> Result<Vec<Vst3Descriptor>, String> {
    match serde_json::from_str::<Answer>(json) {
        Ok(Answer::Ok(ds)) => Ok(ds),
        Ok(Answer::Error(e)) => Err(e),
        Err(e) => Err(format!("unreadable answer from the scan helper: {e}")),
    }
}

/// How a helper that never answered ended.
fn ended(st: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            return format!("the scan helper crashed (signal {sig})");
        }
    }
    match st.code() {
        Some(c) => format!("the scan helper exited with status {c} without an answer"),
        None => format!("the scan helper ended without an answer ({st})"),
    }
}

/// The last non-empty line of `text`, shortened.
fn last_line(text: &str) -> Option<String> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty()).map(|l| l.chars().take(MAX_LAST_LINE).collect())
}

fn end(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads the helper's standard output on a thread; answers (the text after [`MARKER`]) come out of
/// the receiver, which disconnects at the end of the output.
fn read_answers(child: &mut Child) -> Option<mpsc::Receiver<String>> {
    let out = child.stdout.take()?;
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
                if let Some((_, json)) = String::from_utf8_lossy(&line).split_once(MARKER) {
                    let _ = tx.send(json.trim_end().to_string());
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
        let json = line.split_once(MARKER).unwrap().1;
        assert_eq!(parse_answer(json), Ok(ds), "every field, the path included");
        let err = error_line("bundleEntry failed");
        assert_eq!(parse_answer(err.split_once(MARKER).unwrap().1), Err("bundleEntry failed".to_string()));
        assert!(parse_answer("{\"ok\":[{\"id\":1}]}").unwrap_err().starts_with("unreadable answer"));
        assert!(parse_answer("").is_err());
    }

    #[test]
    fn a_missing_bundle_answers_an_error() {
        let (line, ok) = answer(Path::new("/nonexistent/Missing.vst3"));
        assert!(!ok);
        let e = parse_answer(line.strip_prefix(MARKER).unwrap()).unwrap_err();
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
        // This test binary as the helper: it rejects the flag and exits without an answer.
        let helper = ScanHelper { timeout: Duration::MAX, ..ScanHelper::new(std::env::current_exe().unwrap()) };
        let r = scan_bundles(&helper, &[PathBuf::from("/x.vst3")]);
        assert!(matches!(r.as_slice(), [Err(Skip::Bundle(e))] if e.contains("without an answer")), "{r:?}");
    }

    #[test]
    fn last_lines_are_trimmed_and_shortened() {
        assert_eq!(last_line("a\nThrift: connect()  \n\n  "), Some("Thrift: connect()".to_string()));
        assert_eq!(last_line(" \n"), None);
        assert_eq!(last_line(&"x".repeat(1000)).map(|l| l.len()), Some(MAX_LAST_LINE));
    }
}
