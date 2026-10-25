//! VST3 plugin hosting.
//!
//! Threads:
//! * **Plugin thread** ([`run`] / [`post`]): loads modules, creates and
//!   destroys instances, saves state and hosts editors (plus the Linux run
//!   loop). Every call VST3 expects on its "UI thread" happens here; JUCE
//!   plugins in particular need one consistent message thread, which Studio's
//!   own UI thread (busy with egui) cannot be.
//! * **Audio thread**: [`Vst3Device::process`] runs `IAudioProcessor::process`.
//! * **UI thread**: talks to instances through [`Vst3Link`].
//!
//! Parameters live in the device's `DeviceShared`, as for every device:
//! generic sliders and the plugin's own GUI both write there; the audio
//! thread forwards changes to the processor, the plugin thread to the
//! controller (so the GUI follows the sliders).
//!
//! Plugins are found in the standard VST3 folders (plus `VST3_PATH`). Each
//! module is loaded once to list its classes and parameters; the result is
//! cached by file size and time, so later starts only load modules that
//! changed.

// The C enums behind the SDK constants have different integer types on
// Linux and Windows, so casts that are no-ops on one platform are needed on
// the other.
#![allow(clippy::unnecessary_cast)]

pub(crate) mod com;
mod editor;

use crate::device::{Device, DeviceCategory, DeviceInfo, DeviceShared, MidiEvent, ParamInfo, ProcessContext as Block};
use crate::lv2::{MAX_BLOCK, StateEntry};
use crate::util::AtomicF32;
use com::{EventList, Handler, HostApp, MemStream, ParamChanges, read_cstring, read_wstring};
use editor::Editor;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper, Interface};

const AUDIO_MODULE_CLASS: &str = "Audio Module Class";
/// Steps up to which a parameter's value labels are listed.
const MAX_LABELLED_STEPS: i32 = 64;

/// A plugin class found by scanning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vst3Class {
    /// Class id as 32 hex digits; devices are `vst3:<cid>`.
    pub cid: String,
    pub name: String,
    pub vendor: String,
    pub instrument: bool,
    pub module: PathBuf,
    pub params: Vec<ScannedParam>,
    pub has_gui: bool,
    /// Why the class could not be instantiated while scanning.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScannedParam {
    pub id: u32,
    pub name: String,
    /// 0 = continuous, else number of steps (1 = on/off).
    pub steps: i32,
    pub default: f64,
    pub read_only: bool,
    pub labels: Vec<String>,
}

/// Display value (what `DeviceShared` holds: 0..1, or the step number) to
/// VST3's normalized value and back.
fn to_normalized(v: f32, steps: i32) -> f64 {
    let v = v as f64;
    (if steps > 0 { v / steps as f64 } else { v }).clamp(0.0, 1.0)
}

fn to_display(n: f64, steps: i32) -> f32 {
    let n = n.clamp(0.0, 1.0);
    (if steps > 0 { (n * steps as f64).round() } else { n }) as f32
}

/// The device description shown in the browser and used for the generic UI.
pub fn device_info(c: &Vst3Class) -> DeviceInfo {
    let param = |p: &ScannedParam| {
        let max = if p.steps > 0 { p.steps as f32 } else { 1.0 };
        let mut info = ParamInfo::new(&p.id.to_string(), &p.name, 0.0, max, to_display(p.default, p.steps));
        info.integer = p.steps > 0;
        info.toggled = p.steps == 1;
        if p.steps > 1 && p.labels.len() == p.steps as usize + 1 {
            info.scale_points = p.labels.iter().enumerate().map(|(i, l)| (l.clone(), i as f32)).collect();
        }
        info
    };
    DeviceInfo {
        id: format!("vst3:{}", c.cid),
        name: c.name.clone(),
        category: if c.instrument { DeviceCategory::Instrument } else { DeviceCategory::Effect },
        class: if c.vendor.is_empty() { "VST3".into() } else { format!("VST3 · {}", c.vendor) },
        params: c.params.iter().filter(|p| !p.read_only).map(param).collect(),
        outputs: c.params.iter().filter(|p| p.read_only).map(param).collect(),
        unsupported: c.error.clone(),
        has_gui: c.has_gui,
    }
}

// ---------------------------------------------------------------------------
// Plugin thread

type Job = Box<dyn FnOnce(&mut ThreadState) + Send>;

struct PluginThread {
    tx: Mutex<mpsc::Sender<Job>>,
    id: std::thread::ThreadId,
}

static THREAD: OnceLock<PluginThread> = OnceLock::new();

fn thread() -> &'static PluginThread {
    THREAD.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let handle = std::thread::Builder::new().name("studio-vst3".into()).spawn(move || thread_main(rx)).expect("spawn VST3 thread");
        PluginThread { tx: Mutex::new(tx), id: handle.thread().id() }
    })
}

/// Queue `f` on the plugin thread.
fn post(f: impl FnOnce(&mut ThreadState) + Send + 'static) {
    if let Ok(tx) = thread().tx.lock() {
        let _ = tx.send(Box::new(f));
    }
}

/// Run `f` on the plugin thread and wait for its result.
fn run<R: Send + 'static>(f: impl FnOnce(&mut ThreadState) -> R + Send + 'static) -> Result<R, String> {
    let t = thread();
    assert_ne!(std::thread::current().id(), t.id, "vst3::run called on the plugin thread");
    let (tx, rx) = mpsc::channel();
    post(move |st| {
        let _ = tx.send(f(st));
    });
    rx.recv().map_err(|_| "the VST3 thread stopped".to_string())
}

#[derive(Default)]
struct ThreadState {
    modules: HashMap<PathBuf, Module>,
    instances: HashMap<u64, Instance>,
    next_id: u64,
}

fn thread_main(rx: mpsc::Receiver<Job>) {
    #[cfg(windows)]
    unsafe {
        // Plugin GUIs expect an OLE apartment (drag and drop, clipboard).
        let _ = windows::Win32::System::Ole::OleInitialize(None);
    }
    let mut st = ThreadState::default();
    loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(job) => job(&mut st),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(job) = rx.try_recv() {
            job(&mut st);
        }
        editor::pump_os();
        for inst in st.instances.values_mut() {
            inst.pump();
        }
    }
}

// ---------------------------------------------------------------------------
// Modules

struct Module {
    factory: ComPtr<IPluginFactory>,
    /// Never unloaded: many plugins (JUCE ones included) do not survive it.
    _lib: libloading::Library,
}

/// The shared library inside a bundle (or the legacy single-file module).
fn module_binary(bundle: &Path) -> Option<PathBuf> {
    if bundle.is_file() {
        return Some(bundle.to_path_buf());
    }
    let (arch, ext) = if cfg!(windows) { ("x86_64-win", "vst3") } else { ("x86_64-linux", "so") };
    let dir = bundle.join("Contents").join(arch);
    let stem = bundle.file_stem()?;
    let named = dir.join(stem).with_extension(ext);
    if named.is_file() {
        return Some(named);
    }
    std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == ext))
}

fn load_module(path: &Path) -> Result<Module, String> {
    unsafe {
        #[cfg(unix)]
        let lib = {
            let lib = libloading::os::unix::Library::new(path).map_err(|e| e.to_string())?;
            let handle = lib.into_raw();
            let lib = libloading::os::unix::Library::from_raw(handle);
            if let Ok(entry) = lib.get::<unsafe extern "C" fn(*mut c_void) -> bool>(b"ModuleEntry\0")
                && !entry(handle)
            {
                return Err("ModuleEntry failed".into());
            }
            libloading::Library::from(lib)
        };
        #[cfg(windows)]
        let lib = {
            let lib = libloading::Library::new(path).map_err(|e| e.to_string())?;
            if let Ok(init) = lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0")
                && !init()
            {
                return Err("InitDll failed".into());
            }
            lib
        };
        let get = lib
            .get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory\0")
            .map_err(|_| "not a VST3 module (no GetPluginFactory)".to_string())?;
        let factory = ComPtr::from_raw(get()).ok_or("the module has no plugin factory")?;
        Ok(Module { factory, _lib: lib })
    }
}

impl ThreadState {
    fn factory(&mut self, path: &Path) -> Result<ComPtr<IPluginFactory>, String> {
        if !self.modules.contains_key(path) {
            let m = load_module(path)?;
            self.modules.insert(path.to_path_buf(), m);
        }
        Ok(self.modules[path].factory.clone())
    }
}

/// Byte order of the class id in its canonical string form. Windows stores
/// the first three fields as a little-endian COM GUID; the string form (as
/// in `moduleinfo.json`) is the same on every platform, so projects move
/// between systems.
fn canonical_order(cid: &TUID) -> [u8; 16] {
    let b: [u8; 16] = cid.map(|c| c as u8);
    if cfg!(windows) { [b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]] } else { b }
}

fn cid_hex(cid: &TUID) -> String {
    canonical_order(cid).iter().map(|b| format!("{b:02X}")).collect()
}

fn parse_cid(hex: &str) -> Option<TUID> {
    if hex.len() != 32 {
        return None;
    }
    let mut cid: TUID = [0; 16];
    for (i, c) in cid.iter_mut().enumerate() {
        *c = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()? as c_char;
    }
    // The reordering is its own inverse.
    Some(canonical_order(&cid).map(|b| b as c_char))
}

/// List the audio module classes of one module (on the plugin thread).
fn scan_module(st: &mut ThreadState, path: &Path) -> Result<Vec<Vst3Class>, String> {
    let factory = st.factory(path)?;
    let mut classes = vec![];
    unsafe {
        let mut finfo: PFactoryInfo = std::mem::zeroed();
        factory.getFactoryInfo(&mut finfo);
        let factory_vendor = read_cstring(&finfo.vendor);
        let f2 = factory.cast::<IPluginFactory2>();
        for i in 0..factory.countClasses() {
            let mut info: PClassInfo = std::mem::zeroed();
            if factory.getClassInfo(i, &mut info) != kResultOk || read_cstring(&info.category) != AUDIO_MODULE_CLASS {
                continue;
            }
            let (subcategories, vendor) = match &f2 {
                Some(f2) => {
                    let mut i2: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(i, &mut i2) == kResultOk {
                        (read_cstring(&i2.subCategories), read_cstring(&i2.vendor))
                    } else {
                        (String::new(), String::new())
                    }
                }
                None => (String::new(), String::new()),
            };
            let mut class = Vst3Class {
                cid: cid_hex(&info.cid),
                name: read_cstring(&info.name),
                vendor: if vendor.is_empty() { factory_vendor.clone() } else { vendor },
                instrument: subcategories.split('|').any(|s| s == "Instrument"),
                module: path.to_path_buf(),
                params: vec![],
                has_gui: false,
                error: None,
            };
            match Instance::create(&factory, &info.cid, 48000.0, None) {
                Ok((inst, _processor)) => {
                    class.params = inst.scan_params();
                    class.has_gui = inst.controller.as_ref().and_then(|c| ComPtr::from_raw(c.createView(c"editor".as_ptr()))).is_some();
                }
                Err(e) => class.error = Some(e),
            }
            classes.push(class);
        }
    }
    Ok(classes)
}

// ---------------------------------------------------------------------------
// Instances (plugin thread side)

/// Parameter bookkeeping shared by the plugin thread (controller side) and
/// the component handler.
pub struct ParamSync {
    shared: Arc<DeviceShared>,
    /// Parameter index → id and step count (indices of `DeviceShared::params`).
    ids: Vec<ParamID>,
    steps: Vec<i32>,
    out_ids: Vec<ParamID>,
    out_steps: Vec<i32>,
    /// Values the controller has (display units), to send only changes.
    known: Vec<AtomicF32>,
    out_known: Vec<AtomicF32>,
}

impl ParamSync {
    fn new(class: &Vst3Class, shared: Arc<DeviceShared>) -> Self {
        let (outs, ins): (Vec<&ScannedParam>, Vec<&ScannedParam>) = class.params.iter().partition(|p| p.read_only);
        ParamSync {
            ids: ins.iter().map(|p| p.id).collect(),
            steps: ins.iter().map(|p| p.steps).collect(),
            out_ids: outs.iter().map(|p| p.id).collect(),
            out_steps: outs.iter().map(|p| p.steps).collect(),
            known: ins.iter().map(|_| AtomicF32::new(f32::NAN)).collect(),
            out_known: outs.iter().map(|_| AtomicF32::new(f32::NAN)).collect(),
            shared,
        }
    }

    fn index(&self, id: ParamID) -> Option<usize> {
        self.ids.iter().position(|&i| i == id)
    }

    /// An edit from the plugin's GUI (or a value the controller reports).
    pub fn set_from_controller(&self, id: ParamID, normalized: f64) {
        if let Some(i) = self.index(id)
            && let Some(p) = self.shared.params.get(i)
        {
            let v = to_display(normalized, self.steps[i]);
            p.set(v);
            self.known[i].set(v);
        }
    }

    /// Tell the controller about values changed elsewhere.
    fn push_to(&self, controller: &ComPtr<IEditController>) {
        let ins = self.ids.iter().zip(&self.steps).zip(&self.known).zip(&self.shared.params);
        let outs = self.out_ids.iter().zip(&self.out_steps).zip(&self.out_known).zip(&self.shared.outputs);
        for (((id, steps), known), value) in ins.chain(outs) {
            let v = value.get();
            if v != known.get() {
                known.set(v);
                unsafe { controller.setParamNormalized(*id, to_normalized(v, *steps)) };
            }
        }
    }
}

struct Instance {
    component: ComPtr<IComponent>,
    controller: Option<ComPtr<IEditController>>,
    /// Controller is a separate object (not the component itself).
    separate: bool,
    points: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    handler: Option<ComWrapper<Handler>>,
    _host: ComWrapper<HostApp>,
    editor: Option<Editor>,
    editor_open: Arc<AtomicBool>,
    sync: Option<Arc<ParamSync>>,
    active: bool,
}

/// What the audio thread needs from a new instance.
struct ProcessorSetup {
    processor: ComPtr<IAudioProcessor>,
    in_channels: Vec<i32>,
    out_channels: Vec<i32>,
    /// Per MIDI channel: controller number (128 = aftertouch, 129 = pitch
    /// bend) → parameter.
    midi_map: Vec<[Option<ParamID>; 130]>,
}

fn guid_ptr<I: Interface>() -> FIDString {
    I::IID.as_ptr() as FIDString
}

impl Instance {
    /// Create, initialize and activate an instance of class `cid`,
    /// restoring `state` if given.
    unsafe fn create(factory: &ComPtr<IPluginFactory>, cid: &TUID, rate: f64, state: Option<&[StateEntry]>) -> Result<(Instance, ProcessorSetup), String> {
        unsafe {
            let host = ComWrapper::new(HostApp);
            let host_ptr = host.as_com_ref::<IHostApplication>().ok_or("host object")?.as_ptr() as *mut FUnknown;
            if let Some(f3) = factory.cast::<IPluginFactory3>() {
                f3.setHostContext(host_ptr);
            }
            let mut obj: *mut c_void = std::ptr::null_mut();
            factory.createInstance(cid.as_ptr(), guid_ptr::<IComponent>(), &mut obj);
            let component = ComPtr::from_raw(obj as *mut IComponent).ok_or("the plugin could not be created")?;
            if component.initialize(host_ptr) != kResultOk {
                return Err("the plugin failed to initialize".into());
            }
            let mut inst = Instance {
                component,
                controller: None,
                separate: false,
                points: None,
                handler: None,
                _host: host,
                editor: None,
                editor_open: Arc::new(AtomicBool::new(false)),
                sync: None,
                active: false,
            };
            // The controller: the component itself, or a separate class.
            inst.controller = inst.component.cast::<IEditController>();
            if inst.controller.is_none() {
                let mut ctrl_cid: TUID = [0; 16];
                if inst.component.getControllerClassId(&mut ctrl_cid) == kResultOk {
                    let mut obj: *mut c_void = std::ptr::null_mut();
                    factory.createInstance(ctrl_cid.as_ptr(), guid_ptr::<IEditController>(), &mut obj);
                    if let Some(c) = ComPtr::from_raw(obj as *mut IEditController) {
                        if c.initialize(host_ptr) == kResultOk {
                            inst.controller = Some(c);
                            inst.separate = true;
                        } else {
                            eprintln!("studio: VST3 controller failed to initialize; no GUI or parameters");
                        }
                    }
                }
            }
            if inst.separate
                && let (Some(a), Some(b)) = (inst.component.cast::<IConnectionPoint>(), inst.controller.as_ref().and_then(|c| c.cast::<IConnectionPoint>()))
            {
                a.connect(b.as_ptr());
                b.connect(a.as_ptr());
                inst.points = Some((a, b));
            }
            let processor = inst.component.cast::<IAudioProcessor>().ok_or("the plugin has no audio processor")?;
            if processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32) != kResultOk {
                return Err("the plugin cannot process 32-bit audio".into());
            }

            // Restore state before activation.
            let entry = |key: &str| state.and_then(|s| s.iter().find(|e| e.key == key)).map(|e| e.value.clone());
            if let Some(bytes) = entry(STATE_COMPONENT) {
                let stream = MemStream::new(bytes.clone());
                if let Some(s) = stream.as_com_ref::<IBStream>() {
                    inst.component.setState(s.as_ptr());
                }
                if let Some(c) = &inst.controller {
                    let stream = MemStream::new(bytes);
                    if let Some(s) = stream.as_com_ref::<IBStream>() {
                        c.setComponentState(s.as_ptr());
                    }
                }
            }
            if let (Some(bytes), Some(c)) = (entry(STATE_CONTROLLER), &inst.controller) {
                let stream = MemStream::new(bytes);
                if let Some(s) = stream.as_com_ref::<IBStream>() {
                    c.setState(s.as_ptr());
                }
            }

            // Buses: stereo main input and output where the plugin agrees.
            let audio = MediaTypes_::kAudio as i32;
            let (dir_in, dir_out) = (BusDirections_::kInput as i32, BusDirections_::kOutput as i32);
            let arrangement = |dir: i32, i: i32| {
                let mut a: SpeakerArrangement = 0;
                processor.getBusArrangement(dir, i, &mut a);
                a
            };
            let n_in = inst.component.getBusCount(audio, dir_in);
            let n_out = inst.component.getBusCount(audio, dir_out);
            let mut ins: Vec<SpeakerArrangement> = (0..n_in).map(|i| arrangement(dir_in, i)).collect();
            let mut outs: Vec<SpeakerArrangement> = (0..n_out).map(|i| arrangement(dir_out, i)).collect();
            if let Some(a) = ins.first_mut() {
                *a = SpeakerArr::kStereo;
            }
            if let Some(a) = outs.first_mut() {
                *a = SpeakerArr::kStereo;
            }
            if processor.setBusArrangements(ins.as_mut_ptr(), n_in, outs.as_mut_ptr(), n_out) != kResultOk {
                ins = (0..n_in).map(|i| arrangement(dir_in, i)).collect();
                outs = (0..n_out).map(|i| arrangement(dir_out, i)).collect();
            }
            if n_in > 0 {
                inst.component.activateBus(audio, dir_in, 0, 1);
            }
            if n_out > 0 {
                inst.component.activateBus(audio, dir_out, 0, 1);
            }
            if inst.component.getBusCount(MediaTypes_::kEvent as i32, dir_in) > 0 {
                inst.component.activateBus(MediaTypes_::kEvent as i32, dir_in, 0, 1);
            }
            let mut setup = ProcessSetup {
                processMode: ProcessModes_::kRealtime as i32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: MAX_BLOCK as i32,
                sampleRate: rate,
            };
            if processor.setupProcessing(&mut setup) != kResultOk {
                return Err("the plugin refused the processing setup".into());
            }
            if inst.component.setActive(1) != kResultOk {
                return Err("the plugin failed to activate".into());
            }
            inst.active = true;

            let mut midi_map = vec![];
            if let Some(mapping) = inst.controller.as_ref().and_then(|c| c.cast::<IMidiMapping>()) {
                midi_map = vec![[None; 130]; 16];
                for (channel, map) in midi_map.iter_mut().enumerate() {
                    for (cc, slot) in map.iter_mut().enumerate() {
                        let mut id: ParamID = 0;
                        if mapping.getMidiControllerAssignment(0, channel as i16, cc as i16, &mut id) == kResultOk {
                            *slot = Some(id);
                        }
                    }
                }
            }
            let channels = |arr: &[SpeakerArrangement]| arr.iter().map(|a| a.count_ones() as i32).collect();
            let setup = ProcessorSetup { processor, in_channels: channels(&ins), out_channels: channels(&outs), midi_map };
            Ok((inst, setup))
        }
    }

    /// Parameters as the controller lists them, without the hidden ones and
    /// the MIDI-controller proxies some frameworks add (JUCE adds 16 × 130),
    /// which are not automatable and only reached through MIDI.
    fn scan_params(&self) -> Vec<ScannedParam> {
        use ParameterInfo_::ParameterFlags_::*;
        let Some(c) = &self.controller else { return vec![] };
        let mut proxies = std::collections::HashSet::new();
        if let Some(mapping) = c.cast::<IMidiMapping>() {
            for channel in 0..16 {
                for cc in 0..130 {
                    let mut id: ParamID = 0;
                    if unsafe { mapping.getMidiControllerAssignment(0, channel, cc, &mut id) } == kResultOk {
                        proxies.insert(id);
                    }
                }
            }
        }
        let mut out = vec![];
        unsafe {
            for i in 0..c.getParameterCount() {
                let mut info: ParameterInfo = std::mem::zeroed();
                if c.getParameterInfo(i, &mut info) != kResultOk {
                    continue;
                }
                let flags = info.flags;
                if flags & (kIsHidden | kIsBypass | kIsProgramChange) as i32 != 0
                    || (proxies.contains(&info.id) && flags & kCanAutomate as i32 == 0)
                {
                    continue;
                }
                let steps = info.stepCount.max(0);
                let mut labels = vec![];
                if steps > 0 && steps <= MAX_LABELLED_STEPS {
                    for k in 0..=steps {
                        let mut s: String128 = [0; 128];
                        if c.getParamStringByValue(info.id, k as f64 / steps as f64, &mut s) == kResultOk {
                            labels.push(read_wstring(&s));
                        }
                    }
                }
                out.push(ScannedParam {
                    id: info.id,
                    name: read_wstring(&info.title),
                    steps,
                    default: info.defaultNormalizedValue,
                    read_only: flags & kIsReadOnly as i32 != 0,
                    labels,
                });
            }
        }
        out
    }

    fn save_state(&self) -> Vec<StateEntry> {
        let mut entries = vec![];
        let mut save = |key: &str, get: &dyn Fn(*mut IBStream) -> tresult| {
            let stream = MemStream::new(vec![]);
            if let Some(s) = stream.as_com_ref::<IBStream>()
                && get(s.as_ptr()) == kResultOk
            {
                entries.push(StateEntry { key: key.into(), type_uri: String::new(), flags: 0, value: stream.bytes() });
            }
        };
        save(STATE_COMPONENT, &|s| unsafe { self.component.getState(s) });
        if self.separate
            && let Some(c) = &self.controller
        {
            save(STATE_CONTROLLER, &|s| unsafe { c.getState(s) });
        }
        entries
    }

    fn open_editor(&mut self, title: &str) -> Result<(), String> {
        if self.editor.is_some() {
            return Ok(());
        }
        let c = self.controller.as_ref().ok_or("this plugin has no editor")?;
        let view = unsafe { ComPtr::from_raw(c.createView(c"editor".as_ptr())) }.ok_or("this plugin has no editor")?;
        self.editor = Some(Editor::open(view, title)?);
        self.editor_open.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn close_editor(&mut self) {
        self.editor = None;
        self.editor_open.store(false, Ordering::Relaxed);
    }

    /// Periodic work on the plugin thread: editor events, parameter sync and
    /// restart requests.
    fn pump(&mut self) {
        if let Some(e) = &mut self.editor
            && !e.pump()
        {
            self.close_editor();
        }
        let (Some(c), Some(sync)) = (&self.controller, &self.sync) else { return };
        let flags = self.handler.as_ref().map_or(0, |h| h.restart.swap(0, Ordering::Relaxed));
        if flags & RestartFlags_::kParamValuesChanged as i32 != 0 {
            for &id in &sync.ids {
                sync.set_from_controller(id, unsafe { c.getParamNormalized(id) });
            }
        }
        sync.push_to(c);
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.close_editor();
        unsafe {
            if self.active {
                self.component.setActive(0);
            }
            if let Some((a, b)) = self.points.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = self.controller.take() {
                c.setComponentHandler(std::ptr::null_mut());
                if self.separate {
                    c.terminate();
                }
            }
            self.component.terminate();
        }
    }
}

const STATE_COMPONENT: &str = "vst3:component";
const STATE_CONTROLLER: &str = "vst3:controller";

// ---------------------------------------------------------------------------
// Host: scanning and creating devices

/// Bump when scanning changes what it records, to rescan cached modules.
const CACHE_VERSION: u32 = 2;

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    version: u32,
    modules: Vec<CacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    path: PathBuf,
    size: u64,
    modified: u64,
    classes: Vec<Vst3Class>,
    error: Option<String>,
}

fn cache_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(crate::util::home_dir)
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| crate::util::home_dir().join(".cache"))
    };
    base.join(if cfg!(windows) { "Studio" } else { "studio" }).join("vst3-cache.json")
}

/// Folders searched for plugins: `VST3_PATH`, then the standard ones.
pub fn search_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var_os("VST3_PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if cfg!(windows) {
        for (var, sub) in [("COMMONPROGRAMFILES", "VST3"), ("LOCALAPPDATA", "Programs\\Common\\VST3")] {
            if let Some(base) = std::env::var_os(var) {
                v.push(PathBuf::from(base).join(sub));
            }
        }
    } else {
        v.push(crate::util::home_dir().join(".vst3"));
        v.extend(["/usr/lib/vst3", "/usr/local/lib/vst3", "/usr/lib64/vst3"].map(PathBuf::from));
    }
    v.dedup();
    v
}

/// `.vst3` bundles (or files) under `dir`, looking into vendor folders.
fn find_bundles(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vst3")) {
            out.push(p);
        } else if depth > 0 && p.is_dir() {
            find_bundles(&p, depth - 1, out);
        }
    }
}

pub struct Vst3Host {
    pub classes: Vec<Vst3Class>,
    /// Modules that failed to load: (path, why).
    pub failures: Vec<(PathBuf, String)>,
}

impl Vst3Host {
    /// Find plugins, loading only modules not in the cache (or changed).
    pub fn new() -> Self {
        let mut cache: Cache = std::fs::read_to_string(cache_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|c: &Cache| c.version == CACHE_VERSION)
            .unwrap_or_default();
        let mut bundles = vec![];
        for dir in search_paths() {
            find_bundles(&dir, 3, &mut bundles);
        }
        bundles.sort();
        bundles.dedup();
        let mut host = Vst3Host { classes: vec![], failures: vec![] };
        let mut fresh = Cache { version: CACHE_VERSION, modules: vec![] };
        let mut changed = false;
        for bundle in bundles {
            let Some(binary) = module_binary(&bundle) else { continue };
            let Ok(meta) = std::fs::metadata(&binary) else { continue };
            let size = meta.len();
            let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            let cached = cache.modules.iter().position(|c| c.path == binary && c.size == size && c.modified == modified);
            let entry = match cached {
                Some(i) => cache.modules.swap_remove(i),
                None => {
                    changed = true;
                    let path = binary.clone();
                    let scanned = run(move |st| scan_module(st, &path)).and_then(|r| r);
                    let (classes, error) = match scanned {
                        Ok(c) => (c, None),
                        Err(e) => (vec![], Some(e)),
                    };
                    CacheEntry { path: binary.clone(), size, modified, classes, error }
                }
            };
            host.classes.extend(entry.classes.iter().cloned());
            if let Some(e) = &entry.error {
                host.failures.push((entry.path.clone(), e.clone()));
            }
            fresh.modules.push(entry);
        }
        if changed || !cache.modules.is_empty() {
            let path = cache_path();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(s) = serde_json::to_string(&fresh) {
                let _ = std::fs::write(path, s);
            }
        }
        host
    }

    pub fn find(&self, cid: &str) -> Option<&Vst3Class> {
        self.classes.iter().find(|c| c.cid == cid)
    }

    /// Create a device of class `cid` (on the plugin thread), restoring `state`.
    pub fn instantiate(&self, cid: &str, rate: f64, shared: Arc<DeviceShared>, state: Option<&[StateEntry]>) -> Result<(Vst3Device, Arc<Vst3Link>), String> {
        let class = self.find(cid).ok_or_else(|| format!("VST3 plugin not installed: {cid}"))?.clone();
        let tuid = parse_cid(&class.cid).ok_or("bad class id")?;
        let state = state.map(|s| s.to_vec());
        let sync = Arc::new(ParamSync::new(&class, shared.clone()));
        let sync2 = sync.clone();
        let module = class.module.clone();
        let (id, setup, editor_open) = run(move |st| -> Result<_, String> {
            let factory = st.factory(&module)?;
            let (mut inst, setup) = unsafe { Instance::create(&factory, &tuid, rate, state.as_deref())? };
            // Start from the plugin's own values (restored state included).
            if let Some(c) = &inst.controller {
                for &id in &sync2.ids {
                    sync2.set_from_controller(id, unsafe { c.getParamNormalized(id) });
                }
                let handler = ComWrapper::new(Handler { sync: sync2.clone(), restart: AtomicI32::new(0) });
                if let Some(h) = handler.as_com_ref::<IComponentHandler>() {
                    unsafe { c.setComponentHandler(h.as_ptr()) };
                }
                inst.handler = Some(handler);
            }
            inst.sync = Some(sync2);
            st.next_id += 1;
            let id = st.next_id;
            let open = inst.editor_open.clone();
            st.instances.insert(id, inst);
            Ok((id, SendProcessor(setup), open))
        })??;
        let device = Vst3Device::new(id, setup.0, shared, sync, rate);
        let link = Arc::new(Vst3Link { id, editor_open });
        Ok((device, link))
    }
}

/// Moves a new processor from the plugin thread to the audio thread.
struct SendProcessor(ProcessorSetup);
// SAFETY: VST3 processors are made to be called from the audio thread
// while the rest of the plugin lives on the UI thread.
unsafe impl Send for SendProcessor {}

// ---------------------------------------------------------------------------
// UI-side handle

/// The UI thread's handle on a running instance: editor and state.
pub struct Vst3Link {
    id: u64,
    editor_open: Arc<AtomicBool>,
}

impl Vst3Link {
    pub fn open_editor(&self, title: String) -> Result<(), String> {
        let id = self.id;
        run(move |st| st.instances.get_mut(&id).ok_or("the plugin is gone".to_string())?.open_editor(&title))?
    }

    pub fn close_editor(&self) {
        if self.editor_open() {
            let id = self.id;
            let _ = run(move |st| {
                if let Some(i) = st.instances.get_mut(&id) {
                    i.close_editor();
                }
            });
        }
    }

    pub fn editor_open(&self) -> bool {
        self.editor_open.load(Ordering::Relaxed)
    }

    pub fn save_state(&self) -> Option<Vec<StateEntry>> {
        let id = self.id;
        run(move |st| st.instances.get(&id).map(|i| i.save_state())).ok().flatten()
    }
}

// ---------------------------------------------------------------------------
// Audio thread side

struct Bus {
    bufs: Vec<Vec<f32>>,
    ptrs: Vec<*mut f32>,
}

impl Bus {
    fn new(channels: i32) -> Self {
        let bufs: Vec<Vec<f32>> = (0..channels.max(0)).map(|_| vec![0.0; MAX_BLOCK]).collect();
        let ptrs = vec![std::ptr::null_mut(); bufs.len()];
        Bus { bufs, ptrs }
    }

    fn header(&mut self) -> AudioBusBuffers {
        for (p, b) in self.ptrs.iter_mut().zip(&mut self.bufs) {
            *p = b.as_mut_ptr();
        }
        AudioBusBuffers { numChannels: self.bufs.len() as i32, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: self.ptrs.as_mut_ptr() } }
    }
}

/// A VST3 plugin in a track's chain.
pub struct Vst3Device {
    id: u64,
    processor: Option<ComPtr<IAudioProcessor>>,
    shared: Arc<DeviceShared>,
    sync: Arc<ParamSync>,
    /// Values last sent to the processor.
    last: Vec<f32>,
    in_changes: ComWrapper<ParamChanges>,
    out_changes: ComWrapper<ParamChanges>,
    events: ComWrapper<EventList>,
    inputs: Vec<Bus>,
    outputs: Vec<Bus>,
    in_headers: Vec<AudioBusBuffers>,
    out_headers: Vec<AudioBusBuffers>,
    midi_map: Vec<[Option<ParamID>; 130]>,
    context: ProcessContext,
    processing: bool,
}

// SAFETY: the device is created on the UI thread and then only used by the
// audio thread; the COM objects it owns are not shared with other threads
// (the processor is the audio half of the plugin by design).
unsafe impl Send for Vst3Device {}

impl Vst3Device {
    fn new(id: u64, setup: ProcessorSetup, shared: Arc<DeviceShared>, sync: Arc<ParamSync>, rate: f64) -> Self {
        let mut context: ProcessContext = unsafe { std::mem::zeroed() };
        context.sampleRate = rate;
        let inputs: Vec<Bus> = setup.in_channels.iter().map(|&c| Bus::new(c)).collect();
        let outputs: Vec<Bus> = setup.out_channels.iter().map(|&c| Bus::new(c)).collect();
        let zero = || AudioBusBuffers { numChannels: 0, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: std::ptr::null_mut() } };
        Vst3Device {
            id,
            processor: Some(setup.processor),
            last: vec![f32::NAN; sync.ids.len()],
            in_changes: ParamChanges::new(sync.ids.len() + 16),
            out_changes: ParamChanges::new(sync.ids.len() + sync.out_ids.len() + 16),
            events: EventList::new(),
            in_headers: inputs.iter().map(|_| zero()).collect(),
            out_headers: outputs.iter().map(|_| zero()).collect(),
            inputs,
            outputs,
            midi_map: setup.midi_map,
            context,
            processing: false,
            shared,
            sync,
        }
    }

    /// A MIDI controller value for parameter `id`, also shown in the UI.
    fn controller_value(&mut self, id: ParamID, frame: u32, normalized: f64) {
        self.in_changes.add(id, frame as i32, normalized);
        if let Some(i) = self.sync.index(id) {
            let v = to_display(normalized, self.sync.steps[i]);
            self.shared.params[i].set(v);
            self.last[i] = v;
        }
    }

    fn midi(&mut self, e: &MidiEvent) {
        let d = e.bytes();
        let (Some(&status), data1, data2) = (d.first(), d.get(1).copied().unwrap_or(0), d.get(2).copied().unwrap_or(0)) else { return };
        let channel = (status & 0x0F) as i16;
        let mapped = |cc: usize| self.midi_map.get(channel as usize).and_then(|m| m[cc]);
        let mut ev: Event = unsafe { std::mem::zeroed() };
        ev.sampleOffset = e.frame as i32;
        match status & 0xF0 {
            0x90 if data2 > 0 => {
                ev.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
                ev.__field0.noteOn = NoteOnEvent { channel, pitch: data1 as i16, tuning: 0.0, velocity: data2 as f32 / 127.0, length: 0, noteId: -1 };
                self.events.push(ev);
            }
            0x80 | 0x90 => {
                ev.r#type = Event_::EventTypes_::kNoteOffEvent as u16;
                ev.__field0.noteOff = NoteOffEvent { channel, pitch: data1 as i16, velocity: data2 as f32 / 127.0, noteId: -1, tuning: 0.0 };
                self.events.push(ev);
            }
            0xB0 => {
                if let Some(id) = mapped((data1 & 0x7F) as usize) {
                    self.controller_value(id, e.frame, data2 as f64 / 127.0);
                }
            }
            0xD0 => {
                if let Some(id) = mapped(ControllerNumbers_::kAfterTouch as usize) {
                    self.controller_value(id, e.frame, data1 as f64 / 127.0);
                }
            }
            0xE0 => {
                if let Some(id) = mapped(ControllerNumbers_::kPitchBend as usize) {
                    let bend = ((data2 as u32) << 7 | data1 as u32) as f64 / 16383.0;
                    self.controller_value(id, e.frame, bend);
                }
            }
            _ => {}
        }
    }
}

impl Device for Vst3Device {
    fn process(&mut self, ctx: &mut Block) {
        let n = ctx.left.len().min(ctx.right.len()).min(MAX_BLOCK);
        if n == 0 || self.shared.bypass.load(Ordering::Relaxed) {
            return;
        }
        if self.processor.is_none() {
            return;
        }
        self.in_changes.clear();
        self.out_changes.clear();
        self.events.clear();
        for i in 0..self.last.len() {
            let v = self.shared.params[i].get();
            if v != self.last[i] {
                self.last[i] = v;
                self.in_changes.add(self.sync.ids[i], 0, to_normalized(v, self.sync.steps[i]));
            }
        }
        for e in ctx.midi {
            self.midi(e);
        }
        if let Some(bus) = self.inputs.first_mut() {
            match bus.bufs.as_mut_slice() {
                [] => {}
                [mono] => {
                    for (d, (l, r)) in mono[..n].iter_mut().zip(ctx.left.iter().zip(ctx.right.iter())) {
                        *d = 0.5 * (l + r);
                    }
                }
                [l, r, ..] => {
                    l[..n].copy_from_slice(&ctx.left[..n]);
                    r[..n].copy_from_slice(&ctx.right[..n]);
                }
            }
        }
        for (h, b) in self.in_headers.iter_mut().zip(&mut self.inputs) {
            *h = b.header();
        }
        for (h, b) in self.out_headers.iter_mut().zip(&mut self.outputs) {
            *h = b.header();
        }
        let ptr_of = |v: &mut Vec<AudioBusBuffers>| if v.is_empty() { std::ptr::null_mut() } else { v.as_mut_ptr() };
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: n as i32,
            numInputs: self.in_headers.len() as i32,
            numOutputs: self.out_headers.len() as i32,
            inputs: ptr_of(&mut self.in_headers),
            outputs: ptr_of(&mut self.out_headers),
            inputParameterChanges: self.in_changes.as_com_ref::<IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputParameterChanges: self.out_changes.as_com_ref::<IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            inputEvents: self.events.as_com_ref::<IEventList>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputEvents: std::ptr::null_mut(),
            processContext: &mut self.context,
        };
        let Some(processor) = &self.processor else { return };
        unsafe {
            if !self.processing {
                processor.setProcessing(1);
                self.processing = true;
            }
            processor.process(&mut data);
        }
        self.context.continousTimeSamples += n as i64;
        if let Some(bus) = self.outputs.first() {
            match bus.bufs.as_slice() {
                [] => {}
                [mono] => {
                    ctx.left[..n].copy_from_slice(&mono[..n]);
                    ctx.right[..n].copy_from_slice(&mono[..n]);
                }
                [l, r, ..] => {
                    ctx.left[..n].copy_from_slice(&l[..n]);
                    ctx.right[..n].copy_from_slice(&r[..n]);
                }
            }
        }
        for q in self.out_changes.queues() {
            if let (Some(i), Some(v)) = (self.sync.out_ids.iter().position(|&id| id == q.id()), q.last()) {
                self.shared.outputs[i].set(to_display(v, self.sync.out_steps[i]));
            }
        }
    }
}

impl Drop for Vst3Device {
    /// Hand the processor back to the plugin thread, which destroys the instance.
    fn drop(&mut self) {
        let processor = self.processor.take().map(|p| SendProcessor(ProcessorSetup { processor: p, in_channels: vec![], out_channels: vec![], midi_map: vec![] }));
        let (id, processing) = (self.id, self.processing);
        post(move |st| {
            if let Some(p) = processor
                && processing
            {
                unsafe { p.0.processor.setProcessing(0) };
            }
            st.instances.remove(&id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_conversion_and_ids() {
        assert_eq!(to_normalized(3.0, 4), 0.75);
        assert_eq!(to_display(0.75, 4), 3.0);
        assert_eq!(to_display(0.3, 0), 0.3);
        let cid = parse_cid("ABCDEF019182FAEB5446544654334B30").unwrap();
        assert_eq!(cid_hex(&cid), "ABCDEF019182FAEB5446544654334B30");
        assert!(parse_cid("XYZ").is_none());
    }

    #[test]
    fn memory_stream_reads_writes_and_seeks() {
        let s = MemStream::new(vec![]);
        let mut n = 0;
        let mut data = *b"hello";
        unsafe {
            s.write(data.as_mut_ptr() as *mut c_void, 5, &mut n);
            assert_eq!(n, 5);
            let mut pos = 0;
            s.seek(1, IBStream_::IStreamSeekMode_::kIBSeekSet as i32, &mut pos);
            let mut buf = [0u8; 8];
            s.read(buf.as_mut_ptr() as *mut c_void, 8, &mut n);
            assert_eq!((n, &buf[..4]), (4, &b"ello"[..]));
        }
        assert_eq!(s.bytes(), b"hello");
    }

    #[test]
    fn memory_stream_refuses_huge_seeks_and_writes() {
        let s = MemStream::new(vec![]);
        let (mut pos, mut n) = (0, 0);
        let mut byte = 1u8;
        unsafe {
            assert_eq!(s.seek(1 << 40, IBStream_::IStreamSeekMode_::kIBSeekSet as i32, &mut pos), kInvalidArgument);
            assert_eq!(s.seek(i64::MAX, IBStream_::IStreamSeekMode_::kIBSeekCur as i32, &mut pos), kInvalidArgument);
            s.seek(com::MAX_STREAM as i64, IBStream_::IStreamSeekMode_::kIBSeekSet as i32, &mut pos);
            assert_eq!(s.write(&mut byte as *mut u8 as *mut c_void, 1, &mut n), kResultFalse);
        }
        assert!(s.bytes().is_empty(), "nothing was allocated");
    }

    #[test]
    fn parameter_changes_reuse_queues_without_growing() {
        let pc = ParamChanges::new(2);
        pc.add(7, 0, 0.5);
        pc.add(7, 10, 0.6);
        pc.add(9, 0, 1.0);
        pc.add(11, 0, 1.0); // pool full: dropped
        assert_eq!(pc.queues().len(), 2);
        assert_eq!(pc.queues()[0].last(), Some(0.6));
        pc.clear();
        assert!(pc.queues().is_empty());
    }
}
