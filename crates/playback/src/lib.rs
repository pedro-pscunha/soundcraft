//! Realtime playback through the system audio device (cpal: CoreAudio, WASAPI, ALSA/Pulse/
//! PipeWire via ALSA, JACK, WebAudio).
//!
//! The UI thread owns a [`Player`]; the audio callback owns the [`MixEngine`] and a snapshot of
//! the session (`Arc<Session>`) that the UI swaps whenever the document changes. Commands go to
//! the callback through a channel; position and meters come back through atomics and a
//! `try_lock`ed snapshot, so the callback never blocks. When no device is available the player
//! falls back to a silent clock thread so the transport, meters and automation still run.
//!
//! Surround: the device is opened with as many output channels as it offers, up to the width of
//! the session's main format. Main channels (SMPTE/WAV order) go to device channels in order; when
//! the device has fewer channels than the main mix, the mix is folded down to stereo (ITU-R
//! BS.775) on the first two device channels.
//!
//! Plugins and threads: hosted third-party plugins (CLAP/VST3) are created, given their stored
//! state and prepared on the thread that calls [`Player::update_session`] (the UI thread), and
//! travel to the audio thread together with the session snapshot that needs them; the mix
//! engine adopts them (see `soundcraft_mix`'s crate docs). Instances the engine retires, and the
//! session snapshots the callback replaces, come back on return channels and are dropped on the UI
//! thread ([`Player::idle`]). Plugin editors are driven, plugin states for saving read
//! ([`Player::capture_states`]), and the work a plugin format leaves to the UI thread done every
//! frame (a VST3 edit controller learns the values the host set; [`Player::idle`]), on the UI
//! thread through handles taken from each instance before it leaves ([`Player::open_editor`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod devices;
pub mod record;

pub use devices::AudioConfig;

use soundcraft_dsp::{Plugin, PluginEditor, PluginStateReader};
use soundcraft_mix::{MAX_CHANNELS, MixEngine, PreparedInstance, StripMeter, itu_stereo_fold, main_channels};
use soundcraft_model::{Insert, Session, Track, TrackId};
use soundcraft_time::{Range, Samples};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};

const BLOCK: usize = 512;

/// Room for replaced snapshots on their way back; `update_session` empties it before each send.
const OLD_SESSIONS: usize = 16;

/// The realtime audio thread marker lives in the mix engine, which renders every strip on the
/// marked thread itself; the cpal callbacks here set it.
pub use soundcraft_mix::{mark_audio_thread, on_audio_thread};

/// Meter values for the UI.
#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub tracks: HashMap<TrackId, StripMeter>,
    pub main: StripMeter,
}

/// A live third-party plugin's state, read for saving ([`Player::capture_states`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedState {
    pub track: TrackId,
    /// The insert slot, or [`soundcraft_mix::INSTRUMENT_SLOT`].
    pub slot: usize,
    pub state: Vec<u8>,
    /// The insert's parameter overrides the plugin has changed by itself, without telling the
    /// host per parameter (a preset loaded in its own editor), with their new values. Stored with
    /// the state, so the old values are not applied over it when the session is loaded.
    pub params: Vec<(String, f32)>,
}

/// Plugin states read from the live instances.
pub type PluginStates = Vec<CapturedState>;

/// A parameter edit made in a plugin's own editor: `(track, slot, param id, value)`.
pub type EditorEdit = (TrackId, usize, String, f32);

enum Cmd {
    /// A new snapshot plus the third-party instances it needs (created on the UI thread).
    Session(Arc<Session>, Vec<PreparedInstance>),
    /// Where the callback sends the snapshots it replaces (freeing one takes thousands of frees).
    OldSessions(SyncSender<Arc<Session>>),
    Input(Arc<record::InputRing>),
    Recording(bool),
    Play {
        from: Samples,
        end: Option<Samples>,
        looped: Option<Range>,
    },
    Stop,
    Locate(Samples),
}

struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    /// Speed × 1000.
    speed: AtomicU32,
    meters: Mutex<MeterSnapshot>,
}

/// What stopped the audio output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTrouble {
    /// A block panicked; the output stays silent.
    Bug,
    /// The device went away.
    Gone,
    /// The device stopped asking for audio while playing.
    Stalled,
}

/// How the audio output is doing, for the UI.
#[derive(Default)]
struct Fault {
    /// A block panicked: output stays silent from then on.
    failed: AtomicBool,
    gone: AtomicBool,
    /// Blocks so far, to tell a device that stopped asking for audio.
    beats: std::sync::atomic::AtomicU64,
    /// The UI has been told.
    reported: AtomicBool,
}

impl Fault {
    /// What stopped the output, once.
    fn trouble(&self, stalled: bool) -> Option<OutputTrouble> {
        let trouble = if self.failed.load(Ordering::Relaxed) {
            OutputTrouble::Bug
        } else if self.gone.load(Ordering::Relaxed) {
            OutputTrouble::Gone
        } else if stalled {
            OutputTrouble::Stalled
        } else {
            return None;
        };
        (!self.reported.swap(true, Ordering::Relaxed)).then_some(trouble)
    }
}

/// How long a playing output may go without a block before it counts as stalled.
#[cfg(not(target_arch = "wasm32"))]
const STALL: std::time::Duration = std::time::Duration::from_secs(2);

/// True once the block count `beats` has stood still for `STALL` while playing; `seen` is the
/// count last seen and since when.
#[cfg(not(target_arch = "wasm32"))]
fn stalled(seen: &mut Option<(u64, std::time::Instant)>, beats: u64, playing: bool) -> bool {
    match *seen {
        Some((b, since)) if playing && b == beats => since.elapsed() >= STALL,
        _ => {
            *seen = Some((beats, std::time::Instant::now()));
            false
        }
    }
}

/// Fill one block with `fill`. If it panics, the block and every later one are silent and the
/// transport stops, rather than the panic taking the app down.
fn guarded_block(fault: &Fault, shared: &Shared, out: &mut [f32], fill: impl FnOnce(&mut [f32])) {
    fault.beats.fetch_add(1, Ordering::Relaxed);
    if fault.failed.load(Ordering::Relaxed) || std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fill(out))).is_err() {
        fault.failed.store(true, Ordering::Relaxed);
        shared.playing.store(false, Ordering::Relaxed);
        out.fill(0.0);
    }
}

/// State owned by the audio callback.
struct AudioState {
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    session: Arc<Session>,
    mix: MixEngine,
    pos: Samples,
    playing: bool,
    end: Option<Samples>,
    looped: Option<Range>,
    /// Rendered frames waiting to be consumed (planar, main channels) and the fractional read index.
    pending: Vec<Vec<f32>>,
    /// Stereo fold-down gains per main channel (used when the device has fewer channels).
    fold: [(f32, f32); MAX_CHANNELS],
    pending_len: usize,
    read: f64,
    device_rate: f64,
    input: Option<Arc<record::InputRing>>,
    recording: bool,
    /// Retired plugin instances go back to the UI thread to be dropped there.
    garbage: Sender<Vec<Box<dyn Plugin>>>,
    old_sessions: Option<SyncSender<Arc<Session>>>,
}

fn new_engine(sr: f32) -> MixEngine {
    let mut m = MixEngine::new(sr, BLOCK);
    m.set_external_instances(true);
    m.metronome = true;
    m
}

impl AudioState {
    fn new(rx: Receiver<Cmd>, shared: Arc<Shared>, session: Arc<Session>, device_rate: f64, garbage: Sender<Vec<Box<dyn Plugin>>>) -> Self {
        let sr = session.sample_rate.as_f64() as f32;
        let nch = main_channels(&session);
        AudioState {
            garbage,
            old_sessions: None,
            fold: fold_gains(&session),
            rx,
            shared,
            mix: new_engine(sr),
            session,
            pos: 0,
            playing: false,
            end: None,
            looped: None,
            pending: vec![vec![0.0; BLOCK]; nch],
            pending_len: 0,
            read: 0.0,
            device_rate,
            input: None,
            recording: false,
        }
    }

    fn drain(&mut self) {
        let mut changed = false;
        while let Ok(c) = self.rx.try_recv() {
            match c {
                Cmd::Input(r) => self.input = Some(r),
                Cmd::Recording(r) => self.recording = r,
                Cmd::Session(s, instances) => {
                    changed = true;
                    if (s.sample_rate.as_f64() - self.session.sample_rate.as_f64()).abs() > 0.5 {
                        let mut old = std::mem::replace(&mut self.mix, new_engine(s.sample_rate.as_f64() as f32));
                        let _ = self.garbage.send(old.drain_plugins());
                    }
                    self.mix.adopt(instances);
                    // The main format changed (a document edit, not steady state): resize.
                    let nch = main_channels(&s);
                    if self.pending.len() != nch {
                        self.pending = vec![vec![0.0; BLOCK]; nch];
                        self.pending_len = 0;
                        self.read = 0.0;
                    }
                    self.fold = fold_gains(&s);
                    let old = std::mem::replace(&mut self.session, s);
                    if let Some(tx) = &self.old_sessions {
                        let _ = tx.try_send(old);
                    }
                }
                Cmd::OldSessions(tx) => self.old_sessions = Some(tx),
                Cmd::Play { from, end, looped } => {
                    self.pos = from;
                    self.end = end;
                    self.looped = looped.filter(|r| r.len() > 64);
                    self.playing = true;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.mix.reset();
                }
                Cmd::Stop => {
                    self.playing = false;
                    self.end = None;
                    self.looped = None;
                    self.mix.reset();
                }
                Cmd::Locate(at) => {
                    self.pos = at;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.mix.reset();
                }
            }
        }
        if changed {
            // Place adopted instances now (not only once playback starts), so editors see them;
            // `sync` creates no third-party plugins here.
            self.mix.ensure_synced(&self.session);
        }
        self.send_garbage();
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }

    fn send_garbage(&mut self) {
        let g = self.mix.take_retired();
        if !g.is_empty() {
            let _ = self.garbage.send(g);
        }
    }

    /// Render the next block at the session rate into `pending`.
    fn render_next(&mut self, quantum: usize) {
        let mut n = quantum.clamp(1, BLOCK);
        if let Some(l) = self.looped
            && self.pos >= l.end
        {
            self.pos = l.start;
        }
        if let Some(l) = self.looped {
            n = n.min(usize::try_from((l.end - self.pos).max(1)).unwrap_or(BLOCK));
        } else if let Some(e) = self.end {
            if self.pos >= e {
                self.playing = false;
                self.end = None;
                self.shared.playing.store(false, Ordering::Relaxed);
                n = 0;
            } else {
                n = n.min(usize::try_from(e - self.pos).unwrap_or(BLOCK));
            }
        }
        if n == 0 {
            for c in &mut self.pending {
                c.iter_mut().for_each(|x| *x = 0.0);
            }
            self.pending_len = BLOCK;
            return;
        }
        if let Some(ring) = &self.input {
            let ch = ring.channels.load(Ordering::Relaxed).max(1);
            if self.mix.input.len() != ch {
                self.mix.input = vec![vec![0.0; BLOCK]; ch];
            }
            ring.pop_into(&mut self.mix.input, n, self.session.sample_rate.hz());
        }
        self.mix.monitor_only = !self.playing;
        self.mix.recording = self.recording;
        self.mix.render(&self.session, self.pos, n, &mut self.pending);
        self.send_garbage();
        self.pending_len = n;
        if let Ok(mut m) = self.shared.meters.try_lock() {
            m.main.copy_from(&self.mix.main_meter);
            for (k, v) in &self.mix.meters {
                match m.tracks.get_mut(k) {
                    Some(d) => d.copy_from(v),
                    None => {
                        m.tracks.insert(*k, v.clone());
                    }
                }
            }
        }
        if self.playing {
            self.pos += n as i64;
            self.shared.position.store(self.pos, Ordering::Relaxed);
        }
    }

    /// Fill an interleaved device buffer.
    fn fill(&mut self, out: &mut [f32], channels: usize) {
        self.drain();
        let channels = channels.max(1);
        let monitoring = self.input.is_some() && self.session.tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm);
        if !self.playing && !monitoring {
            out.iter_mut().for_each(|x| *x = 0.0);
            if let Ok(mut m) = self.shared.meters.try_lock() {
                m.main = StripMeter::default();
                m.tracks.clear();
            }
            return;
        }
        let speed = f64::from(self.shared.speed.load(Ordering::Relaxed)) / 1000.0;
        let step = self.session.sample_rate.as_f64() / self.device_rate.max(1.0) * speed.clamp(0.1, 4.0);
        let callback_frames = out.len() / channels;
        let quantum = ((callback_frames as f64 * step).ceil() as usize).clamp(1, BLOCK);
        for frame in out.chunks_mut(channels) {
            while self.read >= self.pending_len as f64 {
                self.read -= self.pending_len as f64;
                if !self.playing && !monitoring {
                    break;
                }
                self.render_next(quantum);
                if self.pending_len == 0 {
                    break;
                }
            }
            let i0 = self.read.floor() as usize;
            let fr = (self.read - i0 as f64) as f32;
            let sounding = self.playing || monitoring;
            let nch = self.pending.len();
            let sample = |c: usize| {
                let ch = self.pending.get(c);
                let a = ch.and_then(|v| v.get(i0)).copied().unwrap_or(0.0);
                let b = ch.and_then(|v| v.get(i0 + 1)).copied().unwrap_or(a);
                a + (b - a) * fr
            };
            if nch > 2 && channels < nch {
                // Fewer device channels than the main mix: ITU fold-down to stereo.
                let (mut l, mut r) = (0.0f32, 0.0f32);
                if sounding {
                    for (c, (gl, gr)) in self.fold.iter().enumerate().take(nch) {
                        let v = sample(c);
                        l += v * gl;
                        r += v * gr;
                    }
                }
                for (c, o) in frame.iter_mut().enumerate() {
                    *o = match c {
                        0 => l.clamp(-1.0, 1.0),
                        1 => r.clamp(-1.0, 1.0),
                        _ => 0.0,
                    };
                }
            } else {
                for (c, o) in frame.iter_mut().enumerate() {
                    // Stereo mixes on a mono device play the left channel (as before).
                    *o = if c < nch.max(1) && sounding { sample(c).clamp(-1.0, 1.0) } else { 0.0 };
                }
            }
            self.read += step;
        }
    }
}

/// Where the plugin in a slot stands with its own editor ([`Player::editor_state`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorState {
    /// No instance was asked for: a built-in plugin, or a hosted one the player has not been
    /// given yet ([`Player::update_session`]).
    NotLoaded,
    /// The hosted plugin could not be made (not installed, or it failed to load).
    Failed,
    /// The instance has no editor of its own.
    NoEditor,
    Closed,
    Open,
}

/// Why [`Player::open_editor`] opened nothing. The UI puts it in words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorError {
    /// The slot's plugin has no editor to open: where it stands ([`EditorState::NotLoaded`],
    /// [`EditorState::Failed`] or [`EditorState::NoEditor`]).
    Unavailable(EditorState),
    /// The plugin's editor failed to open, as its plugin host reports it.
    Failed(String),
}

/// What was last created for a slot.
#[derive(Debug, PartialEq)]
struct Live {
    id: String,
    ch: usize,
    /// False when the plugin could not be made (remembered, not retried every update).
    created: bool,
    /// The [`Insert::state_epoch`] it was made from: a new one means a new state to restore.
    epoch: u64,
}

/// UI-thread bookkeeping of the third-party instances the audio thread holds.
struct Side {
    /// What was last created for each slot.
    shadow: HashMap<(TrackId, usize), Live>,
    /// Session sample rate the instances were prepared for.
    rate: f32,
    /// Editor handles of the live instances.
    editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>>,
    /// State readers of the live instances.
    states: BTreeMap<(TrackId, usize), Box<dyn PluginStateReader>>,
    /// Instances that refused their stored state: saves keep that state until the plugin's editor
    /// is opened.
    refused: HashSet<(TrackId, usize)>,
    garbage: Receiver<Vec<Box<dyn Plugin>>>,
    old_sessions: Option<Receiver<Arc<Session>>>,
    /// Edits a forgotten editor still held, with the plugin id and state epoch they were made for:
    /// reported once the slot is made again for the same plugin and state (a new sample rate or
    /// channel count), dropped when it holds another plugin or state, or nothing.
    orphaned: Vec<(String, u64, EditorEdit)>,
}

impl Side {
    fn new(rate: f32, garbage: Receiver<Vec<Box<dyn Plugin>>>) -> Side {
        Side {
            shadow: HashMap::new(),
            rate,
            editors: HashMap::new(),
            states: BTreeMap::new(),
            refused: HashSet::new(),
            garbage,
            old_sessions: None,
            orphaned: Vec::new(),
        }
    }

    /// Drops retired instances (closing their editors first, inside the plugin's drop) and
    /// replaced session snapshots.
    fn collect_garbage(&mut self) {
        while let Ok(g) = self.garbage.try_recv() {
            for p in g {
                log::debug!("plugin instance released: {}", p.info().name);
                drop(p);
            }
        }
        if let Some(rx) = &self.old_sessions {
            rx.try_iter().for_each(drop);
        }
    }

    fn return_old_sessions(&mut self, tx: &Sender<Cmd>) {
        let (otx, orx) = sync_channel(OLD_SESSIONS);
        self.old_sessions = Some(orx);
        let _ = tx.send(Cmd::OldSessions(otx));
    }

    fn forget(&mut self, key: (TrackId, usize)) {
        let live = self.shadow.remove(&key);
        self.states.remove(&key);
        self.refused.remove(&key);
        if let Some(mut e) = self.editors.remove(&key) {
            // Edits made in the editor's last frame (see `orphaned`).
            if let Some(l) = live {
                self.orphaned.extend(e.idle().into_iter().map(|(id, v)| (l.id.clone(), l.epoch, (key.0, key.1, id, v))));
            }
            e.close();
        }
    }

    /// The orphaned edits whose slot holds the same plugin and state again (see `orphaned`);
    /// the others are dropped.
    fn take_orphaned(&mut self) -> Vec<EditorEdit> {
        let shadow = &self.shadow;
        self.orphaned
            .drain(..)
            .filter(|(id, epoch, (t, slot, ..))| shadow.get(&(*t, *slot)).is_some_and(|l| l.id == *id && l.epoch == *epoch))
            .map(|(_, _, e)| e)
            .collect()
    }
}

/// The transport and audio device.
pub struct Player {
    shared: Arc<Shared>,
    fault: Arc<Fault>,
    /// The block count last seen while playing, and since when.
    #[cfg(not(target_arch = "wasm32"))]
    watch: Mutex<Option<(u64, std::time::Instant)>>,
    tx: Sender<Cmd>,
    side: Mutex<Side>,
    #[cfg(not(target_arch = "wasm32"))]
    _stream: Option<cpal::Stream>,
    #[cfg(target_arch = "wasm32")]
    _stream: Option<cpal::Stream>,
    pub device_name: String,
    pub device_rate: u32,
    /// Output channels the device was opened with (0 for the silent clock).
    pub device_channels: u16,
    /// The device buffer in frames when one was chosen (`None`: the device default).
    pub device_buffer: Option<u32>,
    /// True when no audio device could be opened (silent clock).
    pub silent: bool,
}

impl Player {
    /// Open the default output device. Never fails: without a device it runs a silent clock.
    pub fn new(session: Arc<Session>) -> Player {
        Player::with_config(session, &AudioConfig::default())
    }

    /// Open the output device `cfg` chooses (system, device, rate, buffer size), falling back to the
    /// defaults for whatever is unavailable. Never fails: without a device it runs a silent clock.
    pub fn with_config(session: Arc<Session>, cfg: &AudioConfig) -> Player {
        Player::started(Player::open(Arc::clone(&session), cfg), session)
    }

    fn started(p: Player, session: Arc<Session>) -> Player {
        p.side().return_old_sessions(&p.tx);
        // Hand the initial session's third-party plugins over.
        p.update_session(session);
        p
    }

    /// What stopped the audio output, once: a bug in the engine, or the device went away or
    /// stopped asking for audio while playing.
    pub fn trouble(&self) -> Option<OutputTrouble> {
        #[cfg(not(target_arch = "wasm32"))]
        let stalled = {
            let playing = self.shared.playing.load(Ordering::Relaxed) && !self.silent;
            stalled(&mut self.watch.lock().unwrap_or_else(PoisonError::into_inner), self.fault.beats.load(Ordering::Relaxed), playing)
        };
        #[cfg(target_arch = "wasm32")]
        let stalled = false;
        self.fault.trouble(stalled)
    }

    fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn new_shared() -> Arc<Shared> {
        Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        })
    }

    fn open(session: Arc<Session>, cfg: &AudioConfig) -> Player {
        let shared = Player::new_shared();
        let want = session.sample_rate.hz();
        let want_ch = u16::try_from(main_channels(&session)).unwrap_or(2);
        let side_rate = session.sample_rate.as_f64() as f32;
        let new_side = |garbage: Receiver<Vec<Box<dyn Plugin>>>| Mutex::new(Side::new(side_rate, garbage));
        match open_device(want, want_ch, cfg) {
            Ok((device, config, format, name)) => {
                // The chosen buffer size first; if the device refuses it, its default.
                let mut attempts = vec![config.clone()];
                if config.buffer_size != cpal::BufferSize::Default {
                    attempts.push(cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..config });
                }
                for config in attempts {
                    let rate = config.sample_rate.0;
                    let channels = usize::from(config.channels);
                    let (tx, rx) = channel();
                    let (gtx, grx) = channel();
                    let state = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&session), f64::from(rate), gtx);
                    let fault = Arc::new(Fault::default());
                    use cpal::traits::StreamTrait;
                    let built = match format {
                        cpal::SampleFormat::I32 => build_output::<i32>(&device, &config, state, channels, &fault, &shared),
                        cpal::SampleFormat::I16 => build_output::<i16>(&device, &config, state, channels, &fault, &shared),
                        _ => build_output::<f32>(&device, &config, state, channels, &fault, &shared),
                    };
                    match built {
                        Ok(stream) => {
                            if let Err(e) = stream.play() {
                                log::warn!("audio stream failed to start: {e}");
                            }
                            let buffer = match config.buffer_size {
                                cpal::BufferSize::Fixed(n) => Some(n),
                                cpal::BufferSize::Default => None,
                            };
                            log::info!(
                                "audio device: {name} @ {rate} Hz, {channels} ch, {format}, buffer {}",
                                buffer.map_or("default".to_string(), |n| n.to_string())
                            );
                            return Player {
                                shared,
                                fault,
                                #[cfg(not(target_arch = "wasm32"))]
                                watch: Mutex::new(None),
                                tx,
                                side: new_side(grx),
                                _stream: Some(stream),
                                device_name: name,
                                device_rate: rate,
                                device_channels: config.channels,
                                device_buffer: buffer,
                                silent: false,
                            };
                        }
                        Err(e) => log::warn!("cannot open audio stream ({:?}): {e}", config.buffer_size),
                    }
                }
                log::warn!("no usable audio stream; using a silent clock");
                let (tx, rx) = channel();
                let (gtx, grx) = channel();
                Player::silent_clock(shared, tx, rx, want, new_side(grx), gtx)
            }
            Err(e) => {
                log::warn!("no audio output: {e}; using a silent clock");
                let (tx, rx) = channel();
                let (gtx, grx) = channel();
                Player::silent_clock_with(shared, tx, rx, Arc::clone(&session), new_side(grx), gtx)
            }
        }
    }

    fn silent_clock(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        rx: Receiver<Cmd>,
        rate: u32,
        side: Mutex<Side>,
        gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> Player {
        let s = Arc::new(Session::new("Untitled", soundcraft_time::SampleRate::new(rate).unwrap_or_default()));
        Player::silent_clock_with(shared, tx, rx, s, side, gtx)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn silent_clock_with(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        rx: Receiver<Cmd>,
        session: Arc<Session>,
        side: Mutex<Side>,
        gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> Player {
        let rate = session.sample_rate.hz();
        let mut state = AudioState::new(rx, Arc::clone(&shared), session, f64::from(rate), gtx);
        let fault = Arc::new(Fault::default());
        let failed = Arc::clone(&fault);
        let spawn = std::thread::Builder::new().name("soundcraft-clock".into()).spawn(move || {
            let mut buf = vec![0.0f32; 4096];
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let now = std::time::Instant::now();
                let frames = ((now - last).as_secs_f64() * state.device_rate) as usize;
                last = now;
                let n = (frames * 2).min(buf.len());
                if let Some(b) = buf.get_mut(..n) {
                    let shared = Arc::clone(&state.shared);
                    guarded_block(&failed, &shared, b, |b| state.fill(b, 2));
                }
                if Arc::strong_count(&state.shared) <= 1 {
                    break;
                }
            }
        });
        if let Err(e) = spawn {
            log::warn!("clock thread failed: {e}");
        }
        Player {
            shared,
            fault,
            watch: Mutex::new(None),
            tx,
            side,
            _stream: None,
            device_name: "No audio device (silent)".into(),
            device_rate: rate,
            device_channels: 0,
            device_buffer: None,
            silent: true,
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn silent_clock_with(
        shared: Arc<Shared>,
        tx: Sender<Cmd>,
        _rx: Receiver<Cmd>,
        session: Arc<Session>,
        side: Mutex<Side>,
        _gtx: Sender<Vec<Box<dyn Plugin>>>,
    ) -> Player {
        Player {
            shared,
            fault: Arc::default(),
            tx,
            side,
            _stream: None,
            device_name: "No audio device".into(),
            device_rate: session.sample_rate.hz(),
            device_channels: 0,
            device_buffer: None,
            silent: true,
        }
    }

    /// Sends a new session snapshot to the audio thread, with freshly created (state-restored,
    /// prepared) instances of any third-party plugin it newly needs. Call on the UI thread.
    pub fn update_session(&self, s: Arc<Session>) {
        let instances = self.side().prepare(&s);
        let _ = self.tx.send(Cmd::Session(s, instances));
    }

    /// Reads the state of every live third-party plugin, for saving `s` (the current session):
    /// store each on its insert with [`Insert::store_live_state`], and its
    /// [`params`](CapturedState::params) over the insert's overrides. Slots `s` changed since the
    /// last [`update_session`](Self::update_session) are left out (their new insert wins).
    pub fn capture_states(&self, s: &Session) -> PluginStates {
        self.side().capture_states(s)
    }

    /// Per-UI-frame housekeeping: drops retired instances, lets state readers hand their
    /// instances what the UI thread owes them, and services plugin editors. Returns the
    /// parameter edits made in open editors since the last call.
    pub fn idle(&self) -> Vec<EditorEdit> {
        let mut side = self.side();
        side.collect_garbage();
        for r in side.states.values_mut() {
            r.idle();
        }
        let mut out = side.take_orphaned();
        for (&(t, slot), e) in side.editors.iter_mut() {
            for (id, v) in e.idle() {
                out.push((t, slot, id, v));
            }
        }
        out
    }

    /// Where the plugin in this slot (an insert, or [`soundcraft_mix::INSTRUMENT_SLOT`]) stands with
    /// its own editor.
    pub fn editor_state(&self, track: TrackId, slot: usize) -> EditorState {
        self.side().editor_state((track, slot))
    }

    /// Opens (or raises) the plugin's own editor window. Main thread only.
    pub fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), EditorError> {
        self.side().open_editor((track, slot))
    }

    pub fn close_editor(&self, track: TrackId, slot: usize) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.close();
        }
    }

    /// Closes every plugin editor window (main thread). Call before replacing the player: its
    /// instances may be dropped on the audio thread, where editor windows cannot be closed.
    pub fn close_editors(&self) {
        for e in self.side().editors.values_mut() {
            e.close();
        }
    }

    pub fn any_editor_open(&self) -> bool {
        self.side().editors.values().any(|e| e.is_open())
    }

    /// Shows a parameter change made elsewhere in the plugin's open editor.
    pub fn editor_set_param(&self, track: TrackId, slot: usize, id: &str, value: f32) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.set_param(id, value);
        }
    }

    /// Start playback at `from`, stopping at `end`, or looping `looped`.
    pub fn play(&self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        self.shared.position.store(from, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Play { from, end, looped });
    }

    /// Feed live input (from a `record::Recorder`) into the mix for input monitoring.
    pub fn set_input(&self, ring: Arc<record::InputRing>) {
        let _ = self.tx.send(Cmd::Input(ring));
    }

    pub fn set_recording(&self, on: bool) {
        let _ = self.tx.send(Cmd::Recording(on));
    }

    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn locate(&self, at: Samples) {
        self.shared.position.store(at, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Locate(at));
    }

    pub fn set_speed(&self, speed: f32) {
        let v = if speed.is_finite() { (speed.clamp(0.1, 4.0) * 1000.0) as u32 } else { 1000 };
        self.shared.speed.store(v, Ordering::Relaxed);
    }

    /// Playback rate last requested, 1.0 at full speed.
    pub fn speed(&self) -> f32 {
        self.shared.speed.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn position(&self) -> Samples {
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn meters(&self) -> MeterSnapshot {
        self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Copy meters into reusable UI storage. Once track/channel counts are stable this performs
    /// no allocation, unlike cloning a new snapshot every video frame.
    pub fn copy_meters(&self, out: &mut MeterSnapshot) {
        let source = self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner);
        copy_meter_snapshot(&source, out);
    }
}

/// Test seams (the `test-util` feature): a player that never touches the audio device, and plugin
/// editors that stand in for a hosted plugin's own.
#[cfg(any(test, feature = "test-util"))]
impl Player {
    /// A player on the silent clock, as when no audio device is available, without looking for
    /// one: tests must never open the real device.
    pub fn without_device(session: Arc<Session>) -> Player {
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let side = Mutex::new(Side::new(session.sample_rate.as_f64() as f32, grx));
        let p = Player::silent_clock_with(Player::new_shared(), tx, rx, Arc::clone(&session), side, gtx);
        Player::started(p, session)
    }

    /// Makes `editor` the editor of the hosted plugin `s` holds in `slot` (an insert, or
    /// [`soundcraft_mix::INSTRUMENT_SLOT`]), as if the player had made its instance; the next
    /// [`update_session`](Self::update_session) with that plugin keeps it. Returns false, changing
    /// nothing, when `s` holds no hosted plugin there.
    pub fn adopt_editor(&self, s: &Session, track: TrackId, slot: usize, editor: Box<dyn PluginEditor>) -> bool {
        self.adopt_slot(s, (track, slot)).map(|mut side| side.editors.insert((track, slot), editor)).is_some()
    }

    /// Makes `reader` the state reader of the hosted plugin `s` holds in `slot`, as if the player
    /// had made its instance (like [`adopt_editor`](Self::adopt_editor), which it replaces):
    /// [`capture_states`](Self::capture_states) reads it. Returns false, changing nothing, when
    /// `s` holds no hosted plugin there.
    pub fn adopt_state_reader(&self, s: &Session, track: TrackId, slot: usize, reader: Box<dyn PluginStateReader>) -> bool {
        self.adopt_slot(s, (track, slot)).map(|mut side| side.states.insert((track, slot), reader)).is_some()
    }

    /// Records a made instance of the hosted plugin `s` holds at `key`, forgetting the slot's
    /// previous one; `None` when `s` holds no hosted plugin there.
    fn adopt_slot(&self, s: &Session, key: (TrackId, usize)) -> Option<std::sync::MutexGuard<'_, Side>> {
        let spec = soundcraft_mix::instance_specs(s).into_iter().find(|x| (x.track, x.slot) == key)?;
        let epoch = insert_at(s, key).map_or(0, Insert::state_epoch);
        let mut side = self.side();
        side.forget(key);
        side.shadow.insert(key, Live { id: spec.id, ch: spec.ch, created: true, epoch });
        Some(side)
    }
}

/// A stand-in for a hosted plugin's own editor ([`Player::adopt_editor`], the `test-util`
/// feature): its window is a flag, and it records the calls it gets. Clones share these.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug, Default)]
pub struct StandInEditor {
    calls: Arc<Mutex<Vec<&'static str>>>,
    shown: Arc<AtomicBool>,
    /// Its window took the keyboard focus since [`StandInEditor::took_focus`] last looked.
    focus: Arc<AtomicBool>,
    /// What `open` answers instead of opening.
    refusal: Option<String>,
}

#[cfg(any(test, feature = "test-util"))]
impl StandInEditor {
    /// An editor whose window never opens: `open` answers `why`.
    pub fn refusing(why: &str) -> StandInEditor {
        StandInEditor { refusal: Some(why.into()), ..StandInEditor::default() }
    }

    /// The calls so far, in order: "open" and "close".
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn shown(&self) -> bool {
        self.shown.load(Ordering::Relaxed)
    }

    /// Its window took the keyboard focus since the last call. A real editor's window becomes key
    /// each time it opens or is raised, so SoundCraft's window loses focus then: a UI test hands
    /// this on as the window's `WindowFocused(false)`.
    pub fn took_focus(&self) -> bool {
        self.focus.swap(false, Ordering::Relaxed)
    }

    fn record(&self, call: &'static str) {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).push(call);
    }
}

#[cfg(any(test, feature = "test-util"))]
impl PluginEditor for StandInEditor {
    fn open(&mut self) -> Result<(), String> {
        self.record("open");
        match &self.refusal {
            Some(why) => Err(why.clone()),
            None => {
                self.shown.store(true, Ordering::Relaxed);
                self.focus.store(true, Ordering::Relaxed);
                Ok(())
            }
        }
    }
    fn close(&mut self) {
        self.record("close");
        self.shown.store(false, Ordering::Relaxed);
    }
    fn is_open(&self) -> bool {
        self.shown()
    }
    fn idle(&mut self) -> Vec<(String, f32)> {
        Vec::new()
    }
}

fn copy_meter_snapshot(source: &MeterSnapshot, out: &mut MeterSnapshot) {
    out.tracks.retain(|id, _| source.tracks.contains_key(id));
    for (id, meter) in &source.tracks {
        out.tracks.entry(*id).or_default().copy_from(meter);
    }
    out.main.copy_from(&source.main);
}

impl Side {
    /// Reads the live instances `s` still describes. A slot whose insert changed since the last
    /// `prepare` (another plugin, or a new state set by a command in the same frame as the save) is
    /// skipped: its instance is about to be made again, and its state would overwrite the new one.
    fn capture_states(&mut self, s: &Session) -> PluginStates {
        let current = |k: &(TrackId, usize)| {
            self.shadow.get(k).zip(insert_at(s, *k)).is_some_and(|(l, ins)| l.id == ins.plugin && l.epoch == ins.state_epoch())
        };
        self.states
            .iter_mut()
            .filter(|(k, _)| !self.refused.contains(*k) && current(k))
            .filter_map(|(&(t, slot), r)| {
                let state = r.save_state()?;
                let params = s.track(t).zip(insert_at(s, (t, slot))).map_or_else(Vec::new, |(tr, ins)| moved_params(tr, slot, ins, r.as_mut()));
                Some(CapturedState { track: t, slot, state, params })
            })
            .collect()
    }

    fn editor_state(&self, key: (TrackId, usize)) -> EditorState {
        match (self.shadow.get(&key), self.editors.get(&key)) {
            (None, _) => EditorState::NotLoaded,
            (Some(l), _) if !l.created => EditorState::Failed,
            (Some(_), None) => EditorState::NoEditor,
            (Some(_), Some(e)) if e.is_open() => EditorState::Open,
            (Some(_), Some(_)) => EditorState::Closed,
        }
    }

    fn open_editor(&mut self, key: (TrackId, usize)) -> Result<(), EditorError> {
        let Some(e) = self.editors.get_mut(&key) else {
            return Err(EditorError::Unavailable(self.editor_state(key)));
        };
        e.open().map_err(EditorError::Failed)?;
        self.refused.remove(&key);
        Ok(())
    }

    /// Creates the third-party instances `s` needs that the audio thread does not have yet.
    fn prepare(&mut self, s: &Session) -> Vec<PreparedInstance> {
        self.collect_garbage();
        let sr = s.sample_rate.as_f64() as f32;
        if (sr - self.rate).abs() > 0.5 {
            // The audio thread rebuilds its engine: everything is created anew.
            let keys: Vec<(TrackId, usize)> = self.shadow.keys().copied().collect();
            for k in keys {
                self.forget(k);
            }
            self.rate = sr;
        }
        let specs = soundcraft_mix::instance_specs(s);
        let stale: Vec<(TrackId, usize)> = self.shadow.keys().filter(|k| !specs.iter().any(|x| (x.track, x.slot) == **k)).copied().collect();
        for k in stale {
            self.forget(k);
        }
        let mut out = Vec::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            // A new state (set by a command or a preset, or a loaded insert) makes the instance
            // again; otherwise it would keep its old settings and the next capture would write them
            // over the new state. Undo, Redo and captured states keep the epoch, and the instance.
            let epoch = insert_at(s, key).map_or(0, Insert::state_epoch);
            if self.shadow.get(&key).is_some_and(|l| l.id == spec.id && l.ch == spec.ch && l.epoch == epoch) {
                continue;
            }
            self.forget(key);
            // Played live: these stay in real time (offline renders make their own instances).
            let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), sr, BLOCK, spec.ch, soundcraft_dsp::RenderMode::Realtime);
            log::debug!("plugin instance for {:?} slot {}: {} ({} ch, created: {})", spec.track, spec.slot, spec.id, spec.ch, made.is_some());
            self.shadow.insert(key, Live { id: spec.id.clone(), ch: spec.ch, created: made.is_some(), epoch });
            if let Some(soundcraft_mix::NewInstance { mut plugin, refused_state }) = made {
                if let Some(e) = plugin.editor() {
                    self.editors.insert(key, e);
                }
                if let Some(r) = plugin.state_reader() {
                    self.states.insert(key, r);
                }
                if refused_state {
                    self.refused.insert(key);
                }
                out.push(PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin });
            }
        }
        out
    }
}

/// The overrides of `ins`, in `slot` of `t`, that its live instance has moved away from by itself:
/// read now, the value differs from the override and from the value the host last knew the
/// instance had. That leaves out an override the instance has not received yet (the mixer sends
/// overrides only while it renders: it still has the value the host knew), and a parameter an
/// automation lane drives ([`soundcraft_mix::driving_lane`]: the lane decides its value during
/// playback, not the override).
fn moved_params(t: &Track, slot: usize, ins: &Insert, r: &mut dyn PluginStateReader) -> Vec<(String, f32)> {
    let ids: Vec<&str> = ins.params.keys().map(String::as_str).filter(|id| soundcraft_mix::driving_lane(t, slot, id).is_none()).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    ids.iter()
        .zip(r.param_values(&ids))
        .filter_map(|(id, read)| {
            let read = read?;
            let stored = *ins.params.get(*id)?;
            (read.live.is_finite() && !same_value(read.live, stored) && !same_value(read.live, read.host)).then(|| (id.to_string(), read.live))
        })
        .collect()
}

/// Equal but for float noise (a value converted to a plugin's own units and back).
fn same_value(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-5 * a.abs().max(b.abs()).max(1.0)
}

/// The insert in `slot` (or the instrument, [`soundcraft_mix::INSTRUMENT_SLOT`]) of a track.
fn insert_at(s: &Session, (t, slot): (TrackId, usize)) -> Option<&Insert> {
    s.track(t)?.insert_at((slot != soundcraft_mix::INSTRUMENT_SLOT).then_some(slot))
}

/// ITU stereo fold-down gains for each main channel of a session.
fn fold_gains(s: &Session) -> [(f32, f32); MAX_CHANNELS] {
    let mut g = [(0.0, 0.0); MAX_CHANNELS];
    let f = s.main_format();
    let sp = f.speakers();
    if sp.len() == f.channels() {
        for (o, x) in g.iter_mut().zip(sp.iter()) {
            *o = itu_stereo_fold(*x);
        }
    } else {
        // Ambisonics: W only, at -3 dB per side.
        g[0] = (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2);
    }
    g
}

/// The output stream: the mix renders f32 into a scratch buffer allocated here (so the callback
/// never allocates), converted to the device's sample type.
fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: AudioState,
    channels: usize,
    fault: &Arc<Fault>,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let frame = channels.max(1);
    let mut scratch = vec![0.0f32; 8192 * frame];
    let chunk = scratch.len();
    let (failed, guard, gone) = (Arc::clone(fault), Arc::clone(shared), Arc::clone(fault));
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            mark_audio_thread();
            for out in data.chunks_mut(chunk) {
                let Some(s) = scratch.get_mut(..out.len()) else { return };
                guarded_block(&failed, &guard, s, |d| state.fill(d, channels));
                for (o, x) in out.iter_mut().zip(s.iter()) {
                    *o = T::from_sample(*x);
                }
            }
        },
        // cpal may call this on the audio thread (on ALSA in a loop while the device stays
        // broken): marked, so the logger hands the record off instead of writing it.
        move |e: cpal::StreamError| {
            mark_audio_thread();
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                gone.gone.store(true, Ordering::Relaxed);
            }
            log::warn!("audio stream error: {e}");
        },
        None,
    )
}

/// Open the output device `cfg` chooses (else the default) at the chosen rate (else the session
/// rate) if possible, in the best sample format it offers (f32, else 32- or 16-bit integers),
/// with as many channels as it offers up to `want_ch` (at least stereo when available), and the
/// chosen buffer size when the device allows it.
fn open_device(want_rate: u32, want_ch: u16, cfg: &AudioConfig) -> Result<(cpal::Device, cpal::StreamConfig, cpal::SampleFormat, String), String> {
    use cpal::traits::DeviceTrait;
    let host = devices::host(cfg);
    let device = devices::device(&host, cfg.output.as_deref(), true).ok_or_else(|| "no output device".to_string())?;
    #[allow(deprecated)]
    let name = device.name().unwrap_or_else(|_| "Audio device".into());
    // Prefer a config at the chosen (else session) rate; else the device default (we resample).
    let want_ch = want_ch.max(2);
    let rates: Vec<u32> = cfg.sample_rate.into_iter().chain(Some(want_rate)).collect();
    let mut chosen: Option<(cpal::StreamConfig, cpal::SupportedBufferSize, cpal::SampleFormat)> = None;
    // Better: a preferred format first, then more channels.
    let better = |c: &cpal::SupportedStreamConfigRange, x: &(cpal::StreamConfig, cpal::SupportedBufferSize, cpal::SampleFormat)| {
        let (rc, rx) = (devices::format_rank(c.sample_format()).unwrap_or(usize::MAX), devices::format_rank(x.2).unwrap_or(usize::MAX));
        rc < rx || (rc == rx && c.channels() > x.0.channels)
    };
    if let Ok(configs) = device.supported_output_configs() {
        let configs: Vec<cpal::SupportedStreamConfigRange> = configs.collect();
        for rate in rates {
            for c in &configs {
                if devices::format_rank(c.sample_format()).is_some()
                    && c.min_sample_rate().0 <= rate
                    && c.max_sample_rate().0 >= rate
                    && c.channels() >= 2
                    && c.channels() <= want_ch
                    && chosen.as_ref().is_none_or(|x| better(c, x))
                {
                    chosen = Some((c.with_sample_rate(cpal::SampleRate(rate)).config(), *c.buffer_size(), c.sample_format()));
                }
            }
            if chosen.is_some() {
                break;
            }
        }
    }
    let (config, format) = match chosen {
        Some((mut c, buffers, format)) => {
            c.buffer_size = devices::buffer_size(cfg.buffer_size, &buffers);
            (c, format)
        }
        None => {
            let d = device.default_output_config().map_err(|e| e.to_string())?;
            if devices::format_rank(d.sample_format()).is_none() {
                return Err(format!("unsupported device sample format {:?}", d.sample_format()));
            }
            let buffers = *d.buffer_size();
            let format = d.sample_format();
            let mut c = d.config();
            c.buffer_size = devices::buffer_size(cfg.buffer_size, &buffers);
            (c, format)
        }
    };
    Ok((device, config, format, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::AutoParam;

    #[test]
    fn reusable_meter_snapshot_updates_and_drops_stale_tracks() {
        let keep = TrackId(1);
        let stale = TrackId(2);
        let mut source = MeterSnapshot::default();
        source.tracks.insert(keep, StripMeter { peak: [0.5, 0.25], peaks: vec![0.5, 0.25], gain_reduction_db: 3.0, ..StripMeter::default() });
        source.main.peaks = vec![0.75, 0.5];
        let mut out = MeterSnapshot::default();
        out.tracks.insert(stale, StripMeter::default());
        copy_meter_snapshot(&source, &mut out);
        assert_eq!(out.tracks.len(), 1);
        assert_eq!(out.tracks[&keep], source.tracks[&keep]);
        assert_eq!(out.main, source.main);
    }

    #[test]
    fn a_block_that_panics_goes_silent_and_is_reported_once() {
        let shared = Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(true),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        };
        let fault = Fault::default();
        let mut out = [0.5f32; 8];
        guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25));
        assert_eq!((out, fault.trouble(false)), ([0.25; 8], None));
        guarded_block(&fault, &shared, &mut out, |_| std::panic::panic_any("a bug in the mix"));
        guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25));
        assert_eq!(out, [0.0; 8]);
        assert!(!shared.playing.load(Ordering::Relaxed));
        assert_eq!((fault.trouble(false), fault.trouble(false)), (Some(OutputTrouble::Bug), None));
        assert_eq!(fault.beats.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_lost_or_stalled_device_is_reported_once() {
        let ago = |seen: &mut Option<(u64, std::time::Instant)>| {
            if let Some(s) = seen {
                s.1 = std::time::Instant::now().checked_sub(STALL).unwrap();
            }
        };
        let mut seen = None;
        assert!(!stalled(&mut seen, 5, true));
        ago(&mut seen);
        assert!(stalled(&mut seen, 5, true));
        assert!(!stalled(&mut seen, 6, true), "it asked again");
        ago(&mut seen);
        assert!(!stalled(&mut seen, 6, false), "stopped, so nothing to ask for");
        assert_eq!(Fault::default().trouble(true), Some(OutputTrouble::Stalled));
        let fault = Fault::default();
        fault.gone.store(true, Ordering::Relaxed);
        assert_eq!((fault.trouble(false), fault.trouble(true)), (Some(OutputTrouble::Gone), None));
    }

    #[test]
    fn audio_state_plays_and_stops_at_end() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(shared.position.load(Ordering::Relaxed) >= 1000);
        assert!(!shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn short_device_callbacks_render_short_mix_blocks() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 2 * 64];
        st.fill(&mut out, 2);
        assert_eq!(shared.position.load(Ordering::Relaxed), 64);
    }

    /// The device calls back on its realtime thread. When the mixer handed a session's strips to
    /// its worker pool, that thread slept until a pool worker, an ordinary thread, got a core; on
    /// a busy machine that came after the device's deadline, and live playback had gaps of silence
    /// that a bounce of the same session did not. Here every pool worker is held up: the callback
    /// still fills its buffer.
    #[test]
    fn the_callback_never_waits_for_the_worker_pool() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let mut s = soundcraft_model::Session::default();
        s.edit.click = false;
        s.pool.insert(
            SourceId(5),
            Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.05; 4800]; 2] })),
        );
        // Enough tracks for the mixer to render them in parallel.
        for _ in 0..6 {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
            let id = s.new_clip_id();
            s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 4800));
        }
        let shared = Player::new_shared();
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();

        let gate = Arc::new(std::sync::RwLock::new(()));
        let held = gate.write().unwrap();
        let busy = Arc::new(AtomicUsize::new(0));
        for _ in 0..rayon::current_num_threads() {
            let (gate, busy) = (Arc::clone(&gate), Arc::clone(&busy));
            rayon::spawn(move || {
                busy.fetch_add(1, Ordering::SeqCst);
                drop(gate.read());
            });
        }
        while busy.load(Ordering::SeqCst) < rayon::current_num_threads() {
            std::thread::yield_now();
        }
        let (done, filled) = channel();
        let callback = std::thread::spawn(move || {
            mark_audio_thread();
            let mut out = vec![0.0f32; 2 * BLOCK];
            st.fill(&mut out, 2);
            done.send(out).unwrap();
        });
        let out = filled.recv_timeout(Duration::from_secs(2));
        drop(held);
        callback.join().unwrap();
        let out = out.expect("the callback waited for the worker pool");
        assert!(out.iter().all(|x| (x - 0.3).abs() < 1e-4), "six clips at 0.05 sum to 0.3: {:?}", &out[..4]);
    }

    #[test]
    fn surround_main_maps_to_device_channels_or_folds_down() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
        let mut s = soundcraft_model::Session::default();
        s.outputs[0].format = ChannelFormat::Surround51;
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Surround51, None);
        let chans: Vec<Vec<f32>> = (0..6).map(|c| vec![0.1 * (c + 1) as f32; 48_000]).collect();
        s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: chans })));
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 48_000));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&s), 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        // An 8-channel device: channels in order, the rest silent.
        let mut out = vec![0.0f32; 8 * 256];
        st.fill(&mut out, 8);
        let f = &out[8 * 100..8 * 101];
        for c in 0..6 {
            assert!((f[c] - 0.1 * (c + 1) as f32).abs() < 1e-5, "{f:?}");
        }
        assert!(f[6] == 0.0 && f[7] == 0.0);
        // A stereo device: ITU fold-down.
        let mut out = vec![0.0f32; 2 * 256];
        st.fill(&mut out, 2);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let l = (0.1 + h * 0.3 + h * 0.5f32).min(1.0);
        assert!((out[200] - l).abs() < 1e-4, "{} vs {l}", out[200]);
        assert_eq!(shared.meters.lock().unwrap().main.peaks.len(), 6);
    }

    #[test]
    fn instances_arrive_with_the_session_and_leave_through_the_return_channel() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.fake"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let plugin = soundcraft_dsp::create("gain").unwrap();
        tx.send(Cmd::Session(Arc::clone(&s), vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin }])).unwrap();
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        assert!(grx.try_recv().is_err(), "nothing retired yet");
        // A session without the insert retires the instance to the return channel.
        tx.send(Cmd::Session(Arc::new(soundcraft_model::Session::default()), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.try_recv().unwrap().len(), 1);
        // A sample-rate change rebuilds the engine without dropping plugins on this thread.
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin: soundcraft_dsp::create("gain").unwrap() }],
        ))
        .unwrap();
        st.fill(&mut out, 2);
        let mut s44 = (*s).clone();
        s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        tx.send(Cmd::Session(Arc::new(s44), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.try_recv().unwrap().len(), 1);
    }

    /// A session with one mono track whose insert A holds a hosted plugin that is not installed:
    /// `prepare` remembers the slot without making an instance, and a state reader stands in for
    /// the live instance (making the instance again, `forget`, drops it).
    fn hosted_slot() -> (Side, Session, (TrackId, usize)) {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut side = Side::new(48_000.0, channel().1);
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
        side.prepare(&s);
        side.states.insert((t, 0), live(b"live"));
        (side, s, (t, 0))
    }

    fn live(state: &'static [u8]) -> Box<dyn PluginStateReader> {
        Box::new(move || Some(state.to_vec()))
    }

    fn insert_mut(s: &mut Session, (t, slot): (TrackId, usize)) -> &mut Insert {
        s.track_mut(t).unwrap().mixer.inserts[slot].as_mut().unwrap()
    }

    #[test]
    fn plugin_states_are_read_without_the_audio_thread() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut side = Side::new(48_000.0, channel().1);
        let mut s = Session::default();
        let a = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let b = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let hosted = || Some(Insert::new("clap:does.not.exist"));
        s.track_mut(a).unwrap().instrument = hosted();
        s.track_mut(a).unwrap().mixer.inserts[3] = hosted();
        s.track_mut(a).unwrap().mixer.inserts[4] = hosted();
        s.track_mut(b).unwrap().mixer.inserts[0] = hosted();
        side.prepare(&s);
        side.states.insert((b, 0), live(b"b0"));
        side.states.insert((a, soundcraft_mix::INSTRUMENT_SLOT), live(b"synth"));
        side.states.insert((a, 3), live(b"a3"));
        side.states.insert((a, 4), Box::new(|| None));
        let captured = |track, slot, state: &[u8]| CapturedState { track, slot, state: state.to_vec(), params: Vec::new() };
        let want = vec![captured(a, 3, b"a3"), captured(a, soundcraft_mix::INSTRUMENT_SLOT, b"synth"), captured(b, 0, b"b0")];
        assert_eq!(side.capture_states(&s), want);
        side.forget((a, 3));
        assert_eq!(side.capture_states(&s), want[1..]);
    }

    struct Window;

    impl PluginEditor for Window {
        fn open(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn close(&mut self) {}
        fn is_open(&self) -> bool {
            false
        }
        fn idle(&mut self) -> Vec<(String, f32)> {
            Vec::new()
        }
    }

    /// An editor holding the edits made in its last frame.
    struct Edited(Vec<(String, f32)>);

    impl PluginEditor for Edited {
        fn open(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn close(&mut self) {}
        fn is_open(&self) -> bool {
            true
        }
        fn idle(&mut self) -> Vec<(String, f32)> {
            std::mem::take(&mut self.0)
        }
    }

    /// Edits made in a plugin's editor in the frame its instance is made again for the same plugin
    /// and state (here a new sample rate) still reach the session; edits for a slot that now holds
    /// another plugin are dropped, never written onto it.
    #[test]
    fn a_forgotten_editors_last_edits_reach_the_session_for_the_same_plugin_only() {
        let (mut side, mut s, k) = hosted_slot();
        side.editors.insert(k, Box::new(Edited(vec![("7".into(), 0.5)])));
        s.sample_rate = soundcraft_time::SampleRate::new(96_000).unwrap();
        side.prepare(&s);
        assert_eq!(side.take_orphaned(), vec![(k.0, k.1, "7".to_string(), 0.5)]);
        assert!(side.take_orphaned().is_empty(), "reported once");

        side.editors.insert(k, Box::new(Edited(vec![("7".into(), 0.25)])));
        *insert_mut(&mut s, k) = Insert::new("clap:another.plugin");
        side.prepare(&s);
        assert!(side.take_orphaned().is_empty(), "another plugin got the old one's edits");
    }

    #[test]
    fn an_editor_that_cannot_open_says_where_it_stands() {
        // Insert A holds a plugin that is not installed; insert B was never asked for.
        let (mut side, _, k) = hosted_slot();
        let other = (k.0, 1);
        assert_eq!(side.editor_state(other), EditorState::NotLoaded);
        assert_eq!(side.editor_state(k), EditorState::Failed);
        assert_eq!(side.open_editor(other), Err(EditorError::Unavailable(EditorState::NotLoaded)));
        assert_eq!(side.open_editor(k), Err(EditorError::Unavailable(EditorState::Failed)));
        let made = |side: &mut Side, yes: bool| side.shadow.get_mut(&k).map(|l| l.created = yes);
        made(&mut side, true);
        assert_eq!(side.editor_state(k), EditorState::NoEditor);
        assert_eq!(side.open_editor(k), Err(EditorError::Unavailable(EditorState::NoEditor)));
        side.editors.insert(k, Box::new(StandInEditor::default()));
        assert_eq!(side.editor_state(k), EditorState::Closed);
        side.open_editor(k).unwrap();
        assert_eq!(side.editor_state(k), EditorState::Open);
        side.editors.insert(k, Box::new(StandInEditor::refusing("no window today")));
        assert_eq!(side.open_editor(k), Err(EditorError::Failed("no window today".into())), "the host's own words");
    }

    #[test]
    fn a_player_without_a_device_adopts_a_stand_in_editor_and_keeps_it() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
        s.track_mut(t).unwrap().mixer.inserts[2] = Some(Insert::new("clap:does.not.exist"));
        let s = Arc::new(s);
        let p = Player::without_device(Arc::clone(&s));
        assert!(p.silent && p.device_channels == 0, "{}", p.device_name);
        assert_eq!(p.editor_state(t, 2), EditorState::Failed);
        let editor = StandInEditor::default();
        assert!(!p.adopt_editor(&s, t, 0, Box::new(editor.clone())), "a built-in has no instance here");
        assert!(p.adopt_editor(&s, t, 2, Box::new(editor.clone())));
        assert_eq!(p.editor_state(t, 2), EditorState::Closed);
        p.open_editor(t, 2).unwrap();
        p.update_session(Arc::clone(&s));
        assert_eq!(p.editor_state(t, 2), EditorState::Open, "the same plugin keeps its instance and editor");
        p.close_editors();
        assert_eq!((editor.shown(), editor.calls()), (false, vec!["open", "close"]));
        let refusing = StandInEditor::refusing("no window today");
        assert!(p.adopt_editor(&s, t, 2, Box::new(refusing.clone())));
        assert_eq!(p.open_editor(t, 2), Err(EditorError::Failed("no window today".into())));
        assert_eq!((refusing.shown(), p.editor_state(t, 2)), (false, EditorState::Closed));
    }

    /// A VST3 instance's edit controller learns the values the host set only from the UI thread,
    /// so each UI frame lets every live instance's state reader do that work.
    #[test]
    fn idle_lets_every_state_reader_do_its_main_thread_work() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        use std::sync::atomic::AtomicUsize;
        struct Counting(Arc<AtomicUsize>);
        impl PluginStateReader for Counting {
            fn save_state(&mut self) -> Option<Vec<u8>> {
                None
            }
            fn idle(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:does.not.exist"));
        s.track_mut(t).unwrap().mixer.inserts[2] = Some(Insert::new("vst3:does.not.exist"));
        let s = Arc::new(s);
        let p = Player::without_device(Arc::clone(&s));
        let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        assert!(p.adopt_state_reader(&s, t, 1, Box::new(Counting(Arc::clone(&a)))));
        assert!(p.adopt_state_reader(&s, t, 2, Box::new(Counting(Arc::clone(&b)))));
        p.idle();
        p.idle();
        assert_eq!((a.load(Ordering::Relaxed), b.load(Ordering::Relaxed)), (2, 2));
    }

    #[test]
    fn a_refused_state_is_kept_until_the_editor_opens() {
        let (mut side, s, k) = hosted_slot();
        side.states.insert(k, live(b"defaults"));
        side.editors.insert(k, Box::new(Window));
        side.refused.insert(k);
        assert!(side.capture_states(&s).is_empty());
        side.open_editor(k).unwrap();
        assert_eq!(side.capture_states(&s), vec![CapturedState { track: k.0, slot: 0, state: b"defaults".to_vec(), params: Vec::new() }]);
    }

    #[test]
    fn side_creates_each_missing_instance_once() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut side = Side::new(48_000.0, channel().1);
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("gain"));
        assert!(side.prepare(&s).is_empty(), "the plugin is not installed; built-ins stay with the engine");
        let epoch = insert_mut(&mut s, (t, 0)).state_epoch();
        assert_eq!(side.shadow.get(&(t, 0)), Some(&Live { id: "clap:does.not.exist".into(), ch: 1, created: false, epoch }));
        assert!(side.prepare(&s).is_empty());
        assert_eq!(side.shadow.len(), 1, "failures are remembered, not retried every update");
        s.track_mut(t).unwrap().mixer.inserts[0] = None;
        side.prepare(&s);
        assert!(side.shadow.is_empty());
    }

    /// A state set by a command (an agent's `mix.insert_state`, a track preset) must reach the
    /// live instance. Before, only a new plugin id or channel count made it again, so it kept its
    /// old settings, and the next capture before a save or bounce wrote those over the new state.
    #[test]
    fn a_new_state_makes_the_instance_again_and_a_captured_one_does_not() {
        let (mut side, mut s, k) = hosted_slot();
        // What the UI does before a save or bounce: read the live state and store it on the insert.
        let captured = side.capture_states(&s);
        insert_mut(&mut s, k).store_live_state(&captured[0].state);
        side.prepare(&s);
        assert!(side.states.contains_key(&k), "storing the live instance's own state made it again");
        insert_mut(&mut s, k).set_state_bytes(b"another state");
        side.prepare(&s);
        assert!(!side.states.contains_key(&k), "the new state never reached the live instance");
        side.states.insert(k, live(b"another state"));
        side.prepare(&s);
        assert!(side.states.contains_key(&k), "an unchanged state made it again");
        // The same bytes set again (a preset loaded again over tweaks, an agent restoring a state
        // the instance once held) are a new state too.
        insert_mut(&mut s, k).set_state_bytes(b"another state");
        side.prepare(&s);
        assert!(!side.states.contains_key(&k), "setting a state again did not reset the plugin");
    }

    /// Undo and Redo swap in whole earlier documents, whose inserts hold the states captured before
    /// them (or none). The live instance already went through those: Undo after a save, a bounce or
    /// an autosave must not make it again from an older state.
    #[test]
    fn undo_and_redo_past_a_capture_keep_the_live_instance() {
        let (mut side, mut s, k) = hosted_slot();
        side.states.insert(k, live(b"tweaked in its editor"));
        // A fader move: its undo step holds the insert without a state.
        let before_fader = s.clone();
        s.track_mut(k.0).unwrap().mixer.volume_db = -6.0;
        side.prepare(&s);
        // A save captures the live state and stores it on the insert.
        let captured = side.capture_states(&s);
        insert_mut(&mut s, k).store_live_state(&captured[0].state);
        side.prepare(&s);
        side.prepare(&before_fader);
        assert!(side.states.contains_key(&k), "Undo rebuilt the plugin from the state its undo step holds");
        side.prepare(&s);
        assert!(side.states.contains_key(&k), "Redo rebuilt the plugin");
    }

    /// Undo of a command that set a state (loading a track preset) brings the older state back, so
    /// the instance is made again from it; Redo brings the preset's back.
    #[test]
    fn undo_and_redo_of_a_new_state_make_the_instance_again() {
        let (mut side, mut s, k) = hosted_slot();
        let before = s.clone();
        insert_mut(&mut s, k).set_state_bytes(b"preset");
        side.prepare(&s);
        side.states.insert(k, live(b"preset"));
        side.prepare(&before);
        assert!(!side.states.contains_key(&k), "Undo kept the preset's instance");
        side.states.insert(k, live(b"live"));
        side.prepare(&s);
        assert!(!side.states.contains_key(&k), "Redo kept the older instance");
    }

    /// A captured state the UI could not store (`mix.insert_state` refuses states over 64 MiB)
    /// leaves the session as it was. The live instance still holds its own state and must keep it.
    #[test]
    fn a_capture_that_was_not_stored_keeps_the_live_instance() {
        let (mut side, mut s, k) = hosted_slot();
        side.states.insert(k, live(b"too large to store"));
        assert_eq!(side.capture_states(&s).len(), 1);
        // The next edit sends the session again.
        s.track_mut(k.0).unwrap().mixer.volume_db = -6.0;
        side.prepare(&s);
        assert!(side.states.contains_key(&k), "the plugin was reverted to the state the session still holds");
    }

    /// Commands run before the frame's `prepare`. A state set, or a plugin replaced, in the same
    /// frame as a save must not get the old instance's state stored over it.
    #[test]
    fn a_capture_skips_slots_changed_since_the_last_prepare() {
        let (mut side, mut s, k) = hosted_slot();
        insert_mut(&mut s, k).set_state_bytes(b"from an agent");
        assert!(side.capture_states(&s).is_empty(), "the old instance's state would replace the agent's");
        side.prepare(&s);
        side.states.insert(k, live(b"from an agent"));
        assert_eq!(side.capture_states(&s).len(), 1);
        s.track_mut(k.0).unwrap().mixer.inserts[0] = Some(Insert::new("clap:another.plugin"));
        assert!(side.capture_states(&s).is_empty(), "the old plugin's state would go to the new one");
        s.track_mut(k.0).unwrap().mixer.inserts[0] = None;
        assert!(side.capture_states(&s).is_empty(), "the insert was removed");
    }

    /// A reader whose parameters read as listed: `(id, live, host)`.
    struct Reads(Vec<(&'static str, f32, f32)>);

    impl PluginStateReader for Reads {
        fn save_state(&mut self) -> Option<Vec<u8>> {
            Some(b"live".to_vec())
        }
        fn param_values(&mut self, ids: &[&str]) -> Vec<Option<soundcraft_dsp::ParamReading>> {
            ids.iter().map(|id| self.0.iter().find(|r| r.0 == *id).map(|&(_, live, host)| soundcraft_dsp::ParamReading { live, host })).collect()
        }
    }

    /// A plugin that changes parameters by itself (a preset loaded in its own editor, which VST3
    /// announces only as "values changed") has the new values in its state, but the insert's
    /// overrides of those parameters kept the old ones, and a loaded session applies them on top
    /// of the state: the preset came back half reverted. A capture returns the overrides the
    /// plugin moved away from, and only those: not one a lane drives, not one set by a command the
    /// instance has not received yet (the mixer sends overrides only while it renders), not one
    /// its editor reported already, not float noise.
    #[test]
    fn a_capture_returns_the_overrides_the_plugin_changed_by_itself() {
        let (mut side, mut s, k) = hosted_slot();
        let ins = insert_mut(&mut s, k);
        let overrides =
            [("preset", 1.0), ("kept", 2.0), ("automated", 3.0), ("not sent yet", 4.0), ("reported", 5.0), ("noise", 500.0), ("unread", 6.0)];
        for (id, v) in overrides {
            ins.params.insert(id.into(), v);
        }
        let t = s.track_mut(k.0).unwrap();
        t.lane_mut(&AutoParam::Plugin { slot: 0, param: "automated".into() }).set_point(0, 0.25);
        // A lane with no points drives nothing.
        let _ = t.lane_mut(&AutoParam::Plugin { slot: 0, param: "preset".into() });
        let reads = || {
            Box::new(Reads(vec![
                ("preset", 7.0, 1.0),
                ("kept", 2.0, 2.0),
                ("automated", 0.25, 3.0),
                ("not sent yet", 0.5, 0.5),
                ("reported", 5.0, 0.75),
                ("noise", 500.001, 500.0),
            ]))
        };
        side.states.insert(k, reads());
        let want = CapturedState { track: k.0, slot: 0, state: b"live".to_vec(), params: vec![("preset".into(), 7.0)] };
        assert_eq!(side.capture_states(&s), vec![want.clone()]);
        // With the track's automation off, the mixer sends the override, so the lane does not
        // keep the plugin's own value out.
        s.track_mut(k.0).unwrap().mixer.automation_mode = soundcraft_model::AutomationMode::Off;
        side.states.insert(k, reads());
        let mut want = want;
        want.params.insert(0, ("automated".into(), 0.25));
        assert_eq!(side.capture_states(&s), vec![want]);
    }

    /// An opened session's inserts are new, even where track ids and plugins match the old one's,
    /// and so is the same plugin inserted again into its own slot.
    #[test]
    fn an_opened_session_or_a_new_insert_gets_a_new_instance() {
        let (mut side, mut s, k) = hosted_slot();
        let opened: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        side.prepare(&opened);
        assert!(!side.states.contains_key(&k), "the previous session's instance was kept");
        side.prepare(&s);
        side.states.insert(k, live(b"tweaked"));
        s.track_mut(k.0).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
        side.prepare(&s);
        assert!(!side.states.contains_key(&k), "the new insert kept the old, tweaked instance");
    }

    #[test]
    fn replaced_sessions_are_freed_on_the_ui_thread() {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let mut side = Side::new(48_000.0, grx);
        side.return_old_sessions(&tx);
        let mut out = vec![0.0f32; 256];
        let mut watched = Vec::new();
        for _ in 0..3 * OLD_SESSIONS {
            let s = Arc::new(soundcraft_model::Session::default());
            watched.push(Arc::downgrade(&s));
            side.collect_garbage();
            tx.send(Cmd::Session(s, Vec::new())).unwrap();
            st.fill(&mut out, 2);
            let replaced = watched.len().checked_sub(2).and_then(|i| watched.get(i));
            assert!(replaced.is_none_or(|w| w.strong_count() == 1), "freed on the audio thread");
        }
        side.collect_garbage();
        assert_eq!(watched.iter().filter(|w| w.strong_count() > 0).count(), 1, "only the live snapshot is left");
    }

    #[test]
    fn looping_wraps() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: Some(Range::new(0, 1000)) }).unwrap();
        let mut out = vec![0.0f32; 20_000];
        st.fill(&mut out, 2);
        let p = shared.position.load(Ordering::Relaxed);
        assert!(p <= 1000, "position {p} should stay inside the loop");
        assert!(shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn locate_while_playing_drops_the_old_position_audio() {
        use soundcraft_model::{ChannelFormat, Clip, Insert, SourceAudio, SourceId, TrackKind};
        for (plugin, params) in [("compressor", &[("lookahead", 5.0), ("ratio", 1.0)][..]), ("room_reverb", &[][..])] {
            let mut s = soundcraft_model::Session::default();
            s.edit.click = false;
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            s.pool.insert(
                SourceId(5),
                Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 4800]] })),
            );
            let id = s.new_clip_id();
            let tr = s.track_mut(t).unwrap();
            tr.playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 4800));
            let mut ins = Insert::new(plugin);
            for (k, v) in params {
                ins.params.insert((*k).into(), *v);
            }
            tr.mixer.inserts[0] = Some(ins);
            let shared = Arc::new(Shared {
                position: AtomicI64::new(0),
                playing: AtomicBool::new(false),
                speed: AtomicU32::new(1000),
                meters: Mutex::new(MeterSnapshot::default()),
            });
            let (tx, rx) = channel();
            let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, channel().0);
            tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
            let mut out = vec![0.0f32; 2 * 512];
            let mut before = 0.0f32;
            for _ in 0..9 {
                st.fill(&mut out, 2);
                before = out.iter().fold(before, |m, x| m.max(x.abs()));
            }
            assert!(before > 0.1, "{plugin}: the clip plays before the locate");
            tx.send(Cmd::Locate(1_000_000)).unwrap();
            for _ in 0..8 {
                st.fill(&mut out, 2);
                assert!(out.iter().all(|x| x.abs() < 1e-6), "{plugin}: audio from before the locate");
            }
        }
    }

    #[test]
    fn armed_tracks_are_heard_and_metered_after_playback_stops() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.record_arm = true;
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, channel().0);
        let ring = record::InputRing::new(1, 48_000);
        tx.send(Cmd::Input(Arc::clone(&ring))).unwrap();
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(!shared.playing.load(Ordering::Relaxed));
        ring.push(&[0.5; 4096]);
        st.fill(&mut out, 2);
        assert!(out.iter().any(|x| x.abs() > 0.1), "the input is heard");
        assert!(shared.meters.lock().unwrap().tracks.get(&t).is_some_and(|m| m.peak[0] > 0.1), "and metered");
    }
}
