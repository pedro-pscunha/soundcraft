//! The only module with `unsafe`: hand-written AudioToolbox / CoreFoundation declarations, a
//! thin safe wrapper over one Audio Unit instance, and its view in a host window (AppKit through
//! `objc2`).
//!
//! Every pointer handed to the Audio Unit points into memory this module owns for at least as
//! long as the unit can use it: the input state the render callback reads and the host time the
//! host callbacks read are leaked `Box`es freed only after the unit is disposed, and output buffers
//! are re-pointed before each render. A unit is disposed only once its views are gone: on the
//! main thread, after its editor is closed and any view that outlived it was released.

use objc2::MainThreadMarker;
use soundcraft_dsp::{RenderMode, Transport};
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

type OSStatus = i32;
type AudioComponent = *mut c_void;
type AudioUnit = *mut c_void;
type CFTypeRef = *const c_void;
type CFIndex = isize;

/// Most channels on one Audio Unit bus that we set up.
pub const MAX_AU_CHANNELS: usize = 8;
/// Largest render slice we configure.
pub const MAX_SLICE: usize = 8192;
/// Most parameters read from one unit.
const MAX_PARAMS: usize = 4096;
/// Most components listed for one type.
const MAX_COMPONENTS: usize = 4096;
/// Most value strings read for one indexed parameter.
const MAX_VALUE_STRINGS: CFIndex = 256;
/// Largest state blob accepted or produced.
const MAX_STATE: usize = 64 << 20;
/// Longest CFString converted (bytes).
const MAX_STRING: CFIndex = 4096;

const K_AUDIO_COMPONENT_FLAG_REQUIRES_ASYNC: u32 = 8;

const PROP_CLASS_INFO: u32 = 0;
const PROP_PARAMETER_LIST: u32 = 3;
const PROP_PARAMETER_INFO: u32 = 4;
const PROP_STREAM_FORMAT: u32 = 8;
const PROP_LATENCY: u32 = 12;
const PROP_MAX_FRAMES: u32 = 14;
const PROP_PARAMETER_VALUE_STRINGS: u32 = 16;
const PROP_TAIL_TIME: u32 = 20;
const PROP_SET_RENDER_CALLBACK: u32 = 23;
const PROP_HOST_CALLBACKS: u32 = 27;
const PROP_COCOA_UI: u32 = 31;
const PROP_OFFLINE_RENDER: u32 = 37;

/// Most Cocoa view classes read from one unit.
const MAX_VIEW_CLASSES: usize = 64;

/// `kAudioUnitErr_InvalidProperty`: the unit has no such property.
const ERR_INVALID_PROPERTY: OSStatus = -10879;

/// `kAUParameterListener_AnyParameter`: "any of the unit's parameters" (notifications only).
const ANY_PARAMETER: u32 = 0xFFFF_FFFF;

const SCOPE_GLOBAL: u32 = 0;
const SCOPE_INPUT: u32 = 1;
const SCOPE_OUTPUT: u32 = 2;

const FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const FORMAT_FLAG_FLOAT: u32 = 1;
const FORMAT_FLAG_PACKED: u32 = 1 << 3;
const FORMAT_FLAG_NON_INTERLEAVED: u32 = 1 << 5;

const TIMESTAMP_SAMPLE_TIME_VALID: u32 = 1;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_PROPERTY_LIST_BINARY_FORMAT: CFIndex = 200;

pub const PARAM_FLAG_CF_NAME_RELEASE: u32 = 1 << 4;
pub const PARAM_FLAG_HAS_CF_NAME: u32 = 1 << 27;
pub const PARAM_FLAG_DISPLAY_LOG: u32 = 1 << 22;
#[cfg(test)]
pub const PARAM_FLAG_READABLE: u32 = 1 << 30;
pub const PARAM_FLAG_WRITABLE: u32 = 1 << 31;
pub const PARAM_FLAG_METER_READ_ONLY: u32 = 1 << 15;

pub const UNIT_INDEXED: u32 = 1;
pub const UNIT_CUSTOM: u32 = 26;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmpteTime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    smpte_type: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SmpteTime,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

/// `AudioBufferList` with room for [`MAX_AU_CHANNELS`] buffers (the unit reads only
/// `number_buffers` of them).
#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; MAX_AU_CHANNELS],
}

/// The variable-length list the unit hands the render callback (we only see its header and
/// index into the buffers it says it has).
#[repr(C)]
struct AudioBufferListHeader {
    number_buffers: u32,
    first: AudioBuffer,
}

type BeatAndTempoProc = unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> OSStatus;
type MusicalTimeProc = unsafe extern "C" fn(*mut c_void, *mut u32, *mut f32, *mut u32, *mut f64) -> OSStatus;
type TransportStateProc = unsafe extern "C" fn(*mut c_void, *mut u8, *mut u8, *mut f64, *mut u8, *mut f64, *mut f64) -> OSStatus;
type TransportState2Proc = unsafe extern "C" fn(*mut c_void, *mut u8, *mut u8, *mut u8, *mut f64, *mut u8, *mut f64, *mut f64) -> OSStatus;

#[repr(C)]
struct HostCallbackInfo {
    host_user_data: *mut c_void,
    beat_and_tempo: Option<BeatAndTempoProc>,
    musical_time_location: Option<MusicalTimeProc>,
    transport_state: Option<TransportStateProc>,
    transport_state2: Option<TransportState2Proc>,
}

/// What the host callbacks report: the transport of the block being rendered. Atomics, because a
/// unit may also ask from another thread.
struct HostTime {
    playing: AtomicBool,
    recording: AtomicBool,
    changed: AtomicBool,
    samples: AtomicI64,
    tempo: AtomicU64,
    quarters: AtomicU64,
    bar_start: AtomicU64,
    numerator: AtomicU32,
    denominator: AtomicU32,
    sample_rate: AtomicU64,
}

impl HostTime {
    fn new() -> HostTime {
        HostTime {
            playing: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            samples: AtomicI64::new(0),
            tempo: AtomicU64::new(120f64.to_bits()),
            quarters: AtomicU64::new(0),
            bar_start: AtomicU64::new(0),
            numerator: AtomicU32::new(4),
            denominator: AtomicU32::new(4),
            sample_rate: AtomicU64::new(48_000f64.to_bits()),
        }
    }

    fn set(&self, t: &Transport) {
        if self.playing.swap(t.playing, Ordering::Relaxed) != t.playing {
            self.changed.store(true, Ordering::Relaxed);
        }
        self.recording.store(t.recording, Ordering::Relaxed);
        self.samples.store(t.samples, Ordering::Relaxed);
        self.tempo.store(t.tempo.to_bits(), Ordering::Relaxed);
        self.quarters.store(t.quarters.to_bits(), Ordering::Relaxed);
        self.bar_start.store(t.bar_start.to_bits(), Ordering::Relaxed);
        self.numerator.store(t.numerator, Ordering::Relaxed);
        self.denominator.store(t.denominator, Ordering::Relaxed);
    }
}

fn load_f64(a: &AtomicU64) -> f64 {
    f64::from_bits(a.load(Ordering::Relaxed))
}

/// Writes through an out-pointer the unit may leave null.
///
/// # Safety
/// `p` is null or valid for writing a `T`.
unsafe fn put<T>(p: *mut T, v: T) {
    // SAFETY: per the contract above.
    if let Some(p) = unsafe { p.as_mut() } {
        *p = v;
    }
}

unsafe extern "C" fn beat_and_tempo(user: *mut c_void, beat: *mut f64, tempo: *mut f64) -> OSStatus {
    // SAFETY: `user` is the `HostTime` leaked by `Instance::new`, alive until after the unit is
    // disposed.
    let Some(t) = (unsafe { (user as *const HostTime).as_ref() }) else { return -50 };
    // SAFETY: the unit's out-pointers (or null).
    unsafe {
        put(beat, load_f64(&t.quarters));
        put(tempo, load_f64(&t.tempo));
    }
    0
}

unsafe extern "C" fn musical_time(
    user: *mut c_void,
    to_next_beat: *mut u32,
    numerator: *mut f32,
    denominator: *mut u32,
    downbeat: *mut f64,
) -> OSStatus {
    // SAFETY: as in `beat_and_tempo`.
    let Some(t) = (unsafe { (user as *const HostTime).as_ref() }) else { return -50 };
    let q = load_f64(&t.quarters);
    let per_quarter = load_f64(&t.sample_rate) * 60.0 / load_f64(&t.tempo).max(1.0);
    let next = ((q.ceil() - q) * per_quarter).round();
    let next = if next.is_finite() { next.clamp(0.0, f64::from(u32::MAX)) as u32 } else { 0 };
    // SAFETY: the unit's out-pointers (or null).
    unsafe {
        put(to_next_beat, next);
        put(numerator, t.numerator.load(Ordering::Relaxed) as f32);
        put(denominator, t.denominator.load(Ordering::Relaxed));
        put(downbeat, load_f64(&t.bar_start));
    }
    0
}

unsafe extern "C" fn transport_state2(
    user: *mut c_void,
    playing: *mut u8,
    recording: *mut u8,
    changed: *mut u8,
    sample: *mut f64,
    cycling: *mut u8,
    cycle_start: *mut f64,
    cycle_end: *mut f64,
) -> OSStatus {
    // SAFETY: as in `beat_and_tempo`.
    let Some(t) = (unsafe { (user as *const HostTime).as_ref() }) else { return -50 };
    // SAFETY: the unit's out-pointers (or null).
    unsafe {
        put(playing, u8::from(t.playing.load(Ordering::Relaxed)));
        put(recording, u8::from(t.recording.load(Ordering::Relaxed)));
        put(changed, u8::from(t.changed.swap(false, Ordering::Relaxed)));
        put(sample, t.samples.load(Ordering::Relaxed) as f64);
        put(cycling, 0);
        put(cycle_start, 0.0);
        put(cycle_end, 0.0);
    }
    0
}

unsafe extern "C" fn transport_state(
    user: *mut c_void,
    playing: *mut u8,
    changed: *mut u8,
    sample: *mut f64,
    cycling: *mut u8,
    cycle_start: *mut f64,
    cycle_end: *mut f64,
) -> OSStatus {
    // SAFETY: the same pointers as `transport_state2`, without the recording flag.
    unsafe { transport_state2(user, playing, ptr::null_mut(), changed, sample, cycling, cycle_start, cycle_end) }
}

type AuRenderCallback = unsafe extern "C" fn(
    ref_con: *mut c_void,
    action_flags: *mut u32,
    time_stamp: *const AudioTimeStamp,
    bus: u32,
    frames: u32,
    data: *mut AudioBufferListHeader,
) -> OSStatus;

#[repr(C)]
struct AuRenderCallbackStruct {
    input_proc: Option<AuRenderCallback>,
    input_proc_ref_con: *mut c_void,
}

/// One parameter of one unit, as parameter listeners see it.
#[repr(C)]
struct AudioUnitParameter {
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
}

type ParameterListenerProc = unsafe extern "C" fn(user: *mut c_void, object: *mut c_void, parameter: *const AudioUnitParameter, value: f32);

#[repr(C)]
struct AudioUnitParameterInfo {
    name: [c_char; 52],
    unit_name: CFTypeRef,
    clump_id: u32,
    cf_name_string: CFTypeRef,
    unit: u32,
    min_value: f32,
    max_value: f32,
    default_value: f32,
    flags: u32,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(component: AudioComponent, desc: *const AudioComponentDescription) -> AudioComponent;
    fn AudioComponentCopyName(component: AudioComponent, name: *mut CFTypeRef) -> OSStatus;
    fn AudioComponentGetDescription(component: AudioComponent, desc: *mut AudioComponentDescription) -> OSStatus;
    fn AudioComponentGetVersion(component: AudioComponent, version: *mut u32) -> OSStatus;
    fn AudioComponentInstanceNew(component: AudioComponent, instance: *mut AudioUnit) -> OSStatus;
    fn AudioComponentInstanceDispose(instance: AudioUnit) -> OSStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitGetPropertyInfo(unit: AudioUnit, id: u32, scope: u32, element: u32, size: *mut u32, writable: *mut u8) -> OSStatus;
    fn AudioUnitGetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32) -> OSStatus;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> OSStatus;
    fn AudioUnitGetParameter(unit: AudioUnit, id: u32, scope: u32, element: u32, value: *mut f32) -> OSStatus;
    fn AudioUnitSetParameter(unit: AudioUnit, id: u32, scope: u32, element: u32, value: f32, offset: u32) -> OSStatus;
    fn AudioUnitRender(
        unit: AudioUnit,
        action_flags: *mut u32,
        time_stamp: *const AudioTimeStamp,
        bus: u32,
        frames: u32,
        data: *mut AudioBufferList,
    ) -> OSStatus;
    fn AudioUnitReset(unit: AudioUnit, scope: u32, element: u32) -> OSStatus;
    fn MusicDeviceMIDIEvent(unit: AudioUnit, status: u32, data1: u32, data2: u32, offset: u32) -> OSStatus;
    fn AUParameterListenerNotify(sending_listener: *mut c_void, sending_object: *mut c_void, parameter: *const AudioUnitParameter) -> OSStatus;
    fn AUParameterSet(
        sending_listener: *mut c_void,
        sending_object: *mut c_void,
        parameter: *const AudioUnitParameter,
        value: f32,
        offset: u32,
    ) -> OSStatus;
    fn AUListenerCreate(
        listener_proc: ParameterListenerProc,
        user: *mut c_void,
        run_loop: CFTypeRef,
        mode: CFTypeRef,
        interval: f32,
        out: *mut *mut c_void,
    ) -> OSStatus;
    fn AUListenerAddParameter(listener: *mut c_void, object: *mut c_void, parameter: *const AudioUnitParameter) -> OSStatus;
    fn AUListenerDispose(listener: *mut c_void) -> OSStatus;
}

// `AUGenericView`, the view for units without one of their own (looked up by name at runtime).
#[link(name = "CoreAudioKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFURLGetTypeID() -> usize;
    fn CFStringGetLength(s: CFTypeRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(s: CFTypeRef, buffer: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFArrayGetCount(a: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: CFIndex) -> CFTypeRef;
    fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, length: CFIndex) -> CFTypeRef;
    fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
    fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    fn CFPropertyListCreateData(alloc: CFTypeRef, plist: CFTypeRef, format: CFIndex, options: usize, error: *mut CFTypeRef) -> CFTypeRef;
    fn CFPropertyListCreateWithData(alloc: CFTypeRef, data: CFTypeRef, options: usize, format: *mut CFIndex, error: *mut CFTypeRef) -> CFTypeRef;
}

/// Releases a CF object we own (null is ignored).
fn release(cf: CFTypeRef) {
    if !cf.is_null() {
        // SAFETY: `cf` is a non-null CF object the caller owns a +1 reference to.
        unsafe { CFRelease(cf) }
    }
}

/// The unit's `ClassInfo` property list (+1: the caller releases it), or null when the unit
/// refuses.
///
/// # Safety
/// `unit` is a live Audio Unit.
unsafe fn class_info(unit: AudioUnit) -> CFTypeRef {
    let mut plist: CFTypeRef = ptr::null();
    let mut size = std::mem::size_of::<CFTypeRef>() as u32;
    // SAFETY: `unit` is live (the contract above); the property is a `CFPropertyListRef`
    // returned +1; `plist`/`size` are locals of the right types.
    let st = unsafe { AudioUnitGetProperty(unit, PROP_CLASS_INFO, SCOPE_GLOBAL, 0, (&mut plist as *mut CFTypeRef).cast(), &mut size) };
    if st != 0 {
        release(plist);
        return ptr::null();
    }
    plist
}

/// The location of the unit's view bundle and the name of its first view factory class
/// (`kAudioUnitProperty_CocoaUI`), +1 each (the caller releases both), or `None` when it
/// publishes no Cocoa view.
///
/// # Safety
/// `unit` is a live Audio Unit.
unsafe fn cocoa_ui(unit: AudioUnit) -> Option<(CFTypeRef, CFTypeRef)> {
    let word = std::mem::size_of::<CFTypeRef>();
    let mut size = 0u32;
    let mut writable = 0u8;
    // SAFETY: the unit is live (the contract above); out pointers are locals.
    let st = unsafe { AudioUnitGetPropertyInfo(unit, PROP_COCOA_UI, SCOPE_GLOBAL, 0, &mut size, &mut writable) };
    // `AudioUnitCocoaViewInfo`: a bundle URL, then one class name per view.
    let n = size as usize / word;
    if st != 0 || !(2..=MAX_VIEW_CLASSES + 1).contains(&n) {
        return None;
    }
    let mut refs: Vec<CFTypeRef> = vec![ptr::null(); n];
    let mut size = (n * word) as u32;
    // SAFETY: `refs` has room for `size` bytes; the unit fills it with a CFURL and CFStrings,
    // each returned +1.
    let st = unsafe { AudioUnitGetProperty(unit, PROP_COCOA_UI, SCOPE_GLOBAL, 0, refs.as_mut_ptr().cast(), &mut size) };
    if st != 0 {
        // Whatever a failed call left in the buffer is not ours to release.
        return None;
    }
    let got = (size as usize / word).min(n);
    for &extra in refs.iter().take(got).skip(2) {
        release(extra);
    }
    let (url, class) = (refs.first().copied().unwrap_or(ptr::null()), refs.get(1).copied().unwrap_or(ptr::null()));
    // SAFETY: non-null CF objects the unit handed us (the type checks guard their use).
    let typed = !url.is_null() && !class.is_null() && unsafe { CFGetTypeID(url) == CFURLGetTypeID() && CFGetTypeID(class) == CFStringGetTypeID() };
    if got < 2 || !typed {
        release(url);
        release(class);
        return None;
    }
    Some((url, class))
}

/// Parameter edits made in a unit's view: the latest value of each parameter not yet taken.
/// The editor's parameter listener writes them on whichever thread the view sets a value (lock-
/// and allocation-free); the editor takes them on the main thread.
///
/// A view that loads a preset sends one "any parameter" notice, which AudioToolbox hands the
/// listener as one call per parameter it listens to, each with the parameter's current value: the
/// unchanged ones too. So the editor reports only values that differ from what the host last knew
/// of each parameter (`known`).
pub struct Edits {
    /// `(AudioUnitParameterID, index)`, sorted by id.
    index: Vec<(u32, usize)>,
    /// By index: `PENDING` plus the value's bits, or 0 once taken.
    slots: Vec<AtomicU64>,
    any: AtomicBool,
    /// By index: the bits of the last value the host knows the unit has (one it set, showed in
    /// the view, read after a state load, or reported as an edit).
    known: Vec<AtomicU32>,
}

const PENDING: u64 = 1 << 32;

impl Edits {
    /// Edits of the parameters `ids`, reported by their index in `ids`; `values` are what the host
    /// knows of them now (by index).
    pub fn new(ids: &[u32], values: &[f32]) -> Edits {
        let mut index: Vec<(u32, usize)> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        index.sort_unstable();
        Edits {
            index,
            slots: ids.iter().map(|_| AtomicU64::new(0)).collect(),
            any: AtomicBool::new(false),
            known: (0..ids.len()).map(|i| AtomicU32::new(values.get(i).copied().unwrap_or(f32::NAN).to_bits())).collect(),
        }
    }

    /// Notes that parameter `i` has `value` as far as the host knows (lock-free: the audio thread
    /// calls it too).
    pub fn know(&self, i: usize, value: f32) {
        if let Some(k) = self.known.get(i) {
            k.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    /// The value the host last knew parameter `i` has.
    pub fn known(&self, i: usize) -> Option<f32> {
        self.known.get(i).map(|k| f32::from_bits(k.load(Ordering::Relaxed)))
    }

    fn record(&self, id: u32, value: f32) {
        if !value.is_finite() {
            return;
        }
        let Ok(k) = self.index.binary_search_by_key(&id, |&(p, _)| p) else { return };
        if let Some(slot) = self.index.get(k).and_then(|&(_, i)| self.slots.get(i)) {
            slot.store(PENDING | u64::from(value.to_bits()), Ordering::Release);
            self.any.store(true, Ordering::Release);
        }
    }

    /// The edits since the last call, as `(index, value)`, one per parameter (its latest value).
    pub fn take(&self) -> Vec<(usize, f32)> {
        if !self.any.swap(false, Ordering::Acquire) {
            return Vec::new();
        }
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let bits = s.swap(0, Ordering::AcqRel);
                (bits & PENDING != 0).then(|| (i, f32::from_bits(bits as u32)))
            })
            .collect()
    }
}

/// Records a parameter value change into the `Edits` behind `user`.
unsafe extern "C" fn edit_listener(user: *mut c_void, _object: *mut c_void, parameter: *const AudioUnitParameter, value: f32) {
    // SAFETY: `user` is the `Edits` the listener's `EditListener` keeps alive until after the
    // listener is disposed; `parameter` is AudioToolbox's in-pointer (or null).
    let (Some(edits), Some(p)) = (unsafe { (user as *const Edits).as_ref() }, unsafe { parameter.as_ref() }) else { return };
    if p.scope == SCOPE_GLOBAL && p.element == 0 {
        edits.record(p.id, value);
    }
}

/// A parameter listener reporting the value changes of a unit's global parameters into an
/// [`Edits`]. Views set parameters with `AUParameterSet` (which notifies listeners); a host's
/// `AudioUnitSetParameter` notifies nobody, so only the view's edits arrive. Created with a zero
/// notification interval, so it is called at once on the thread that set the value, with no run
/// loop involved (an event listener, by contrast, is always called from a run loop). Disposed on
/// drop.
pub(crate) struct EditListener {
    listener: *mut c_void,
    /// Read by the listener until it is disposed.
    _edits: Arc<Edits>,
}

// SAFETY: the listener handle may be used and disposed from any thread (AudioToolbox serializes
// its listener calls); the edits are shared atomics.
unsafe impl Send for EditListener {}

impl EditListener {
    /// Listens to `ids` of `unit`. `None` (logged) when AudioToolbox refuses.
    ///
    /// # Safety
    /// `unit` is a live Audio Unit, and the listener is dropped before the unit is disposed.
    unsafe fn new(unit: AudioUnit, ids: &[u32], edits: Arc<Edits>) -> Option<EditListener> {
        let mut listener: *mut c_void = ptr::null_mut();
        let user = Arc::as_ptr(&edits).cast_mut().cast::<c_void>();
        // SAFETY: `user` stays valid while the listener lives (`_edits`, dropped after the dispose);
        // no run loop or mode (both may be null with a zero interval); `listener` is a local.
        let st = unsafe { AUListenerCreate(edit_listener, user, ptr::null(), ptr::null(), 0.0, &mut listener) };
        if st != 0 || listener.is_null() {
            log::warn!("au: no parameter listener for the editor ({st}); its edits reach the session through the plugin's state only");
            return None;
        }
        let me = EditListener { listener, _edits: edits };
        for &id in ids.iter().filter(|&&id| id != ANY_PARAMETER) {
            let p = AudioUnitParameter { unit, id, scope: SCOPE_GLOBAL, element: 0 };
            // SAFETY: a live listener and unit (the contract above); `p` is a local.
            unsafe { AUListenerAddParameter(me.listener, ptr::null_mut(), &p) };
        }
        Some(me)
    }
}

impl Drop for EditListener {
    fn drop(&mut self) {
        // SAFETY: the listener created in `new`, disposed once.
        unsafe { AUListenerDispose(self.listener) };
    }
}

/// Converts a borrowed CFString to a Rust string (empty on any failure or a non-string).
fn cf_string(s: CFTypeRef) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: `s` is a live, non-null CF object (borrowed from the caller); the type check
    // guards the CFString calls, and the buffer is sized by CoreFoundation's own maximum.
    unsafe {
        if CFGetTypeID(s) != CFStringGetTypeID() {
            return String::new();
        }
        let len = CFStringGetLength(s).clamp(0, MAX_STRING);
        let cap = CFStringGetMaximumSizeForEncoding(len, CF_STRING_ENCODING_UTF8);
        if cap <= 0 {
            return String::new();
        }
        let cap = cap.saturating_add(1).min(MAX_STRING * 4 + 1);
        let mut buf = vec![0 as c_char; cap as usize];
        if CFStringGetCString(s, buf.as_mut_ptr(), cap, CF_STRING_ENCODING_UTF8) == 0 {
            return String::new();
        }
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// One registered component.
#[derive(Debug, Clone)]
pub struct RawComponent {
    pub ty: u32,
    pub subtype: u32,
    pub manufacturer: u32,
    /// `"Vendor: Name"` as registered.
    pub name: String,
    pub version: u32,
}

/// Every component of type `ty` that can be instantiated synchronously.
pub fn components(ty: u32) -> Vec<RawComponent> {
    let desc = AudioComponentDescription { component_type: ty, ..Default::default() };
    let mut out = Vec::new();
    let mut comp: AudioComponent = ptr::null_mut();
    for _ in 0..MAX_COMPONENTS {
        // SAFETY: `desc` is a valid description; `comp` is null or a component returned by the
        // previous call (components live for the process).
        comp = unsafe { AudioComponentFindNext(comp, &desc) };
        if comp.is_null() {
            break;
        }
        let mut d = AudioComponentDescription::default();
        let mut name: CFTypeRef = ptr::null();
        let mut version = 0u32;
        // SAFETY: `comp` is a live component; the out pointers point at locals of the right types.
        let (ds, ns) = unsafe {
            let ds = AudioComponentGetDescription(comp, &mut d);
            let ns = AudioComponentCopyName(comp, &mut name);
            let _ = AudioComponentGetVersion(comp, &mut version);
            (ds, ns)
        };
        let full = if ns == 0 { cf_string(name) } else { String::new() };
        release(name);
        if ds != 0 || d.component_type != ty || d.component_flags & K_AUDIO_COMPONENT_FLAG_REQUIRES_ASYNC != 0 {
            continue;
        }
        out.push(RawComponent { ty, subtype: d.component_sub_type, manufacturer: d.component_manufacturer, name: full, version });
    }
    out
}

/// One global-scope parameter as the unit describes it.
#[derive(Debug, Clone)]
pub struct RawParam {
    pub id: u32,
    pub name: String,
    pub unit: u32,
    pub custom_unit: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub flags: u32,
    /// Names of the values of an indexed parameter (when it has them).
    pub value_strings: Vec<String>,
}

/// The state the input render callback reads: one buffer per channel, filled by the host before
/// each render.
struct InputState {
    bufs: Vec<Vec<f32>>,
    frames: usize,
}

/// Supplies the effect's input: copies (or lends) our input buffers into the unit's list.
unsafe extern "C" fn input_callback(
    ref_con: *mut c_void,
    _action_flags: *mut u32,
    _time_stamp: *const AudioTimeStamp,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferListHeader,
) -> OSStatus {
    if ref_con.is_null() || data.is_null() {
        return -50; // paramErr
    }
    // SAFETY: `ref_con` is the `InputState` leaked by `Instance::new`, alive until after the unit
    // is disposed, and only read here while the host is inside `AudioUnitRender` (it does not
    // touch it concurrently). `data` is the unit's buffer list with `number_buffers` buffers laid
    // out contiguously from `first`.
    unsafe {
        let input = &mut *(ref_con as *mut InputState);
        let n = (*data).number_buffers as usize;
        let first: *mut AudioBuffer = ptr::addr_of_mut!((*data).first);
        let frames = (frames as usize).min(MAX_SLICE);
        let avail = input.frames.min(frames);
        for i in 0..n.min(64) {
            let b = &mut *first.add(i);
            let last = input.bufs.len().saturating_sub(1);
            let Some(src) = input.bufs.get_mut(i.min(last)) else { continue };
            if b.data.is_null() {
                // Lend our buffer (the unit may process in place; it is ours to scribble on).
                if src.len() >= frames {
                    b.data = src.as_mut_ptr().cast();
                    b.data_byte_size = (frames * 4) as u32;
                    b.number_channels = 1;
                }
                continue;
            }
            let room = (b.data_byte_size as usize / 4).min(frames);
            let dst = std::slice::from_raw_parts_mut(b.data as *mut f32, room);
            for (k, d) in dst.iter_mut().enumerate() {
                *d = if k < avail { src.get(k).copied().unwrap_or(0.0) } else { 0.0 };
            }
        }
    }
    0
}

/// A live Audio Unit instance.
pub struct Instance {
    unit: AudioUnit,
    /// Leaked `Box<InputState>` read by `input_callback`; freed in `Drop` after disposal.
    input: *mut InputState,
    /// Leaked `Box<HostTime>` read by the host callbacks; freed in `Drop` after disposal.
    time: *mut HostTime,
    outputs: Vec<Vec<f32>>,
    abl: AudioBufferList,
    has_input: bool,
    initialized: bool,
    /// The render mode the unit is in (set by `configure`); `None` when it has no
    /// `OfflineRender` property, so it renders the same either way.
    mode: Option<RenderMode>,
    channels: usize,
    max_frames: usize,
    sample_rate: f64,
    sample_time: f64,
    link: Arc<UnitLink>,
}

// SAFETY: an Audio Unit instance may be used from any thread as long as calls are not
// concurrent; `Instance` is only reachable through `&mut`/`&` of its owner, which the `Plugin`
// contract keeps on one thread at a time. The input state and host time are owned exclusively by
// it (the host time is all atomics).
unsafe impl Send for Instance {}

impl Instance {
    /// Creates (but does not initialize) the component `(ty, subtype, manufacturer)`.
    pub fn new(ty: u32, subtype: u32, manufacturer: u32, has_input: bool) -> Result<Instance, String> {
        let desc =
            AudioComponentDescription { component_type: ty, component_sub_type: subtype, component_manufacturer: manufacturer, ..Default::default() };
        // SAFETY: `desc` is a valid description.
        let comp = unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) };
        if comp.is_null() {
            return Err("component not registered".into());
        }
        let mut d = AudioComponentDescription::default();
        // SAFETY: `comp` is a live component; `d` is a local of the right type.
        if unsafe { AudioComponentGetDescription(comp, &mut d) } == 0 && d.component_flags & K_AUDIO_COMPONENT_FLAG_REQUIRES_ASYNC != 0 {
            return Err("requires asynchronous instantiation (not supported)".into());
        }
        let mut unit: AudioUnit = ptr::null_mut();
        // SAFETY: `comp` is a live component; `unit` receives the new instance.
        let st = unsafe { AudioComponentInstanceNew(comp, &mut unit) };
        if st != 0 || unit.is_null() {
            return Err(format!("AudioComponentInstanceNew failed ({st})"));
        }
        let input = Box::into_raw(Box::new(InputState { bufs: Vec::new(), frames: 0 }));
        let time = Box::into_raw(Box::new(HostTime::new()));
        let empty = AudioBuffer { number_channels: 1, data_byte_size: 0, data: ptr::null_mut() };
        let inst = Instance {
            unit,
            input,
            time,
            outputs: Vec::new(),
            abl: AudioBufferList { number_buffers: 0, buffers: [empty; MAX_AU_CHANNELS] },
            has_input,
            link: Arc::new(UnitLink(std::sync::Mutex::new(Linked { unit: Some(unit), editor: None, listener: None, closed: Vec::new() }))),
            initialized: false,
            mode: Some(RenderMode::Realtime),
            channels: 0,
            max_frames: 0,
            sample_rate: 48_000.0,
            sample_time: 0.0,
        };
        let callbacks = HostCallbackInfo {
            host_user_data: time.cast(),
            beat_and_tempo: Some(beat_and_tempo),
            musical_time_location: Some(musical_time),
            transport_state: Some(transport_state),
            transport_state2: Some(transport_state2),
        };
        // Units that never ask for host time may refuse it.
        let _ = inst.set_property(PROP_HOST_CALLBACKS, SCOPE_GLOBAL, 0, &callbacks);
        Ok(inst)
    }

    fn time(&self) -> &HostTime {
        // SAFETY: `time` is the `HostTime` leaked in `new`, freed only in `Drop`.
        unsafe { &*self.time }
    }

    /// The transport the host callbacks report during the next renders.
    pub fn set_transport(&self, t: &Transport) {
        self.time().set(t);
    }

    fn set_property<T>(&self, id: u32, scope: u32, element: u32, value: &T) -> OSStatus {
        // SAFETY: `value` points at a live `T` of `size_of::<T>()` bytes, matching what the
        // property expects (callers pass the documented type).
        unsafe { AudioUnitSetProperty(self.unit, id, scope, element, (value as *const T).cast(), std::mem::size_of::<T>() as u32) }
    }

    fn get_f64(&self, id: u32) -> Option<f64> {
        let mut v = 0f64;
        let mut size = std::mem::size_of::<f64>() as u32;
        // SAFETY: the property is a `Float64`; `v`/`size` are locals of the right types.
        let st = unsafe { AudioUnitGetProperty(self.unit, id, SCOPE_GLOBAL, 0, (&mut v as *mut f64).cast(), &mut size) };
        (st == 0 && v.is_finite()).then_some(v)
    }

    fn format(&self, scope: u32, sample_rate: f64, channels: usize) -> OSStatus {
        let asbd = AudioStreamBasicDescription {
            sample_rate,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: FORMAT_FLAG_FLOAT | FORMAT_FLAG_PACKED | FORMAT_FLAG_NON_INTERLEAVED,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: channels as u32,
            bits_per_channel: 32,
            reserved: 0,
        };
        self.set_property(PROP_STREAM_FORMAT, scope, 0, &asbd)
    }

    /// (Re)configures and initializes the unit for `sample_rate`, `max_frames`-frame slices and
    /// (preferably) `channels` channels on its main input and output buses, in `mode` as far as
    /// the unit goes along (see [`Instance::render_mode`]). Returns the channel count it accepted.
    /// Not realtime-safe (allocates the buffers).
    pub fn configure(&mut self, sample_rate: f64, max_frames: usize, channels: usize, mode: RenderMode) -> Result<usize, String> {
        if self.initialized {
            // SAFETY: the unit is live and initialized.
            unsafe { AudioUnitUninitialize(self.unit) };
            self.initialized = false;
        }
        self.max_frames = 0;
        // Set while uninitialized, and only when it changes (as hosts do around a bounce).
        if self.mode.is_some_and(|m| m != mode) {
            match self.set_property(PROP_OFFLINE_RENDER, SCOPE_GLOBAL, 0, &u32::from(mode == RenderMode::Offline)) {
                0 => self.mode = Some(mode),
                ERR_INVALID_PROPERTY => self.mode = None,
                _ => {}
            }
        }
        let mf = max_frames.clamp(1, MAX_SLICE);
        let _ = self.set_property(PROP_MAX_FRAMES, SCOPE_GLOBAL, 0, &(mf as u32));
        if self.has_input {
            let cb = AuRenderCallbackStruct { input_proc: Some(input_callback), input_proc_ref_con: self.input.cast() };
            let st = self.set_property(PROP_SET_RENDER_CALLBACK, SCOPE_INPUT, 0, &cb);
            if st != 0 {
                return Err(format!("cannot set the input callback ({st})"));
            }
        }
        let want = channels.clamp(1, MAX_AU_CHANNELS);
        let mut tried = [0usize; 3];
        let mut ok = None;
        let mut last = 0;
        for (k, ch) in [want, 2, 1].into_iter().enumerate() {
            if tried.contains(&ch) {
                continue;
            }
            if let Some(t) = tried.get_mut(k) {
                *t = ch;
            }
            let o = self.format(SCOPE_OUTPUT, sample_rate, ch);
            let i = if self.has_input { self.format(SCOPE_INPUT, sample_rate, ch) } else { 0 };
            if o != 0 || i != 0 {
                last = if o != 0 { o } else { i };
                continue;
            }
            // SAFETY: the unit is live and uninitialized.
            let st = unsafe { AudioUnitInitialize(self.unit) };
            if st == 0 {
                ok = Some(ch);
                break;
            }
            last = st;
        }
        let ch = ok.ok_or_else(|| format!("the unit accepts no 1/2/{want}-channel float format ({last})"))?;
        self.initialized = true;
        self.channels = ch;
        self.sample_rate = sample_rate;
        self.time().sample_rate.store(sample_rate.to_bits(), Ordering::Relaxed);
        self.outputs = (0..ch).map(|_| vec![0.0; mf]).collect();
        // SAFETY: the callback only runs inside `AudioUnitRender`, which is not running now
        // (we hold `&mut self`), so this exclusive access does not alias.
        let input = unsafe { &mut *self.input };
        input.bufs = if self.has_input { (0..ch).map(|_| vec![0.0; mf]).collect() } else { Vec::new() };
        input.frames = 0;
        self.max_frames = mf;
        Ok(ch)
    }

    pub fn is_ready(&self) -> bool {
        self.initialized && self.max_frames > 0
    }

    /// The render mode the unit is in (`configure` switches it; one that refuses stays in its
    /// mode), or `None` when it has no offline render mode.
    pub fn render_mode(&self) -> Option<RenderMode> {
        self.mode
    }

    /// The unit's `OfflineRender` property as it reports it (`None` when it has none).
    #[cfg(test)]
    pub fn unit_reports_offline(&self) -> Option<bool> {
        let mut v = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: the property is a `UInt32`; `v`/`size` are locals of the right types.
        let st = unsafe { AudioUnitGetProperty(self.unit, PROP_OFFLINE_RENDER, SCOPE_GLOBAL, 0, (&mut v as *mut u32).cast(), &mut size) };
        (st == 0).then_some(v != 0)
    }

    pub fn max_frames(&self) -> usize {
        self.max_frames
    }

    /// Input buffer `ch` (for effects), `max_frames` long.
    pub fn input_mut(&mut self, ch: usize) -> Option<&mut [f32]> {
        // SAFETY: see `configure`: no render is running while we hold `&mut self`.
        let input = unsafe { &mut *self.input };
        input.bufs.get_mut(ch).map(Vec::as_mut_slice)
    }

    /// Renders `frames` (≤ `max_frames`) into the output buffers. Never allocates.
    pub fn render(&mut self, frames: usize) -> Result<(), OSStatus> {
        if !self.is_ready() || frames == 0 || frames > self.max_frames {
            return Err(-50);
        }
        {
            // SAFETY: as in `input_mut`.
            let input = unsafe { &mut *self.input };
            input.frames = frames;
        }
        let n = self.outputs.len().min(MAX_AU_CHANNELS);
        for (b, out) in self.abl.buffers.iter_mut().zip(self.outputs.iter_mut()) {
            *b = AudioBuffer { number_channels: 1, data_byte_size: (frames * 4) as u32, data: out.as_mut_ptr().cast() };
        }
        self.abl.number_buffers = n as u32;
        let ts = AudioTimeStamp { sample_time: self.sample_time, flags: TIMESTAMP_SAMPLE_TIME_VALID, ..Default::default() };
        let mut flags = 0u32;
        // SAFETY: the unit is initialized; `ts` and `flags` are locals; `abl` holds `n` buffers
        // each pointing at `frames` floats we own; the input callback's state is alive.
        let st = unsafe { AudioUnitRender(self.unit, &mut flags, &ts, 0, frames as u32, &mut self.abl) };
        self.sample_time += frames as f64;
        if st == 0 { Ok(()) } else { Err(st) }
    }

    /// Output channel `ch` of the last render (`frames` long), wherever the unit left it.
    pub fn output(&self, ch: usize, frames: usize) -> Option<&[f32]> {
        if ch >= self.abl.number_buffers as usize {
            return None;
        }
        let b = self.abl.buffers.get(ch)?;
        if b.data.is_null() {
            return None;
        }
        let len = (b.data_byte_size as usize / 4).min(frames);
        // SAFETY: after a successful render each buffer points at `data_byte_size` bytes of
        // floats, either ours or the unit's own (valid until its next render).
        Some(unsafe { std::slice::from_raw_parts(b.data as *const f32, len) })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn reset(&mut self) {
        // SAFETY: the unit is live.
        unsafe { AudioUnitReset(self.unit, SCOPE_GLOBAL, 0) };
    }

    /// Latency in samples at the configured rate.
    pub fn latency(&self) -> usize {
        self.seconds_to_samples(self.get_f64(PROP_LATENCY), 10.0)
    }

    /// Tail in samples at the configured rate (capped at a minute).
    pub fn tail(&self) -> usize {
        self.seconds_to_samples(self.get_f64(PROP_TAIL_TIME), 60.0)
    }

    fn seconds_to_samples(&self, s: Option<f64>, cap: f64) -> usize {
        match s {
            Some(s) if s > 0.0 => (s.min(cap) * self.sample_rate).round() as usize,
            _ => 0,
        }
    }

    /// A MIDI channel message at `offset` frames into the next render.
    pub fn midi(&self, status: u8, data1: u8, data2: u8, offset: u32) -> bool {
        // SAFETY: the unit is live; the call only takes integers.
        unsafe { MusicDeviceMIDIEvent(self.unit, u32::from(status), u32::from(data1), u32::from(data2), offset) == 0 }
    }

    pub fn get_param(&self, id: u32) -> Option<f32> {
        let mut v = 0f32;
        // SAFETY: the unit is live; `v` is a local `f32`.
        let st = unsafe { AudioUnitGetParameter(self.unit, id, SCOPE_GLOBAL, 0, &mut v) };
        (st == 0 && v.is_finite()).then_some(v)
    }

    pub fn set_param(&self, id: u32, value: f32) -> bool {
        // SAFETY: the unit is live; the call only takes plain values.
        unsafe { AudioUnitSetParameter(self.unit, id, SCOPE_GLOBAL, 0, value, 0) == 0 }
    }

    /// Tells the unit's parameter listeners that any parameter may have changed, as a view does
    /// after it loads a preset.
    #[cfg(test)]
    pub fn notify_any(&self) {
        let any = AudioUnitParameter { unit: self.unit, id: ANY_PARAMETER, scope: SCOPE_GLOBAL, element: 0 };
        // SAFETY: the unit is live; `any` is a local; no sending listener or object.
        unsafe { AUParameterListenerNotify(ptr::null_mut(), ptr::null_mut(), &any) };
    }

    /// Sets a value the way the unit's view does when the user turns a control: `AUParameterSet`,
    /// which also tells every parameter listener.
    #[cfg(test)]
    pub fn set_as_view(&self, id: u32, value: f32) -> bool {
        let p = AudioUnitParameter { unit: self.unit, id, scope: SCOPE_GLOBAL, element: 0 };
        // SAFETY: the unit is live; `p` is a local; no sending listener or object.
        unsafe { AUParameterSet(ptr::null_mut(), ptr::null_mut(), &p, value, 0) == 0 }
    }

    /// The global-scope parameters.
    pub fn params(&self) -> Vec<RawParam> {
        let mut size = 0u32;
        let mut writable = 0u8;
        // SAFETY: the unit is live; out pointers are locals.
        let st = unsafe { AudioUnitGetPropertyInfo(self.unit, PROP_PARAMETER_LIST, SCOPE_GLOBAL, 0, &mut size, &mut writable) };
        if st != 0 || size == 0 {
            return Vec::new();
        }
        let n = (size as usize / 4).min(MAX_PARAMS);
        let mut ids = vec![0u32; n];
        let mut size = (n * 4) as u32;
        // SAFETY: `ids` has room for `size` bytes.
        let st = unsafe { AudioUnitGetProperty(self.unit, PROP_PARAMETER_LIST, SCOPE_GLOBAL, 0, ids.as_mut_ptr().cast(), &mut size) };
        if st != 0 {
            return Vec::new();
        }
        ids.truncate(size as usize / 4);
        ids.into_iter().filter_map(|id| self.param_info(id)).collect()
    }

    fn param_info(&self, id: u32) -> Option<RawParam> {
        // SAFETY: an all-zero `AudioUnitParameterInfo` is valid (null pointers, zero numbers).
        let mut info: AudioUnitParameterInfo = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<AudioUnitParameterInfo>() as u32;
        // SAFETY: `info` is a local of the documented type and size.
        let st = unsafe {
            AudioUnitGetProperty(self.unit, PROP_PARAMETER_INFO, SCOPE_GLOBAL, id, (&mut info as *mut AudioUnitParameterInfo).cast(), &mut size)
        };
        if st != 0 {
            return None;
        }
        let owned = info.flags & PARAM_FLAG_CF_NAME_RELEASE != 0;
        let mut name = if info.flags & PARAM_FLAG_HAS_CF_NAME != 0 { cf_string(info.cf_name_string) } else { String::new() };
        if name.is_empty() {
            let bytes: Vec<u8> = info.name.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
            name = String::from_utf8_lossy(&bytes).into_owned();
        }
        let custom_unit = if info.unit == UNIT_CUSTOM { cf_string(info.unit_name) } else { String::new() };
        if owned {
            if info.flags & PARAM_FLAG_HAS_CF_NAME != 0 {
                release(info.cf_name_string);
            }
            if info.unit == UNIT_CUSTOM {
                release(info.unit_name);
            }
        }
        let value_strings = if info.unit == UNIT_INDEXED { self.value_strings(id) } else { Vec::new() };
        Some(RawParam {
            id,
            name,
            unit: info.unit,
            custom_unit,
            min: info.min_value,
            max: info.max_value,
            default: info.default_value,
            flags: info.flags,
            value_strings,
        })
    }

    fn value_strings(&self, id: u32) -> Vec<String> {
        let mut arr: CFTypeRef = ptr::null();
        let mut size = std::mem::size_of::<CFTypeRef>() as u32;
        // SAFETY: the property is a `CFArrayRef` (returned +1); `arr` is a local pointer.
        let st = unsafe {
            AudioUnitGetProperty(self.unit, PROP_PARAMETER_VALUE_STRINGS, SCOPE_GLOBAL, id, (&mut arr as *mut CFTypeRef).cast(), &mut size)
        };
        if st != 0 || arr.is_null() {
            return Vec::new();
        }
        // SAFETY: `arr` is a live CFArray we own; indices are below its count.
        let out = unsafe {
            let n = CFArrayGetCount(arr).clamp(0, MAX_VALUE_STRINGS);
            (0..n).map(|i| cf_string(CFArrayGetValueAtIndex(arr, i))).collect()
        };
        release(arr);
        out
    }

    /// The link for state and editor handles.
    pub fn link(&self) -> &Arc<UnitLink> {
        &self.link
    }

    /// Restores a blob from [`UnitLink::save_state`] (hostile input: parsed by CoreFoundation,
    /// must be a dictionary, and the unit validates it), then tells the unit's parameter
    /// listeners (its view) that any parameter may have changed; the editor's own listener is
    /// left out, as a state load is no edit. When the unit refuses the blob,
    /// its previous state is set back: a unit may apply part of a state before refusing the rest.
    pub fn load_state(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() || bytes.len() > MAX_STATE {
            return false;
        }
        // SAFETY: `bytes` is a live slice of `len` bytes (copied by CFDataCreate).
        let data = unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex) };
        if data.is_null() {
            return false;
        }
        let mut err: CFTypeRef = ptr::null();
        // SAFETY: `data` is a live CFData; the format out pointer may be null; `err` is local.
        let plist = unsafe { CFPropertyListCreateWithData(ptr::null(), data, 0, ptr::null_mut(), &mut err) };
        release(err);
        release(data);
        if plist.is_null() {
            return false;
        }
        // SAFETY: `plist` is a live CF object we own.
        let is_dict = unsafe { CFGetTypeID(plist) == CFDictionaryGetTypeID() };
        let st = if is_dict {
            // Under the link's lock: a state handle never reads the state halfway through a load.
            let _g = self.link.lock();
            // SAFETY: the unit is live (disposed only in `Drop`).
            let previous = unsafe { class_info(self.unit) };
            let st = self.set_property(PROP_CLASS_INFO, SCOPE_GLOBAL, 0, &plist);
            if st != 0 && !previous.is_null() {
                let _ = self.set_property(PROP_CLASS_INFO, SCOPE_GLOBAL, 0, &previous);
            }
            release(previous);
            st
        } else {
            -50
        };
        release(plist);
        if st != 0 {
            return false;
        }
        let any = AudioUnitParameter { unit: self.unit, id: ANY_PARAMETER, scope: SCOPE_GLOBAL, element: 0 };
        // Under the lock, so the editor's listener named as the sender is alive.
        let g = self.link.lock();
        let sender = g.listener.as_ref().map_or(ptr::null_mut(), |l| l.listener);
        // SAFETY: the unit is live; `any` is a local; the sending listener is null or live, and
        // the sending object may be null.
        unsafe { AUParameterListenerNotify(sender, ptr::null_mut(), &any) };
        true
    }
}

/// What the link guards.
struct Linked {
    /// `None` once the instance is gone.
    unit: Option<AudioUnit>,
    /// The open editor's window and view (main thread only).
    editor: Option<window::Editor>,
    /// The open editor's parameter listener.
    listener: Option<EditListener>,
    /// Views that outlived their editor; the unit is disposed only once they are gone.
    closed: Vec<window::ClosedView>,
}

// SAFETY: the unit pointer is only used under the link's mutex while the instance keeps the unit
// alive; the editor's AppKit objects are only touched on the main thread (`window` takes a
// `MainThreadMarker` for every call).
unsafe impl Send for Linked {}

/// The link between an instance and its handles (state readers, editors). The unit's state is read
/// and its view driven on the main thread while the instance renders on the audio thread: hosts
/// make property calls from other threads during rendering, and Audio Unit views run on the main
/// thread. The instance closes the editor and clears the link under its lock before it disposes
/// of the unit, so a handle never reaches a dead unit.
pub(crate) struct UnitLink(std::sync::Mutex<Linked>);

impl UnitLink {
    fn lock(&self) -> std::sync::MutexGuard<'_, Linked> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The unit's `ClassInfo` as a binary property list (`None` once the instance is gone).
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let g = self.lock();
        let unit = g.unit?;
        // SAFETY: the unit is live while the link holds it: the instance clears the link under
        // this lock before disposing of it. This may run while the instance renders on the audio
        // thread: Audio Units serve `ClassInfo` from the main thread during rendering (hosts save
        // sessions while playing). State loads take this lock too, so a read never sees one
        // halfway.
        let plist = unsafe { class_info(unit) };
        if plist.is_null() {
            return None;
        }
        let mut err: CFTypeRef = ptr::null();
        // SAFETY: `plist` is a live property list; `err` is a local out pointer.
        let data = unsafe { CFPropertyListCreateData(ptr::null(), plist, CF_PROPERTY_LIST_BINARY_FORMAT, 0, &mut err) };
        release(err);
        release(plist);
        if data.is_null() {
            return None;
        }
        // SAFETY: `data` is a live CFData we own; its byte pointer is valid for its length.
        let out = unsafe {
            let len = CFDataGetLength(data);
            let p = CFDataGetBytePtr(data);
            if len <= 0 || len as usize > MAX_STATE || p.is_null() { None } else { Some(std::slice::from_raw_parts(p, len as usize).to_vec()) }
        };
        release(data);
        out
    }

    /// Opens (or raises) the unit's view in a host window, reporting the view's edits of `ids`
    /// into `edits`. With `show` false the window is never ordered on screen (tests). Main thread
    /// only.
    pub fn open_editor(&self, title: &str, ids: &[u32], edits: &Arc<Edits>, show: bool) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or_else(|| "plugin editors must be opened on the main thread".to_string())?;
        let mut g = self.lock();
        let unit = g.unit.ok_or_else(|| "the plugin is gone".to_string())?;
        if let Some(e) = g.editor.as_mut()
            && !e.closed_by_user(mtm)
        {
            if show {
                e.show(mtm);
            }
            return Ok(());
        }
        // The user closed the window: start over.
        UnitLink::teardown(&mut g, mtm);
        g.editor = Some(window::Editor::open(mtm, unit, title, show)?);
        // SAFETY: the unit is live while the link holds it, and the listener goes (`teardown`)
        // before the instance disposes of the unit.
        g.listener = unsafe { EditListener::new(unit, ids, Arc::clone(edits)) };
        Ok(())
    }

    /// Attaches the editor's parameter listener without a window, as `open_editor` does next to
    /// one (unit tests run off the main thread, where no view can be made).
    #[cfg(test)]
    pub fn listen(&self, ids: &[u32], edits: &Arc<Edits>) {
        let mut g = self.lock();
        if let Some(unit) = g.unit {
            // SAFETY: as in `open_editor`; `kill` drops the listener before the unit is disposed.
            g.listener = unsafe { EditListener::new(unit, ids, Arc::clone(edits)) };
        }
    }

    /// Disposes of the listener, then closes the window and releases the view (remembering it
    /// while it lingers).
    fn teardown(g: &mut Linked, mtm: MainThreadMarker) {
        g.listener = None;
        g.closed.retain(|v| v.alive(mtm));
        if let Some(e) = g.editor.take() {
            g.closed.extend(e.close(mtm));
        }
    }

    /// Closes the editor. Main thread only (no-op elsewhere).
    pub fn close_editor(&self) {
        if let Some(mtm) = MainThreadMarker::new() {
            UnitLink::teardown(&mut self.lock(), mtm);
        }
    }

    pub fn editor_open(&self) -> bool {
        let g = self.lock();
        match (&g.editor, MainThreadMarker::new()) {
            (Some(e), Some(mtm)) => !e.closed_by_user(mtm),
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// Main-thread housekeeping: tears the editor down when the user closed its window, and fits
    /// the window to the view when the view resized itself.
    pub fn editor_idle(&self) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let mut g = self.lock();
        match &g.editor {
            Some(e) if e.closed_by_user(mtm) => UnitLink::teardown(&mut g, mtm),
            Some(e) => e.fit(mtm),
            None => {}
        }
    }

    /// Shows a value set elsewhere (the generic editor, automation) in the open view:
    /// `AUParameterSet` sets it and tells the view's listeners, leaving out the editor's own (it is
    /// no edit). No-op without an open editor.
    pub fn show_param(&self, id: u32, value: f32) {
        if !value.is_finite() || id == ANY_PARAMETER {
            return;
        }
        let g = self.lock();
        let (Some(unit), Some(l)) = (g.unit, &g.listener) else { return };
        let p = AudioUnitParameter { unit, id, scope: SCOPE_GLOBAL, element: 0 };
        // SAFETY: the unit and the listener are live while the link holds them (under its lock);
        // `p` is a local.
        unsafe { AUParameterSet(l.listener, ptr::null_mut(), &p, value, 0) };
    }

    /// The size of the unit's view: the open editor's, or one made in a window that is never
    /// shown and closed at once. A view is never made outside a window: some (Apple's) stop
    /// listening to their unit only when they leave one, and a view released without ever having
    /// one left a listener behind that crashed at the unit's next property change. Main thread
    /// only.
    pub fn editor_size(&self) -> Result<(u32, u32), String> {
        let mtm = MainThreadMarker::new().ok_or_else(|| "plugin views are made on the main thread".to_string())?;
        let mut g = self.lock();
        let unit = g.unit.ok_or_else(|| "the plugin is gone".to_string())?;
        if let Some(e) = &g.editor {
            return Ok(e.size(mtm));
        }
        let e = window::Editor::open(mtm, unit, "", false)?;
        let size = e.size(mtm);
        g.closed.retain(|v| v.alive(mtm));
        g.closed.extend(e.close(mtm));
        Ok(size)
    }

    /// True when the unit publishes a Cocoa view of its own.
    pub fn has_custom_editor(&self) -> bool {
        let g = self.lock();
        let Some(unit) = g.unit else { return false };
        // SAFETY: the unit is live while the link holds it.
        let Some((url, class)) = (unsafe { cocoa_ui(unit) }) else { return false };
        release(url);
        release(class);
        true
    }

    /// Called by the instance before it goes: cuts the handles off and closes the editor (on the
    /// main thread). Returns what must happen on the main thread before the unit may be disposed:
    /// an editor to close (open while the instance goes elsewhere) and views still alive.
    fn kill(&self) -> (Option<window::Editor>, Vec<window::ClosedView>) {
        let mut g = self.lock();
        g.unit = None;
        g.listener = None;
        match MainThreadMarker::new() {
            Some(mtm) => {
                UnitLink::teardown(&mut g, mtm);
                (None, std::mem::take(&mut g.closed))
            }
            // Views are only looked at on the main thread.
            None => (g.editor.take(), std::mem::take(&mut g.closed)),
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let (editor, views) = self.link.kill();
        let remains = Remains { unit: self.unit, input: self.input, time: self.time, initialized: self.initialized };
        if editor.is_none() && views.is_empty() {
            // SAFETY: the handles are cut off and no view of the unit is alive.
            unsafe { remains.dispose() };
        } else {
            // A view still uses the unit: it is disposed on the main thread once the view is gone.
            retire(Retiring { remains, editor, views, tries: 0 });
        }
    }
}

/// A unit's view in SoundCraft's plugin window (`soundcraft_macos::plugin_window`, shared with the
/// VST3 host; it hands the keys the view does not take to SoundCraft's own window). The view is
/// the unit's own Cocoa view (the factory class its `kAudioUnitProperty_CocoaUI` names,
/// conforming to `AUCocoaUIBase`), or Apple's `AUGenericView` when it publishes none or it fails
/// to load. Every function takes a `MainThreadMarker`: AppKit objects are only touched on the main
/// thread.
mod window {
    use super::{AudioUnit, CFTypeRef, cf_string, cocoa_ui, release};
    use objc2::rc::{Allocated, Retained, Weak, autoreleasepool};
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{MainThreadMarker, msg_send, sel};
    use objc2_app_kit::NSView;
    use objc2_foundation::{NSBundle, NSPoint, NSSize, NSString, NSURL};
    use soundcraft_macos::plugin_window::{DEFAULT_CONTENT, PluginWindow};

    /// A view and, for a unit's own view, the factory that made it (kept until the view goes).
    struct View {
        view: Retained<NSView>,
        factory: Option<Retained<AnyObject>>,
    }

    /// The unit's own view, else the generic one.
    fn make_view(_mtm: MainThreadMarker, unit: AudioUnit) -> Result<View, String> {
        // SAFETY: the caller holds the unit (live) under the link's lock.
        if let Some((url, class)) = unsafe { cocoa_ui(unit) } {
            let made = cocoa_view(unit, url, class);
            release(url);
            release(class);
            match made {
                Ok(v) => return Ok(v),
                Err(e) => log::warn!("au: the unit's own view failed ({e}); showing the generic view"),
            }
        }
        generic_view(unit)
    }

    /// Asks the unit's view factory for a view.
    fn cocoa_view(unit: AudioUnit, url: CFTypeRef, class: CFTypeRef) -> Result<View, String> {
        let name = cf_string(class);
        // SAFETY: `url` is a live CFURL (`cocoa_ui` checked its type), toll-free bridged to
        // NSURL, borrowed for this call.
        let url: &NSURL = unsafe { &*url.cast::<NSURL>() };
        let bundle = NSBundle::bundleWithURL(url).ok_or_else(|| "its view bundle cannot be opened".to_string())?;
        let cls: &AnyClass = bundle.classNamed(&NSString::from_str(&name)).ok_or_else(|| format!("no view class `{name}`"))?;
        if !cls.responds_to(sel!(uiViewForAudioUnit:withSize:)) {
            return Err(format!("`{name}` is no AUCocoaUIBase view factory"));
        }
        // SAFETY: `alloc` and `init` of an `NSObject` subclass (every view factory is one).
        let factory: Option<Retained<AnyObject>> = unsafe {
            let a: Allocated<AnyObject> = msg_send![cls, alloc];
            msg_send![a, init]
        };
        let factory = factory.ok_or_else(|| format!("`{name}` did not initialize"))?;
        // SAFETY: the factory implements `uiViewForAudioUnit:withSize:` (checked above); the unit
        // is live; a zero size lets the view choose. The view comes back autoreleased and
        // `msg_send!` retains it.
        let view: Option<Retained<AnyObject>> = unsafe { msg_send![&*factory, uiViewForAudioUnit: unit, withSize: NSSize::new(0.0, 0.0)] };
        let view = view.ok_or_else(|| format!("`{name}` made no view"))?.downcast::<NSView>().map_err(|_| format!("`{name}` made no NSView"))?;
        Ok(View { view, factory: Some(factory) })
    }

    /// Apple's generic view: every parameter (the expert ones too), with the unit's own names.
    fn generic_view(unit: AudioUnit) -> Result<View, String> {
        let cls = AnyClass::get(c"AUGenericView").ok_or_else(|| "CoreAudioKit's generic view is unavailable".to_string())?;
        // SAFETY: `AUGenericView` is an `NSView` subclass whose `initWithAudioUnit:` takes a live
        // unit (the caller holds it).
        let view: Option<Retained<NSView>> = unsafe {
            let a: Allocated<NSView> = msg_send![cls, alloc];
            msg_send![a, initWithAudioUnit: unit]
        };
        let view = view.ok_or_else(|| "the generic view did not initialize".to_string())?;
        if cls.responds_to(sel!(setShowsExpertParameters:)) {
            // SAFETY: a live AUGenericView; the property is a `BOOL`.
            let _: () = unsafe { msg_send![&*view, setShowsExpertParameters: Bool::YES] };
        }
        Ok(View { view, factory: None })
    }

    /// True when a view's frame size is a size (a view may report none).
    fn has_size(s: NSSize) -> bool {
        s.width.is_finite() && s.height.is_finite() && s.width >= 1.0 && s.height >= 1.0
    }

    /// An open editor: the view in its window. Holds the window and one retain each of the view
    /// and the view's factory, released in [`Editor::close`].
    pub struct Editor {
        window: PluginWindow,
        view: *mut NSView,
        /// Null for the generic view.
        factory: *mut AnyObject,
    }

    // SAFETY: the pointers are only dereferenced on the main thread (every method takes a
    // `MainThreadMarker`); the struct merely carries the retains between threads' locks.
    unsafe impl Send for Editor {}

    impl Editor {
        /// Makes the unit's view and puts it in a new host window, ordered on screen when `show`.
        pub fn open(mtm: MainThreadMarker, unit: AudioUnit, title: &str, show: bool) -> Result<Editor, String> {
            autoreleasepool(|_| {
                let View { view, factory } = make_view(mtm, unit)?;
                let mut size = view.frame().size;
                if !has_size(size) {
                    view.setFrameSize(DEFAULT_CONTENT);
                    size = DEFAULT_CONTENT;
                }
                let window = PluginWindow::new(mtm, title, size);
                // The window goes (closed and released) when it is dropped on these early returns.
                let content = window.content_view(mtm).ok_or_else(|| "the editor window has no content view".to_string())?;
                // The window follows the view's size (`fit`), never the other way round.
                content.setAutoresizesSubviews(false);
                view.setFrameOrigin(NSPoint::new(0.0, 0.0));
                content.addSubview(&view);
                let mut e = Editor { window, view: Retained::into_raw(view), factory: factory.map_or(std::ptr::null_mut(), Retained::into_raw) };
                // The view may have sized itself on joining the window.
                e.fit(mtm);
                if show {
                    e.show(mtm);
                }
                Ok(e)
            })
        }

        fn view(&self, _mtm: MainThreadMarker) -> Option<&NSView> {
            // SAFETY: we hold a retain of the view (from `open`) until `close`.
            unsafe { self.view.as_ref() }
        }

        /// The view's size in pixels.
        pub fn size(&self, mtm: MainThreadMarker) -> (u32, u32) {
            let s = self.view(mtm).map_or(NSSize::new(0.0, 0.0), |v| v.frame().size);
            (s.width.max(0.0).round() as u32, s.height.max(0.0).round() as u32)
        }

        /// Orders the window front.
        pub fn show(&mut self, mtm: MainThreadMarker) {
            self.window.show(mtm);
        }

        /// The user closed the window (hidden while the app is inactive is still open).
        pub fn closed_by_user(&self, mtm: MainThreadMarker) -> bool {
            self.window.closed_by_user(mtm)
        }

        /// Fits the window to the view when the view resized itself, keeping the window's top-left
        /// corner where it is.
        pub fn fit(&self, mtm: MainThreadMarker) {
            let Some(v) = self.view(mtm) else { return };
            autoreleasepool(|_| {
                let f = v.frame();
                if !has_size(f.size) {
                    return;
                }
                if f.origin.x != 0.0 || f.origin.y != 0.0 {
                    v.setFrameOrigin(NSPoint::new(0.0, 0.0));
                }
                self.window.fit(mtm, f.size);
            });
        }

        /// Takes the view out of the window and releases it and its factory, then closes and
        /// releases the window, draining the autorelease pool before it returns. A view that is
        /// still alive afterwards (a plugin's timer may hold it for a few more run-loop turns, and
        /// it may still call into the unit meanwhile) is handed back.
        pub fn close(self, mtm: MainThreadMarker) -> Option<ClosedView> {
            let mut view = None;
            let Editor { window, view: raw_view, factory } = self;
            autoreleasepool(|_| {
                // SAFETY: each pointer is null or holds the retain taken in `open`, released here
                // exactly once (`self` is consumed); main thread (the marker).
                unsafe {
                    if let Some(v) = Retained::from_raw(raw_view) {
                        view = Some(ClosedView(Weak::from_retained(&v)));
                        v.removeFromSuperview();
                    }
                    drop(Retained::from_raw(factory));
                }
                window.close(mtm);
            });
            view.filter(|v| v.alive(mtm))
        }
    }

    /// A view that outlived its editor, held weakly until it is gone.
    pub struct ClosedView(Weak<NSView>);

    // SAFETY: the reference is only loaded on the main thread (`alive` takes the marker); a weak
    // reference itself may be dropped on any thread (the runtime's weak table is thread-safe).
    unsafe impl Send for ClosedView {}

    impl ClosedView {
        pub fn alive(&self, _mtm: MainThreadMarker) -> bool {
            self.0.load().is_some()
        }
    }
}

// ---- deferred disposal ------------------------------------------------------------------------

// libdispatch (in libSystem): work handed to the main queue runs on the main thread, from its run
// loop.
unsafe extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_after_f(when: u64, queue: *const c_void, context: *mut c_void, work: unsafe extern "C" fn(*mut c_void));
}

/// `DISPATCH_TIME_NOW`.
const DISPATCH_TIME_NOW: u64 = 0;
/// How often a deferred disposal looks again.
const RETIRE_INTERVAL_NS: i64 = 50_000_000;
/// Looks before a unit whose view never goes is given up (leaked): 10 s.
const RETIRE_TRIES: u32 = 200;

/// Units waiting for their views to go before they are disposed (see [`units_awaiting_disposal`]).
static AWAITING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Units whose disposal waits for the main thread (an editor still to close, or a view that
/// outlived its editor). Diagnostics and tests.
pub fn units_awaiting_disposal() -> usize {
    AWAITING.load(Ordering::Relaxed)
}

/// What an instance leaves to dispose of: the unit and what its callbacks read.
struct Remains {
    unit: AudioUnit,
    input: *mut InputState,
    time: *mut HostTime,
    initialized: bool,
}

impl Remains {
    /// Uninitializes and disposes of the unit, then frees what its callbacks read.
    ///
    /// # Safety
    /// Nothing uses the unit any more: the instance is gone and no view of it is alive.
    unsafe fn dispose(self) {
        // SAFETY: the unit is live (disposed only here); after disposal nothing can call the
        // input callback or the host callbacks, so the leaked input state and host time can be
        // freed.
        unsafe {
            if self.initialized {
                AudioUnitUninitialize(self.unit);
            }
            AudioComponentInstanceDispose(self.unit);
            drop(Box::from_raw(self.input));
            drop(Box::from_raw(self.time));
        }
    }
}

/// A unit whose disposal waits on the main thread.
struct Retiring {
    remains: Remains,
    /// An editor that was open when the instance went off the main thread.
    editor: Option<window::Editor>,
    views: Vec<window::ClosedView>,
    tries: u32,
}

/// Hands `r` to the main queue, which closes its editor and disposes of its unit once its views
/// are gone.
fn retire(r: Retiring) {
    AWAITING.fetch_add(1, Ordering::Relaxed);
    schedule_retire(Box::new(r), 0);
}

fn schedule_retire(r: Box<Retiring>, delay_ns: i64) {
    // SAFETY: the main queue is a process-wide static; `dispatch_after_f` takes ownership of the
    // context, which `retire_on_main` takes back exactly once.
    unsafe {
        let queue = (&raw const _dispatch_main_q).cast::<c_void>();
        dispatch_after_f(dispatch_time(DISPATCH_TIME_NOW, delay_ns), queue, Box::into_raw(r).cast(), retire_on_main);
    }
}

unsafe extern "C" fn retire_on_main(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: the box `schedule_retire` handed over, taken back once.
    let mut r = unsafe { Box::from_raw(context.cast::<Retiring>()) };
    // The main queue runs on the main thread.
    let Some(mtm) = MainThreadMarker::new() else {
        schedule_retire(r, RETIRE_INTERVAL_NS);
        return;
    };
    if let Some(e) = r.editor.take() {
        r.views.extend(e.close(mtm));
    }
    r.views.retain(|v| v.alive(mtm));
    if r.views.is_empty() {
        AWAITING.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: the instance is gone and its views with it.
        unsafe { r.remains.dispose() };
    } else if r.tries >= RETIRE_TRIES {
        AWAITING.fetch_sub(1, Ordering::Relaxed);
        log::warn!("au: a plugin view outlived its plugin by 10 s; the plugin is leaked rather than disposed under it");
    } else {
        r.tries += 1;
        schedule_retire(r, RETIRE_INTERVAL_NS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts notifications into the `AtomicU32` behind `user`.
    unsafe extern "C" fn count(user: *mut c_void, _object: *mut c_void, _parameter: *const AudioUnitParameter, _value: f32) {
        // SAFETY: `user` is the test's counter, alive until the listener is disposed.
        if let Some(n) = unsafe { (user as *const AtomicU32).as_ref() } {
            n.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_state_load_notifies_parameter_listeners() {
        let code = |s: &[u8; 4]| u32::from_be_bytes(*s);
        // AULowpass ships with macOS; skip where it is missing.
        let Ok(inst) = Instance::new(code(b"aufx"), code(b"lpas"), code(b"appl"), true) else { return };
        let state = inst.link().save_state().expect("ClassInfo");
        let calls = AtomicU32::new(0);
        let mut listener: *mut c_void = ptr::null_mut();
        // Listens to parameter 0 only: the load's "any parameter" notice must reach it.
        let first = AudioUnitParameter { unit: inst.unit, id: 0, scope: SCOPE_GLOBAL, element: 0 };
        // SAFETY: `calls` outlives the listener (disposed below); without a run loop the listener
        // is called on the notifying thread; the out pointer and the parameter are locals.
        unsafe {
            assert_eq!(AUListenerCreate(count, ptr::from_ref(&calls).cast_mut().cast(), ptr::null(), ptr::null(), 0.0, &mut listener), 0);
            assert_eq!(AUListenerAddParameter(listener, ptr::null_mut(), &first), 0);
        }
        assert!(!inst.load_state(b"not a property list"));
        assert!(inst.load_state(&state));
        for _ in 0..200 {
            if calls.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1, "one notice, for the accepted state only");
        // SAFETY: the listener created above, disposed once, before `calls` goes away.
        unsafe { AUListenerDispose(listener) };
    }

    #[test]
    fn host_callbacks_report_the_transport() {
        let time = HostTime::new();
        time.set(&Transport { playing: true, tempo: 120.0, numerator: 3, denominator: 4, samples: 48_000, quarters: 2.25, ..Transport::default() });
        let user = ptr::from_ref(&time).cast_mut().cast::<c_void>();
        let (mut beat, mut tempo) = (0.0, 0.0);
        // SAFETY: `user` points at a live `HostTime`; the out-pointers are locals.
        assert_eq!(unsafe { beat_and_tempo(user, &mut beat, &mut tempo) }, 0);
        assert_eq!((beat, tempo), (2.25, 120.0));
        let (mut next, mut num, mut den, mut down) = (0u32, 0f32, 0u32, -1.0);
        // SAFETY: as above.
        assert_eq!(unsafe { musical_time(user, &mut next, &mut num, &mut den, &mut down) }, 0);
        assert_eq!((next, num, den, down), (18_000, 3.0, 4, 0.0));
        let (mut playing, mut recording, mut changed, mut sample) = (0u8, 1u8, 0u8, 0.0);
        let (no_flag, no_beat) = (ptr::null_mut::<u8>(), ptr::null_mut::<f64>());
        // SAFETY: as above; the unit may pass null for what it doesn't want.
        let st = unsafe { transport_state2(user, &mut playing, &mut recording, &mut changed, &mut sample, no_flag, no_beat, no_beat) };
        assert_eq!((st, playing, recording, changed, sample), (0, 1, 0, 1, 48_000.0));
        // SAFETY: as above.
        unsafe { transport_state(user, &mut playing, &mut changed, no_beat, no_flag, no_beat, no_beat) };
        assert_eq!(changed, 0, "a change is reported once");
        // SAFETY: a null user pointer is refused before anything is read.
        assert_eq!(unsafe { beat_and_tempo(ptr::null_mut(), &mut beat, &mut tempo) }, -50);
    }

    #[test]
    fn struct_layouts_match_the_c_headers() {
        assert_eq!(std::mem::size_of::<AudioComponentDescription>(), 20);
        assert_eq!(std::mem::size_of::<AudioStreamBasicDescription>(), 40);
        assert_eq!(std::mem::size_of::<SmpteTime>(), 24);
        assert_eq!(std::mem::size_of::<AudioTimeStamp>(), 64);
        assert_eq!(std::mem::size_of::<AudioBuffer>(), 16);
        assert_eq!(std::mem::offset_of!(AudioBufferList, buffers), 8);
        assert_eq!(std::mem::offset_of!(AudioBufferListHeader, first), 8);
        assert_eq!(std::mem::size_of::<AuRenderCallbackStruct>(), 16);
        assert_eq!(std::mem::size_of::<HostCallbackInfo>(), 40);
        assert_eq!(std::mem::offset_of!(AudioUnitParameter, id), 8);
        assert_eq!(std::mem::offset_of!(AudioUnitParameter, element), 16);
        assert_eq!(std::mem::size_of::<AudioUnitParameter>(), 24);
        assert_eq!(std::mem::offset_of!(AudioUnitParameterInfo, unit_name), 56);
        assert_eq!(std::mem::offset_of!(AudioUnitParameterInfo, cf_name_string), 72);
        assert_eq!(std::mem::offset_of!(AudioUnitParameterInfo, flags), 96);
        assert_eq!(std::mem::size_of::<AudioUnitParameterInfo>(), 104);
    }
}
