//! Minimal LV2 host built on liblilv, loaded at runtime with `dlopen` so the
//! application builds without lilv development headers.
//!
//! The discovery side (`Lv2Host`) lives on the UI thread. Instantiated plugins
//! (`PluginRuntime`) are created on the UI thread, sent to the audio thread for
//! processing, and sent back to the UI thread to be freed.
//!
//! An [`Lv2Link`] stays on the UI thread: it lets the plugin's own GUI (see
//! `lv2ui.rs`) reach the running instance and send it messages, and it is used
//! to save the plugin's internal state.

use crate::device::{Device, DeviceCategory, DeviceInfo, DeviceShared, ParamInfo, ProcessContext};
use libloading::Library;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub const MAX_BLOCK: usize = 8192;
const ATOM_BUF_BYTES: usize = 16 * 1024;
/// Capacity of the GUI → audio thread message queue (bytes).
const UI_QUEUE_BYTES: usize = 64 * 1024;

type P = *mut c_void;

#[repr(C)]
pub(crate) struct Lv2Feature {
    pub uri: *const c_char,
    pub data: *mut c_void,
}

#[repr(C)]
struct Lv2Descriptor {
    uri: *const c_char,
    instantiate: *const c_void,
    connect_port: unsafe extern "C" fn(P, u32, P),
    activate: Option<unsafe extern "C" fn(P)>,
    run: unsafe extern "C" fn(P, u32),
    deactivate: Option<unsafe extern "C" fn(P)>,
    cleanup: *const c_void,
    extension_data: *const c_void,
}

/// Layout of lilv's `LilvInstanceImpl` (public in lilv.h).
#[repr(C)]
struct LilvInstance {
    descriptor: *const Lv2Descriptor,
    handle: P,
    pimpl: P,
}

#[repr(C)]
pub(crate) struct UridMapFeature {
    pub handle: P,
    pub map: unsafe extern "C" fn(P, *const c_char) -> u32,
}

#[repr(C)]
pub(crate) struct UridUnmapFeature {
    pub handle: P,
    pub unmap: unsafe extern "C" fn(P, u32) -> *const c_char,
}

#[repr(C)]
pub(crate) struct Lv2Option {
    pub context: u32,
    pub subject: u32,
    pub key: u32,
    pub size: u32,
    pub type_: u32,
    pub value: *const c_void,
}

macro_rules! lilv_api {
    ($($field:ident : fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        struct Api {
            _lib: Library,
            $($field: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }
        impl Api {
            fn load() -> Result<Self, String> {
                let lib = unsafe { Library::new("liblilv-0.so.0") }
                    .map_err(|e| format!("could not load liblilv-0.so.0: {e}"))?;
                unsafe {
                    Ok(Api {
                        $($field: *lib
                            .get(concat!("lilv_", stringify!($field), "\0").as_bytes())
                            .map_err(|e| format!("lilv symbol {}: {e}", stringify!($field)))?,)*
                        _lib: lib,
                    })
                }
            }
        }
    };
}

lilv_api! {
    world_new: fn() -> P;
    world_load_all: fn(P);
    world_get_all_plugins: fn(P) -> P;
    plugins_begin: fn(P) -> P;
    plugins_is_end: fn(P, P) -> bool;
    plugins_next: fn(P, P) -> P;
    plugins_get: fn(P, P) -> P;
    new_uri: fn(P, *const c_char) -> P;
    node_free: fn(P);
    node_as_string: fn(P) -> *const c_char;
    node_as_float: fn(P) -> f32;
    nodes_begin: fn(P) -> P;
    nodes_is_end: fn(P, P) -> bool;
    nodes_next: fn(P, P) -> P;
    nodes_get: fn(P, P) -> P;
    nodes_free: fn(P);
    plugin_get_uri: fn(P) -> P;
    plugin_get_name: fn(P) -> P;
    plugin_get_class: fn(P) -> P;
    plugin_class_get_label: fn(P) -> P;
    plugin_get_num_ports: fn(P) -> u32;
    plugin_get_port_by_index: fn(P, u32) -> P;
    plugin_get_port_ranges_float: fn(P, *mut f32, *mut f32, *mut f32);
    plugin_get_required_features: fn(P) -> P;
    plugin_instantiate: fn(P, f64, *const *const Lv2Feature) -> *mut LilvInstance;
    instance_free: fn(*mut LilvInstance);
    port_is_a: fn(P, P, P) -> bool;
    port_has_property: fn(P, P, P) -> bool;
    port_get_name: fn(P, P) -> P;
    port_get_symbol: fn(P, P) -> P;
    port_get_scale_points: fn(P, P) -> P;
    port_supports_event: fn(P, P, P) -> bool;
    port_get: fn(P, P, P) -> P;
    scale_points_begin: fn(P) -> P;
    scale_points_is_end: fn(P, P) -> bool;
    scale_points_next: fn(P, P) -> P;
    scale_points_get: fn(P, P) -> P;
    scale_points_free: fn(P);
    scale_point_get_label: fn(P) -> P;
    scale_point_get_value: fn(P) -> P;
    plugin_get_uis: fn(P) -> P;
    uis_begin: fn(P) -> P;
    uis_is_end: fn(P, P) -> bool;
    uis_next: fn(P, P) -> P;
    uis_get: fn(P, P) -> P;
    uis_free: fn(P);
    ui_get_uri: fn(P) -> P;
    ui_is_a: fn(P, P) -> bool;
    ui_get_binary_uri: fn(P) -> P;
    ui_get_bundle_uri: fn(P) -> P;
    node_as_uri: fn(P) -> *const c_char;
    file_uri_parse: fn(*const c_char, *mut *mut c_char) -> *mut c_char;
    free: fn(P);
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(Api::load).as_ref().map_err(|e| e.clone())
}

// ---------------------------------------------------------------------------
// URID map (process global, shared by all plugin instances)

#[derive(Default)]
struct UridTable {
    ids: HashMap<String, u32>,
    uris: Vec<CString>,
}

static URIDS: OnceLock<Mutex<UridTable>> = OnceLock::new();

pub fn urid(uri: &str) -> u32 {
    let mut t = URIDS.get_or_init(Default::default).lock().unwrap();
    if let Some(&id) = t.ids.get(uri) {
        return id;
    }
    t.uris.push(CString::new(uri).unwrap_or_default());
    let id = t.uris.len() as u32;
    t.ids.insert(uri.to_owned(), id);
    id
}

pub(crate) unsafe extern "C" fn urid_map_cb(_: P, uri: *const c_char) -> u32 {
    if uri.is_null() {
        return 0;
    }
    urid(&unsafe { CStr::from_ptr(uri) }.to_string_lossy())
}

pub(crate) unsafe extern "C" fn urid_unmap_cb(_: P, id: u32) -> *const c_char {
    let t = URIDS.get_or_init(Default::default).lock().unwrap();
    match t.uris.get((id as usize).wrapping_sub(1)) {
        // CString heap data stays put even if the Vec reallocates.
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    }
}

// ---------------------------------------------------------------------------
// URIs

const LV2_CORE: &str = "http://lv2plug.in/ns/lv2core#";
pub(crate) const URI_MAP: &CStr = c"http://lv2plug.in/ns/ext/urid#map";
pub(crate) const URI_UNMAP: &CStr = c"http://lv2plug.in/ns/ext/urid#unmap";
pub(crate) const URI_OPTIONS: &CStr = c"http://lv2plug.in/ns/ext/options#options";
const URI_BOUNDED: &CStr = c"http://lv2plug.in/ns/ext/buf-size#boundedBlockLength";

const SUPPORTED_FEATURES: &[&str] = &[
    "http://lv2plug.in/ns/ext/urid#map",
    "http://lv2plug.in/ns/ext/urid#unmap",
    "http://lv2plug.in/ns/ext/options#options",
    "http://lv2plug.in/ns/ext/buf-size#boundedBlockLength",
    "http://lv2plug.in/ns/lv2core#isLive",
    "http://lv2plug.in/ns/lv2core#hardRTCapable",
    "http://lv2plug.in/ns/lv2core#inPlaceBroken",
    "http://lv2plug.in/ns/ext/worker#schedule",
];

// ---------------------------------------------------------------------------
// Plugin catalogue

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PortKind {
    Audio,
    Control,
    Atom,
    Cv,
    Other,
}

/// Which stereo side an audio port belongs to (from `lv2:designation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Center,
    Left,
    Right,
}

#[derive(Clone, Debug)]
struct PortInfo {
    index: u32,
    kind: PortKind,
    is_input: bool,
    side: Side,
    /// Atom input that accepts MIDI events.
    midi: bool,
    param: ParamInfo,
}

/// A plugin GUI that can be embedded in an X11 window.
#[derive(Clone, Debug)]
pub struct UiInfo {
    pub uri: String,
    pub binary: PathBuf,
    /// Bundle directory, with a trailing slash as LV2 expects.
    pub bundle: String,
}

pub struct PluginInfo {
    pub info: DeviceInfo,
    pub uri: String,
    pub ui: Option<UiInfo>,
    ports: Vec<PortInfo>,
    audio_in: usize,
    audio_out: usize,
    plugin: P,
}

impl PluginInfo {
    /// LV2 port indices of the control inputs, in parameter order.
    pub fn control_in_ports(&self) -> Vec<u32> {
        self.ports.iter().filter(|p| p.kind == PortKind::Control && p.is_input).map(|p| p.index).collect()
    }
    /// Stereo side of each audio output, in output order.
    fn out_sides(&self) -> Vec<Side> {
        self.ports.iter().filter(|p| p.kind == PortKind::Audio && !p.is_input).map(|p| p.side).collect()
    }
    /// LV2 port indices of the control outputs, in output order.
    pub fn control_out_ports(&self) -> Vec<u32> {
        self.ports.iter().filter(|p| p.kind == PortKind::Control && !p.is_input).map(|p| p.index).collect()
    }
}

/// Convert a `file://` URI node to a local path.
unsafe fn file_path(node: P) -> Option<String> {
    let api = api().ok()?;
    if node.is_null() {
        return None;
    }
    unsafe {
        let raw = (api.file_uri_parse)((api.node_as_uri)(node), std::ptr::null_mut());
        if raw.is_null() {
            return None;
        }
        let s = CStr::from_ptr(raw).to_string_lossy().into_owned();
        (api.free)(raw as P);
        Some(s)
    }
}

pub struct Lv2Host {
    pub plugins: Vec<PluginInfo>,
    pub error: Option<String>,
    _world: P,
}

unsafe fn node_str(n: P) -> String {
    let api = api().unwrap();
    if n.is_null() {
        return String::new();
    }
    let s = unsafe { (api.node_as_string)(n) };
    if s.is_null() { String::new() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() }
}

impl Lv2Host {
    pub fn new(sample_rate: f32) -> Self {
        match api() {
            Ok(api) => unsafe { Self::load(api, sample_rate) },
            Err(e) => Lv2Host { plugins: vec![], error: Some(e), _world: std::ptr::null_mut() },
        }
    }

    unsafe fn load(api: &Api, sample_rate: f32) -> Self {
        unsafe {
            let world = (api.world_new)();
            (api.world_load_all)(world);
            let uri = |s: &str| {
                let c = CString::new(s).unwrap();
                (api.new_uri)(world, c.as_ptr())
            };
            let core = |s: &str| uri(&format!("{LV2_CORE}{s}"));
            let n_input = core("InputPort");
            let n_audio = core("AudioPort");
            let n_control = core("ControlPort");
            let n_cv = core("CVPort");
            let n_atom = uri("http://lv2plug.in/ns/ext/atom#AtomPort");
            let n_toggled = core("toggled");
            let n_integer = core("integer");
            let n_enum = core("enumeration");
            let n_srate = core("sampleRate");
            let n_log = uri("http://lv2plug.in/ns/ext/port-props#logarithmic");
            let n_midi = uri("http://lv2plug.in/ns/ext/midi#MidiEvent");
            let n_x11ui = uri("http://lv2plug.in/ns/extensions/ui#X11UI");
            let n_designation = core("designation");

            let mut plugins = Vec::new();
            let all = (api.world_get_all_plugins)(world);
            let mut it = (api.plugins_begin)(all);
            while !(api.plugins_is_end)(all, it) {
                let plugin = (api.plugins_get)(all, it);
                it = (api.plugins_next)(all, it);

                let uri_s = node_str((api.plugin_get_uri)(plugin));
                let name_n = (api.plugin_get_name)(plugin);
                let name = node_str(name_n);
                (api.node_free)(name_n);
                let class = node_str((api.plugin_class_get_label)((api.plugin_get_class)(plugin)));

                let mut unsupported = None;
                let req = (api.plugin_get_required_features)(plugin);
                if !req.is_null() {
                    let mut i = (api.nodes_begin)(req);
                    while !(api.nodes_is_end)(req, i) {
                        let f = node_str((api.nodes_get)(req, i));
                        if !SUPPORTED_FEATURES.contains(&f.as_str()) {
                            unsupported = Some(format!("requires {f}"));
                        }
                        i = (api.nodes_next)(req, i);
                    }
                    (api.nodes_free)(req);
                }

                let n = (api.plugin_get_num_ports)(plugin) as usize;
                let (mut mins, mut maxs, mut defs) = (vec![f32::NAN; n], vec![f32::NAN; n], vec![f32::NAN; n]);
                (api.plugin_get_port_ranges_float)(plugin, mins.as_mut_ptr(), maxs.as_mut_ptr(), defs.as_mut_ptr());

                let mut ports = Vec::with_capacity(n);
                for idx in 0..n {
                    let port = (api.plugin_get_port_by_index)(plugin, idx as u32);
                    let is = |c: P| (api.port_is_a)(plugin, port, c);
                    let has = |c: P| (api.port_has_property)(plugin, port, c);
                    let kind = if is(n_audio) {
                        PortKind::Audio
                    } else if is(n_control) {
                        PortKind::Control
                    } else if is(n_atom) {
                        PortKind::Atom
                    } else if is(n_cv) {
                        PortKind::Cv
                    } else {
                        PortKind::Other
                    };
                    let pname_n = (api.port_get_name)(plugin, port);
                    let pname = node_str(pname_n);
                    (api.node_free)(pname_n);
                    let symbol = node_str((api.port_get_symbol)(plugin, port));

                    let scale = if has(n_srate) { sample_rate } else { 1.0 };
                    let min = if mins[idx].is_nan() { 0.0 } else { mins[idx] * scale };
                    let mut max = if maxs[idx].is_nan() { 1.0 } else { maxs[idx] * scale };
                    if max <= min {
                        max = min + 1.0;
                    }
                    let default = if defs[idx].is_nan() { min } else { (defs[idx] * scale).clamp(min, max) };
                    // Log scaling only makes sense on a strictly positive range.
                    let logarithmic = has(n_log) && min > 0.0;

                    let mut scale_points = Vec::new();
                    if kind == PortKind::Control && has(n_enum) {
                        let sps = (api.port_get_scale_points)(plugin, port);
                        if !sps.is_null() {
                            let mut i = (api.scale_points_begin)(sps);
                            while !(api.scale_points_is_end)(sps, i) {
                                let sp = (api.scale_points_get)(sps, i);
                                scale_points.push((
                                    node_str((api.scale_point_get_label)(sp)),
                                    (api.node_as_float)((api.scale_point_get_value)(sp)),
                                ));
                                i = (api.scale_points_next)(sps, i);
                            }
                            (api.scale_points_free)(sps);
                            scale_points.sort_by(|a, b| a.1.total_cmp(&b.1));
                        }
                    }

                    if kind == PortKind::Other && unsupported.is_none() {
                        unsupported = Some(format!("unknown port type on '{symbol}'"));
                    }

                    let is_input = is(n_input);
                    let side = if kind == PortKind::Audio {
                        let d = (api.port_get)(plugin, port, n_designation);
                        let name = node_str(d);
                        if !d.is_null() {
                            (api.node_free)(d);
                        }
                        if name.ends_with("#left") {
                            Side::Left
                        } else if name.ends_with("#right") {
                            Side::Right
                        } else {
                            Side::Center
                        }
                    } else {
                        Side::Center
                    };
                    ports.push(PortInfo {
                        index: idx as u32,
                        kind,
                        is_input,
                        side,
                        midi: kind == PortKind::Atom && is_input && (api.port_supports_event)(plugin, port, n_midi),
                        param: ParamInfo {
                            symbol,
                            name: pname,
                            min,
                            max,
                            default,
                            toggled: has(n_toggled),
                            integer: has(n_integer),
                            logarithmic,
                            scale_points,
                        },
                    });
                }
                let audio_in = ports.iter().filter(|p| p.kind == PortKind::Audio && p.is_input).count();
                let audio_out = ports.iter().filter(|p| p.kind == PortKind::Audio && !p.is_input).count();
                let has_midi = ports.iter().any(|p| p.midi);
                if audio_out == 0 && unsupported.is_none() {
                    unsupported = Some("no audio outputs (MIDI-only plugins are not supported)".into());
                }
                let category = if has_midi && (audio_in == 0 || class.contains("Instrument")) {
                    DeviceCategory::Instrument
                } else {
                    DeviceCategory::Effect
                };
                let control = |input: bool| -> Vec<ParamInfo> {
                    ports.iter().filter(|p| p.kind == PortKind::Control && p.is_input == input).map(|p| p.param.clone()).collect()
                };
                // An embeddable X11 GUI, if the plugin ships one.
                let mut ui = None;
                let uis = (api.plugin_get_uis)(plugin);
                if !uis.is_null() {
                    let mut i = (api.uis_begin)(uis);
                    while !(api.uis_is_end)(uis, i) {
                        let u = (api.uis_get)(uis, i);
                        i = (api.uis_next)(uis, i);
                        if ui.is_none()
                            && (api.ui_is_a)(u, n_x11ui)
                            && let (Some(binary), Some(bundle)) = (file_path((api.ui_get_binary_uri)(u)), file_path((api.ui_get_bundle_uri)(u)))
                        {
                            let bundle = if bundle.ends_with('/') { bundle } else { format!("{bundle}/") };
                            ui = Some(UiInfo { uri: node_str((api.ui_get_uri)(u)), binary: binary.into(), bundle });
                        }
                    }
                    (api.uis_free)(uis);
                }
                let info = DeviceInfo {
                    id: format!("lv2:{uri_s}"),
                    name,
                    category,
                    class,
                    params: control(true),
                    outputs: control(false),
                    unsupported,
                    has_gui: ui.is_some(),
                };
                plugins.push(PluginInfo { info, uri: uri_s, ui, ports, audio_in, audio_out, plugin });
            }
            for n in [n_input, n_audio, n_control, n_cv, n_atom, n_toggled, n_integer, n_enum, n_srate, n_log, n_midi, n_x11ui, n_designation] {
                (api.node_free)(n);
            }
            plugins.sort_by_key(|p| p.info.name.to_lowercase());
            Lv2Host { plugins, error: None, _world: world }
        }
    }

    pub fn find(&self, uri: &str) -> Option<usize> {
        self.plugins.iter().position(|p| p.info.id.strip_prefix("lv2:") == Some(uri))
    }

    /// Instantiate the plugin at `index` for a stereo track. Mono effects are
    /// instantiated twice ("dual mono") so both channels are processed.
    /// `state` (saved by [`save_state`]) is restored before activation.
    pub fn instantiate(
        &self,
        index: usize,
        sample_rate: f64,
        shared: Arc<DeviceShared>,
        state: Option<&[StateEntry]>,
    ) -> Result<(PluginRuntime, Arc<Lv2Link>), String> {
        let api = api()?;
        let info = &self.plugins[index];
        if let Some(r) = &info.info.unsupported {
            return Err(format!("{}: {r}", info.info.name));
        }
        let copies = if info.audio_in == 1 && info.audio_out == 1 { 2 } else { 1 };
        let mut insts = Vec::new();
        for _ in 0..copies {
            // Each instance gets its own worker (and so its own feature array).
            let worker = Worker::new();
            let features = FeatureSet::new(sample_rate as f32, Some(unsafe { &mut (*worker).schedule as *mut WorkerSchedule }));
            let raw = unsafe { (api.plugin_instantiate)(info.plugin, sample_rate, features.ptrs.as_ptr()) };
            if raw.is_null() {
                unsafe { drop(Box::from_raw(worker)) };
                return Err(format!("{}: instantiation failed", info.info.name));
            }
            let mut inst = unsafe { Inst::new(raw, info, features) };
            unsafe { (*worker).start(inst.handle, inst.desc) };
            inst.worker = worker;
            insts.push(inst);
        }
        if let Some(entries) = state {
            for inst in &insts {
                unsafe { restore_state(inst.desc, inst.handle, entries, &inst.features) };
            }
        }
        for inst in &insts {
            if let Some(activate) = unsafe { (*inst.desc).activate } {
                unsafe { activate(inst.handle) };
            }
        }
        let (ui_tx, ui_rx) = rtrb::RingBuffer::new(UI_QUEUE_BYTES);
        let (dsp_tx, dsp_rx) = rtrb::RingBuffer::new(UI_QUEUE_BYTES);
        let gui_open = Arc::new(AtomicBool::new(false));
        let link = Arc::new(Lv2Link {
            plugin_uri: info.uri.clone(),
            ui: info.ui.clone(),
            handle: insts[0].handle as usize,
            desc: insts[0].desc as usize,
            control_in_ports: info.control_in_ports(),
            control_out_ports: info.control_out_ports(),
            ui_tx: Mutex::new(ui_tx),
            dsp_rx: Mutex::new(dsp_rx),
            gui_open: gui_open.clone(),
        });
        let runtime = PluginRuntime {
            insts,
            shared,
            seq_urid: urid("http://lv2plug.in/ns/ext/atom#Sequence"),
            midi_urid: urid("http://lv2plug.in/ns/ext/midi#MidiEvent"),
            chunk_urid: urid("http://lv2plug.in/ns/ext/atom#Chunk"),
            ui_rx,
            dsp_tx,
            gui_open,
            out_sides: info.out_sides(),
            scratch: vec![0u8; ATOM_BUF_BYTES].into_boxed_slice(),
        };
        Ok((runtime, link))
    }
}

// ---------------------------------------------------------------------------
// Link to a running instance (GUI hosting and state)

/// UI-thread handle to a running plugin instance.
///
/// The raw pointers stay valid as long as the [`PluginRuntime`] is alive; the
/// app closes any GUI using this link before the runtime can be freed.
pub struct Lv2Link {
    pub plugin_uri: String,
    pub ui: Option<UiInfo>,
    /// `LV2_Handle` of the (first) instance, for `instance-access`.
    pub handle: usize,
    desc: usize,
    pub control_in_ports: Vec<u32>,
    pub control_out_ports: Vec<u32>,
    /// GUI → audio thread messages: [port u32][size u32][LV2_Atom bytes].
    ui_tx: Mutex<rtrb::Producer<u8>>,
    /// Audio thread → GUI: events from the plugin's atom outputs, same framing.
    dsp_rx: Mutex<rtrb::Consumer<u8>>,
    /// Set while a GUI is open; the audio thread only forwards events then.
    pub gui_open: Arc<AtomicBool>,
}

impl Lv2Link {
    /// Take the next plugin → GUI event: (atom output port, LV2_Atom bytes).
    pub fn next_dsp_event(&self, buf: &mut Vec<u8>) -> Option<u32> {
        let mut rx = self.dsp_rx.lock().ok()?;
        let avail = rx.slots();
        if avail < 8 {
            return None;
        }
        let mut h = [0u8; 8];
        {
            let head = rx.read_chunk(8).ok()?;
            let (a, b) = head.as_slices();
            for (d, s) in h.iter_mut().zip(a.iter().chain(b)) {
                *d = *s;
            }
        }
        let port = u32::from_ne_bytes(h[0..4].try_into().unwrap());
        let len = u32::from_ne_bytes(h[4..8].try_into().unwrap()) as usize;
        if avail < 8 + len {
            return None;
        }
        let chunk = rx.read_chunk(8 + len).ok()?;
        let (a, b) = chunk.as_slices();
        buf.clear();
        buf.extend(a.iter().chain(b).skip(8));
        chunk.commit_all();
        Some(port)
    }

    /// Queue an atom (header + body, as given by a GUI's write function) for
    /// the plugin's atom input `port`.
    pub fn send_atom(&self, port: u32, atom: &[u8]) -> bool {
        let Ok(mut tx) = self.ui_tx.lock() else { return false };
        let need = 8 + atom.len();
        if atom.len() < 8 || tx.slots() < need {
            return false;
        }
        let Ok(mut chunk) = tx.write_chunk_uninit(need) else { return false };
        let msg = port.to_ne_bytes().into_iter().chain((atom.len() as u32).to_ne_bytes()).chain(atom.iter().copied());
        let (a, b) = chunk.as_mut_slices();
        for (dst, v) in a.iter_mut().chain(b.iter_mut()).zip(msg) {
            dst.write(v);
        }
        unsafe { chunk.commit_all() };
        true
    }
}

/// One property of a plugin's saved state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateEntry {
    pub key: String,
    #[serde(rename = "type")]
    pub type_uri: String,
    pub flags: u32,
    #[serde(with = "base64_bytes")]
    pub value: Vec<u8>,
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        STANDARD.decode(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

const URI_STATE_INTERFACE: &CStr = c"http://lv2plug.in/ns/ext/state#interface";
type StoreFn = unsafe extern "C" fn(P, u32, *const c_void, usize, u32, u32) -> u32;
type RetrieveFn = unsafe extern "C" fn(P, u32, *mut usize, *mut u32, *mut u32) -> *const c_void;

#[repr(C)]
struct StateInterface {
    save: unsafe extern "C" fn(P, StoreFn, P, u32, *const *const Lv2Feature) -> u32,
    restore: unsafe extern "C" fn(P, RetrieveFn, P, u32, *const *const Lv2Feature) -> u32,
}

unsafe fn state_interface(desc: *const Lv2Descriptor) -> Option<&'static StateInterface> {
    unsafe {
        if (*desc).extension_data.is_null() {
            return None;
        }
        let ext: unsafe extern "C" fn(*const c_char) -> *const c_void = std::mem::transmute((*desc).extension_data);
        (ext(URI_STATE_INTERFACE.as_ptr()) as *const StateInterface).as_ref()
    }
}

fn unmap(id: u32) -> String {
    let t = URIDS.get_or_init(Default::default).lock().unwrap();
    t.uris.get((id as usize).wrapping_sub(1)).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()
}

unsafe extern "C" fn store_cb(handle: P, key: u32, value: *const c_void, size: usize, type_: u32, flags: u32) -> u32 {
    let out = unsafe { &mut *(handle as *mut Vec<StateEntry>) };
    let value = if value.is_null() { vec![] } else { unsafe { std::slice::from_raw_parts(value as *const u8, size) }.to_vec() };
    out.push(StateEntry { key: unmap(key), type_uri: unmap(type_), flags, value });
    0
}

unsafe extern "C" fn retrieve_cb(handle: P, key: u32, size: *mut usize, type_: *mut u32, flags: *mut u32) -> *const c_void {
    let entries = unsafe { &*(handle as *const Vec<(u32, u32, u32, &[u8])>) };
    match entries.iter().find(|e| e.0 == key) {
        Some(&(_, t, f, v)) => unsafe {
            if !size.is_null() {
                *size = v.len();
            }
            if !type_.is_null() {
                *type_ = t;
            }
            if !flags.is_null() {
                *flags = f;
            }
            v.as_ptr() as *const c_void
        },
        None => std::ptr::null(),
    }
}

/// LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE
const STATE_FLAGS: u32 = 1 | 2;

/// Save the plugin's internal state (e.g. a loaded amp model). `None` if the
/// plugin has no state interface.
pub fn save_state(link: &Lv2Link) -> Option<Vec<StateEntry>> {
    let desc = link.desc as *const Lv2Descriptor;
    let iface = unsafe { state_interface(desc) }?;
    let features = FeatureSet::new(48000.0, None);
    let mut out: Vec<StateEntry> = vec![];
    unsafe { (iface.save)(link.handle as P, store_cb, &mut out as *mut _ as P, STATE_FLAGS, features.ptrs.as_ptr()) };
    Some(out)
}

unsafe fn restore_state(desc: *const Lv2Descriptor, handle: P, entries: &[StateEntry], features: &FeatureSet) {
    let Some(iface) = (unsafe { state_interface(desc) }) else { return };
    let table: Vec<(u32, u32, u32, &[u8])> = entries.iter().map(|e| (urid(&e.key), urid(&e.type_uri), e.flags, e.value.as_slice())).collect();
    unsafe { (iface.restore)(handle, retrieve_cb, &table as *const _ as P, STATE_FLAGS, features.ptrs.as_ptr()) };
}

/// Feature array handed to `instantiate`. Boxed so every pointer stays valid
/// for the plugin's lifetime.
struct FeatureSet {
    _map: Box<UridMapFeature>,
    _unmap: Box<UridUnmapFeature>,
    _opt_ints: Box<[i32; 3]>,
    _opt_rate: Box<f32>,
    _options: Vec<Lv2Option>,
    _features: Vec<Lv2Feature>,
    ptrs: Vec<*const Lv2Feature>,
}

impl FeatureSet {
    fn new(rate: f32, schedule: Option<*mut WorkerSchedule>) -> Box<Self> {
        let mut map = Box::new(UridMapFeature { handle: std::ptr::null_mut(), map: urid_map_cb });
        let mut unmap = Box::new(UridUnmapFeature { handle: std::ptr::null_mut(), unmap: urid_unmap_cb });
        let ints = Box::new([1i32, MAX_BLOCK as i32, 1024]);
        let rate = Box::new(rate);
        let t_int = urid("http://lv2plug.in/ns/ext/atom#Int");
        let t_float = urid("http://lv2plug.in/ns/ext/atom#Float");
        let opt = |key: &str, type_: u32, value: *const c_void| Lv2Option {
            context: 0,
            subject: 0,
            key: urid(key),
            size: 4,
            type_,
            value,
        };
        let mut options = vec![
            opt("http://lv2plug.in/ns/ext/buf-size#minBlockLength", t_int, &ints[0] as *const i32 as _),
            opt("http://lv2plug.in/ns/ext/buf-size#maxBlockLength", t_int, &ints[1] as *const i32 as _),
            opt("http://lv2plug.in/ns/ext/buf-size#nominalBlockLength", t_int, &ints[2] as *const i32 as _),
            opt("http://lv2plug.in/ns/ext/parameters#sampleRate", t_float, &*rate as *const f32 as _),
        ];
        options.push(Lv2Option { context: 0, subject: 0, key: 0, size: 0, type_: 0, value: std::ptr::null() });
        let features = vec![
            Lv2Feature { uri: URI_MAP.as_ptr(), data: &mut *map as *mut _ as P },
            Lv2Feature { uri: URI_UNMAP.as_ptr(), data: &mut *unmap as *mut _ as P },
            Lv2Feature { uri: URI_OPTIONS.as_ptr(), data: options.as_mut_ptr() as P },
            Lv2Feature { uri: URI_BOUNDED.as_ptr(), data: std::ptr::null_mut() },
        ];
        let mut features = features;
        if let Some(sched) = schedule {
            features.push(Lv2Feature { uri: URI_WORKER_SCHEDULE.as_ptr(), data: sched as P });
        }
        let mut ptrs: Vec<*const Lv2Feature> = features.iter().map(|f| f as *const _).collect();
        ptrs.push(std::ptr::null());
        Box::new(FeatureSet {
            _map: map,
            _unmap: unmap,
            _opt_ints: ints,
            _opt_rate: rate,
            _options: options,
            _features: features,
            ptrs,
        })
    }
}

// ---------------------------------------------------------------------------
// Runtime instance

struct Inst {
    raw: *mut LilvInstance,
    desc: *const Lv2Descriptor,
    handle: P,
    audio_in: Vec<Box<[f32]>>,
    audio_out: Vec<Box<[f32]>>,
    control_in: Box<[f32]>,
    control_out: Box<[f32]>,
    atom_in: Vec<Box<[u64]>>,
    /// LV2 port index of each `atom_in` buffer.
    atom_in_ports: Vec<u32>,
    atom_out: Vec<Box<[u64]>>,
    /// LV2 port index of each `atom_out` buffer.
    atom_out_ports: Vec<u32>,
    /// Index into `atom_in` of the port receiving MIDI.
    midi_in: Option<usize>,
    _cv: Vec<Box<[f32]>>,
    /// Worker for `work:schedule` (owned; freed after the thread stopped).
    worker: *mut Worker,
    features: Box<FeatureSet>,
}

impl Inst {
    unsafe fn new(raw: *mut LilvInstance, info: &PluginInfo, features: Box<FeatureSet>) -> Self {
        let (desc, handle) = unsafe { ((*raw).descriptor, (*raw).handle) };
        let mut inst = Inst {
            raw,
            desc,
            handle,
            audio_in: vec![],
            audio_out: vec![],
            control_in: info.info.params.iter().map(|p| p.default).collect(),
            control_out: vec![0.0; info.info.outputs.len()].into_boxed_slice(),
            atom_in: vec![],
            atom_in_ports: vec![],
            atom_out: vec![],
            atom_out_ports: vec![],
            midi_in: None,
            _cv: vec![],
            worker: std::ptr::null_mut(),
            features,
        };
        let connect = unsafe { (*desc).connect_port };
        let (mut ci, mut co) = (0, 0);
        for p in &info.ports {
            let ptr: P = match (p.kind, p.is_input) {
                (PortKind::Audio, true) => push_buf(&mut inst.audio_in, vec![0f32; MAX_BLOCK]),
                (PortKind::Audio, false) => push_buf(&mut inst.audio_out, vec![0f32; MAX_BLOCK]),
                (PortKind::Control, true) => {
                    ci += 1;
                    &mut inst.control_in[ci - 1] as *mut f32 as P
                }
                (PortKind::Control, false) => {
                    co += 1;
                    &mut inst.control_out[co - 1] as *mut f32 as P
                }
                (PortKind::Atom, true) => {
                    if p.midi && inst.midi_in.is_none() {
                        inst.midi_in = Some(inst.atom_in.len());
                    }
                    inst.atom_in_ports.push(p.index);
                    push_buf(&mut inst.atom_in, vec![0u64; ATOM_BUF_BYTES / 8])
                }
                (PortKind::Atom, false) => {
                    inst.atom_out_ports.push(p.index);
                    push_buf(&mut inst.atom_out, vec![0u64; ATOM_BUF_BYTES / 8])
                }
                _ => push_buf(&mut inst._cv, vec![0f32; MAX_BLOCK]),
            };
            unsafe { connect(handle, p.index, ptr) };
        }
        inst
    }

    /// Prepare atom buffers: inputs get an empty sequence, outputs advertise
    /// their capacity, as required by the LV2 atom spec.
    fn prepare_atoms(&mut self, seq: u32, chunk: u32) {
        for b in &mut self.atom_in {
            // LV2_Atom { size = sizeof(LV2_Atom_Sequence_Body), type = Sequence }, body { unit, pad }
            b[0] = (8u64) | ((seq as u64) << 32);
            b[1] = 0;
        }
        for b in &mut self.atom_out {
            b[0] = ((ATOM_BUF_BYTES - 8) as u64) | ((chunk as u64) << 32);
        }
    }

    /// Append one event to atom input `buf` (after `prepare_atoms`).
    /// Returns false when the sequence is full.
    fn append_event(&mut self, buf: usize, frame: i64, type_: u32, body: &[u8]) -> bool {
        let b = &mut self.atom_in[buf];
        let bytes: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr() as *mut u8, b.len() * 8) };
        // The sequence atom's size counts its body; events start 8-aligned after it.
        let size = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let at = 8 + size;
        let total = 16 + body.len().div_ceil(8) * 8;
        if at + total > bytes.len() {
            return false;
        }
        bytes[at..at + 8].copy_from_slice(&frame.to_ne_bytes());
        bytes[at + 8..at + 12].copy_from_slice(&(body.len() as u32).to_ne_bytes());
        bytes[at + 12..at + 16].copy_from_slice(&type_.to_ne_bytes());
        bytes[at + 16..at + 16 + body.len()].copy_from_slice(body);
        bytes[0..4].copy_from_slice(&((size + total) as u32).to_ne_bytes());
        true
    }

    /// Append MIDI events to the MIDI atom sequence (after `prepare_atoms`).
    fn write_midi(&mut self, events: &[crate::device::MidiEvent], midi_urid: u32) {
        let Some(i) = self.midi_in else { return };
        for e in events {
            if !self.append_event(i, e.frame as i64, midi_urid, e.bytes()) {
                break;
            }
        }
    }
}

fn push_buf<T>(list: &mut Vec<Box<[T]>>, v: Vec<T>) -> P {
    let mut b = v.into_boxed_slice();
    let ptr = b.as_mut_ptr() as P;
    list.push(b);
    ptr
}

pub struct PluginRuntime {
    insts: Vec<Inst>,
    shared: Arc<DeviceShared>,
    seq_urid: u32,
    midi_urid: u32,
    chunk_urid: u32,
    ui_rx: rtrb::Consumer<u8>,
    dsp_tx: rtrb::Producer<u8>,
    gui_open: Arc<AtomicBool>,
    /// Stereo side of each audio output (used to downmix multi-out plugins).
    out_sides: Vec<Side>,
    /// Preallocated buffer for GUI messages (no allocation on the audio thread).
    scratch: Box<[u8]>,
}

impl PluginRuntime {
    /// Forward events from the first instance's atom outputs to an open GUI.
    fn forward_atom_outputs(&mut self) {
        if !self.gui_open.load(Ordering::Relaxed) {
            return;
        }
        let inst = &self.insts[0];
        for (buf, &port) in inst.atom_out.iter().zip(&inst.atom_out_ports) {
            let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
            let size = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
            let type_ = u32::from_ne_bytes(bytes[4..8].try_into().unwrap());
            if type_ != self.seq_urid {
                continue; // the plugin wrote nothing this cycle
            }
            let end = (8 + size).min(bytes.len());
            let mut at = 16; // atom header + sequence body
            while at + 16 <= end {
                let asize = u32::from_ne_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
                let atom = &bytes[at + 8..(at + 16 + asize).min(end)];
                let need = 8 + atom.len();
                if self.dsp_tx.slots() >= need
                    && let Ok(mut chunk) = self.dsp_tx.write_chunk_uninit(need)
                {
                    let msg = port.to_ne_bytes().into_iter().chain((atom.len() as u32).to_ne_bytes()).chain(atom.iter().copied());
                    let (a, b) = chunk.as_mut_slices();
                    for (dst, v) in a.iter_mut().chain(b.iter_mut()).zip(msg) {
                        dst.write(v);
                    }
                    unsafe { chunk.commit_all() };
                }
                at += 16 + asize.div_ceil(8) * 8;
            }
        }
    }

    /// Deliver queued GUI messages to the first instance's atom inputs.
    fn deliver_ui_messages(&mut self) {
        let inst = &mut self.insts[0];
        loop {
            let avail = self.ui_rx.slots();
            if avail < 8 {
                break;
            }
            // Peek at the header (an uncommitted read consumes nothing).
            let mut h = [0u8; 8];
            {
                let Ok(head) = self.ui_rx.read_chunk(8) else { break };
                let (a, b) = head.as_slices();
                for (d, s) in h.iter_mut().zip(a.iter().chain(b)) {
                    *d = *s;
                }
            }
            let port = u32::from_ne_bytes(h[0..4].try_into().unwrap());
            let len = u32::from_ne_bytes(h[4..8].try_into().unwrap()) as usize;
            if avail < 8 + len {
                break; // message not fully written yet
            }
            let Ok(chunk) = self.ui_rx.read_chunk(8 + len) else { break };
            let (a, b) = chunk.as_slices();
            let n = len.min(self.scratch.len());
            for (d, s) in self.scratch[..n].iter_mut().zip(a.iter().chain(b).skip(8)) {
                *d = *s;
            }
            chunk.commit_all();
            if n < 8 {
                continue;
            }
            // The message is a whole LV2_Atom: header { size, type } + body.
            let type_ = u32::from_ne_bytes(self.scratch[4..8].try_into().unwrap());
            if let Some(buf) = inst.atom_in_ports.iter().position(|p| *p == port) {
                inst.append_event(buf, 0, type_, &self.scratch[8..n]);
            }
        }
    }
}

// Plugin handles are only used by one thread at a time: created on the UI
// thread, run on the audio thread, freed on the UI thread.
unsafe impl Send for PluginRuntime {}

impl Device for PluginRuntime {
    /// Process a stereo block in place. Real-time safe.
    fn process(&mut self, ctx: &mut ProcessContext) {
        if self.shared.bypass.load(Ordering::Relaxed) {
            return;
        }
        let (l, r) = (&mut *ctx.left, &mut *ctx.right);
        let n = l.len().min(r.len()).min(MAX_BLOCK);
        let dual = self.insts.len() == 2;
        for inst in self.insts.iter_mut() {
            inst.prepare_atoms(self.seq_urid, self.chunk_urid);
        }
        self.deliver_ui_messages();
        for (k, inst) in self.insts.iter_mut().enumerate() {
            for (dst, src) in inst.control_in.iter_mut().zip(&self.shared.params) {
                *dst = src.get();
            }
            inst.write_midi(ctx.midi, self.midi_urid);
            for (i, buf) in inst.audio_in.iter_mut().enumerate() {
                let src: &[f32] = match (dual, k, i) {
                    (true, 0, 0) => l,
                    (true, _, 0) => r,
                    (false, _, 0) => l,
                    (false, _, 1) => r,
                    _ => {
                        buf[..n].fill(0.0); // side-chain / extra inputs stay silent
                        continue;
                    }
                };
                buf[..n].copy_from_slice(&src[..n]);
            }
            unsafe { ((*inst.desc).run)(inst.handle, n as u32) };
            if !inst.worker.is_null() {
                unsafe { (*inst.worker).deliver_responses(inst.handle) };
            }
        }
        self.forward_atom_outputs();
        if dual {
            l[..n].copy_from_slice(&self.insts[0].audio_out[0][..n]);
            r[..n].copy_from_slice(&self.insts[1].audio_out[0][..n]);
        } else if self.out_sides.len() > 2 {
            // Multi-output plugin (e.g. a drum kit with one output per drum):
            // left/right-designated outputs go to their side, others to both.
            l[..n].fill(0.0);
            r[..n].fill(0.0);
            for (out, side) in self.insts[0].audio_out.iter().zip(&self.out_sides) {
                for i in 0..n {
                    match side {
                        Side::Left => l[i] += out[i],
                        Side::Right => r[i] += out[i],
                        Side::Center => {
                            l[i] += out[i];
                            r[i] += out[i];
                        }
                    }
                }
            }
        } else {
            let outs = &self.insts[0].audio_out;
            l[..n].copy_from_slice(&outs[0][..n]);
            let right = outs.get(1).unwrap_or(&outs[0]);
            r[..n].copy_from_slice(&right[..n]);
        }
        for (dst, v) in self.shared.outputs.iter().zip(self.insts[0].control_out.iter()) {
            dst.set(*v);
        }
    }
}

impl Drop for PluginRuntime {
    fn drop(&mut self) {
        let Ok(api) = api() else { return };
        for inst in &mut self.insts {
            unsafe {
                // Stop the worker thread before the instance goes away.
                if !inst.worker.is_null() {
                    drop(Box::from_raw(inst.worker));
                    inst.worker = std::ptr::null_mut();
                }
                if let Some(deactivate) = (*inst.desc).deactivate {
                    deactivate(inst.handle);
                }
                (api.instance_free)(inst.raw);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker extension (work:schedule / work:interface)
//
// Plugins such as sample players load files off the audio thread: `run()`
// schedules work, a worker thread executes it, and the response is handed
// back to the plugin on the audio thread after the next `run()`. Both
// directions use lock-free queues framed as [u32 size][bytes].

const URI_WORKER_SCHEDULE: &CStr = c"http://lv2plug.in/ns/ext/worker#schedule";
const URI_WORKER_INTERFACE: &CStr = c"http://lv2plug.in/ns/ext/worker#interface";
const WORKER_QUEUE_BYTES: usize = 256 * 1024;
/// LV2_WORKER_SUCCESS / LV2_WORKER_ERR_NO_SPACE
const WORK_OK: u32 = 0;
const WORK_NO_SPACE: u32 = 2;

type RespondFn = unsafe extern "C" fn(P, u32, *const c_void) -> u32;

#[repr(C)]
pub(crate) struct WorkerSchedule {
    handle: P,
    schedule_work: unsafe extern "C" fn(P, u32, *const c_void) -> u32,
}

#[repr(C)]
struct WorkerInterface {
    work: unsafe extern "C" fn(P, RespondFn, P, u32, *const c_void) -> u32,
    work_response: unsafe extern "C" fn(P, u32, *const c_void) -> u32,
    end_run: Option<unsafe extern "C" fn(P) -> u32>,
}

fn push_msg(tx: &mut rtrb::Producer<u8>, data: &[u8]) -> bool {
    let need = 4 + data.len();
    if tx.slots() < need {
        return false;
    }
    let Ok(mut chunk) = tx.write_chunk_uninit(need) else { return false };
    let msg = (data.len() as u32).to_ne_bytes().into_iter().chain(data.iter().copied());
    let (a, b) = chunk.as_mut_slices();
    for (dst, v) in a.iter_mut().chain(b.iter_mut()).zip(msg) {
        dst.write(v);
    }
    unsafe { chunk.commit_all() };
    true
}

/// Pop one message into `out`; returns its length.
fn pop_msg(rx: &mut rtrb::Consumer<u8>, out: &mut [u8]) -> Option<usize> {
    let avail = rx.slots();
    if avail < 4 {
        return None;
    }
    let mut h = [0u8; 4];
    {
        let head = rx.read_chunk(4).ok()?;
        let (a, b) = head.as_slices();
        for (d, s) in h.iter_mut().zip(a.iter().chain(b)) {
            *d = *s;
        }
    }
    let len = u32::from_ne_bytes(h) as usize;
    if avail < 4 + len {
        return None;
    }
    let chunk = rx.read_chunk(4 + len).ok()?;
    let (a, b) = chunk.as_slices();
    let n = len.min(out.len());
    for (d, s) in out[..n].iter_mut().zip(a.iter().chain(b).skip(4)) {
        *d = *s;
    }
    chunk.commit_all();
    Some(n)
}

unsafe extern "C" fn schedule_work_cb(handle: P, size: u32, data: *const c_void) -> u32 {
    let Some(w) = (unsafe { (handle as *mut Worker).as_mut() }) else { return WORK_NO_SPACE };
    let bytes = if data.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) } };
    if push_msg(&mut w.requests, bytes) { WORK_OK } else { WORK_NO_SPACE }
}

unsafe extern "C" fn respond_cb(handle: P, size: u32, data: *const c_void) -> u32 {
    let Some(tx) = (unsafe { (handle as *mut rtrb::Producer<u8>).as_mut() }) else { return WORK_NO_SPACE };
    let bytes = if data.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) } };
    if push_msg(tx, bytes) { WORK_OK } else { WORK_NO_SPACE }
}

pub(crate) struct Worker {
    schedule: WorkerSchedule,
    /// Audio thread → worker thread.
    requests: rtrb::Producer<u8>,
    /// Worker thread → audio thread.
    responses: rtrb::Consumer<u8>,
    /// Queue ends handed to the thread when it starts.
    thread_ends: Option<(rtrb::Consumer<u8>, rtrb::Producer<u8>)>,
    iface: *const WorkerInterface,
    scratch: Box<[u8]>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct SendPtr(usize, usize);
unsafe impl Send for SendPtr {}

impl Worker {
    /// Allocate a worker; the schedule feature points into it, so it lives
    /// at a fixed address until the owning instance is freed.
    fn new() -> *mut Worker {
        let (requests, req_rx) = rtrb::RingBuffer::new(WORKER_QUEUE_BYTES);
        let (resp_tx, responses) = rtrb::RingBuffer::new(WORKER_QUEUE_BYTES);
        let w = Box::into_raw(Box::new(Worker {
            schedule: WorkerSchedule { handle: std::ptr::null_mut(), schedule_work: schedule_work_cb },
            requests,
            responses,
            thread_ends: Some((req_rx, resp_tx)),
            iface: std::ptr::null(),
            scratch: vec![0u8; WORKER_QUEUE_BYTES].into_boxed_slice(),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }));
        unsafe { (*w).schedule.handle = w as P };
        w
    }

    /// Start the worker thread if the plugin implements `work:interface`.
    unsafe fn start(&mut self, handle: P, desc: *const Lv2Descriptor) {
        unsafe {
            if (*desc).extension_data.is_null() {
                return;
            }
            let ext: unsafe extern "C" fn(*const c_char) -> *const c_void = std::mem::transmute((*desc).extension_data);
            self.iface = ext(URI_WORKER_INTERFACE.as_ptr()) as *const WorkerInterface;
        }
        if self.iface.is_null() {
            return;
        }
        let Some((mut rx, tx)) = self.thread_ends.take() else { return };
        let stop = self.stop.clone();
        let ptrs = SendPtr(handle as usize, self.iface as usize);
        self.thread = std::thread::Builder::new()
            .name("studio-lv2-worker".into())
            .spawn(move || {
                let ptrs = ptrs;
                let (handle, iface) = (ptrs.0 as P, ptrs.1 as *const WorkerInterface);
                let mut tx = Box::new(tx);
                let mut buf = vec![0u8; WORKER_QUEUE_BYTES];
                while !stop.load(Ordering::Relaxed) {
                    match pop_msg(&mut rx, &mut buf) {
                        Some(n) => unsafe {
                            ((*iface).work)(handle, respond_cb, &mut *tx as *mut rtrb::Producer<u8> as P, n as u32, buf.as_ptr() as *const c_void);
                        },
                        None => std::thread::sleep(std::time::Duration::from_millis(1)),
                    }
                }
            })
            .ok();
    }

    /// Hand finished work back to the plugin (audio thread, after `run`).
    unsafe fn deliver_responses(&mut self, handle: P) {
        if self.iface.is_null() {
            return;
        }
        while let Some(n) = pop_msg(&mut self.responses, &mut self.scratch) {
            unsafe { ((*self.iface).work_response)(handle, n as u32, self.scratch.as_ptr() as *const c_void) };
        }
        if let Some(end_run) = unsafe { (*self.iface).end_run } {
            unsafe { end_run(handle) };
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Make sure lilv also finds plugins in the multiarch directory used by some
/// distributions. Must run before any threads are spawned.
pub fn setup_lv2_path() {
    if std::env::var_os("LV2_PATH").is_some() {
        return;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.lv2"),
        "/usr/local/lib/lv2".into(),
        "/usr/lib/lv2".into(),
        "/usr/local/lib64/lv2".into(),
        "/usr/lib64/lv2".into(),
        "/usr/lib/x86_64-linux-gnu/lv2".into(),
        "/usr/lib/aarch64-linux-gnu/lv2".into(),
    ];
    let path: Vec<_> = candidates.into_iter().filter(|p| std::path::Path::new(p).is_dir()).collect();
    // SAFETY: called from main() before other threads exist.
    unsafe { std::env::set_var("LV2_PATH", path.join(":")) };
}
