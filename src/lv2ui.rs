//! Hosting LV2 plugin GUIs (`ui:X11UI`) in their own X11 windows.
//!
//! libX11 is loaded at runtime. Each GUI gets a top-level window (under
//! Wayland sessions it runs through XWayland), `instance-access` to the running
//! DSP instance, and is driven from the app's UI thread through the
//! `ui:idleInterface`. GUI writes go back to the plugin: control values via
//! the device's shared parameters, atom messages via [`Lv2Link::send_atom`].

use crate::device::DeviceShared;
use crate::lv2::{
    Lv2Feature, Lv2Link, Lv2Option, URI_MAP, URI_OPTIONS, URI_UNMAP, UridMapFeature, UridUnmapFeature, urid, urid_map_cb, urid_unmap_cb,
};
use libloading::Library;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::{Arc, OnceLock};

type P = *mut c_void;
type Display = c_void;
type Window = c_ulong;
type Atom = c_ulong;

/// XEvent is a union padded to 24 longs.
#[repr(C)]
struct XEvent {
    data: [c_long; 24],
}

const STRUCTURE_NOTIFY_MASK: c_long = 1 << 17;
const DESTROY_NOTIFY: c_int = 17;
const CLIENT_MESSAGE: c_int = 33;

macro_rules! x11_api {
    ($($field:ident = $sym:literal : fn($($arg:ty),*) -> $ret:ty;)*) => {
        struct X11 {
            _lib: Library,
            $($field: unsafe extern "C" fn($($arg),*) -> $ret,)*
        }
        impl X11 {
            fn load() -> Result<Self, String> {
                let lib = unsafe { Library::new("libX11.so.6") }.map_err(|e| format!("could not load libX11.so.6: {e}"))?;
                unsafe {
                    Ok(X11 {
                        $($field: *lib.get(concat!($sym, "\0").as_bytes()).map_err(|e| format!("libX11 {}: {e}", $sym))?,)*
                        _lib: lib,
                    })
                }
            }
        }
    };
}

x11_api! {
    init_threads = "XInitThreads": fn() -> c_int;
    open_display = "XOpenDisplay": fn(*const c_char) -> *mut Display;
    close_display = "XCloseDisplay": fn(*mut Display) -> c_int;
    default_screen = "XDefaultScreen": fn(*mut Display) -> c_int;
    root_window = "XRootWindow": fn(*mut Display, c_int) -> Window;
    black_pixel = "XBlackPixel": fn(*mut Display, c_int) -> c_ulong;
    create_simple_window = "XCreateSimpleWindow": fn(*mut Display, Window, c_int, c_int, c_uint, c_uint, c_uint, c_ulong, c_ulong) -> Window;
    store_name = "XStoreName": fn(*mut Display, Window, *const c_char) -> c_int;
    map_raised = "XMapRaised": fn(*mut Display, Window) -> c_int;
    select_input = "XSelectInput": fn(*mut Display, Window, c_long) -> c_int;
    intern_atom = "XInternAtom": fn(*mut Display, *const c_char, c_int) -> Atom;
    set_wm_protocols = "XSetWMProtocols": fn(*mut Display, Window, *mut Atom, c_int) -> c_int;
    pending = "XPending": fn(*mut Display) -> c_int;
    next_event = "XNextEvent": fn(*mut Display, *mut XEvent) -> c_int;
    resize_window = "XResizeWindow": fn(*mut Display, Window, c_uint, c_uint) -> c_int;
    destroy_window = "XDestroyWindow": fn(*mut Display, Window) -> c_int;
    flush = "XFlush": fn(*mut Display) -> c_int;
}

static X11_API: OnceLock<Result<X11, String>> = OnceLock::new();

fn x11() -> Result<&'static X11, String> {
    X11_API.get_or_init(X11::load).as_ref().map_err(|e| e.clone())
}

/// Make Xlib thread-safe. Plugin GUIs (JUCE and others) expect this; it must
/// run before any other Xlib call in the process, so call it from `main`.
pub fn init_x11_threads() {
    if let Ok(x) = x11() {
        unsafe { (x.init_threads)() };
    }
}

// ---------------------------------------------------------------------------
// LV2 UI ABI

type WriteFn = unsafe extern "C" fn(P, u32, u32, u32, *const c_void);

#[repr(C)]
struct UiDescriptor {
    uri: *const c_char,
    instantiate: unsafe extern "C" fn(*const UiDescriptor, *const c_char, *const c_char, WriteFn, P, *mut P, *const *const Lv2Feature) -> P,
    cleanup: unsafe extern "C" fn(P),
    port_event: Option<unsafe extern "C" fn(P, u32, u32, u32, *const c_void)>,
    extension_data: Option<unsafe extern "C" fn(*const c_char) -> *const c_void>,
}

#[repr(C)]
struct IdleInterface {
    idle: unsafe extern "C" fn(P) -> c_int,
}

#[repr(C)]
struct ResizeFeature {
    handle: P,
    ui_resize: unsafe extern "C" fn(P, c_int, c_int) -> c_int,
}

/// Where a GUI's `ui:resize` requests go.
struct ResizeTarget {
    display: *mut Display,
    window: Window,
}

unsafe extern "C" fn ui_resize(handle: P, w: c_int, h: c_int) -> c_int {
    let (Ok(x), Some(t)) = (x11(), unsafe { (handle as *const ResizeTarget).as_ref() }) else { return 1 };
    if w > 0 && h > 0 {
        unsafe {
            (x.resize_window)(t.display, t.window, w as c_uint, h as c_uint);
            (x.flush)(t.display);
        }
    }
    0
}

/// Context passed to the GUI's write function.
struct Controller {
    link: Arc<Lv2Link>,
    shared: Arc<DeviceShared>,
    event_transfer: u32,
}

unsafe extern "C" fn ui_write(controller: P, port: u32, size: u32, protocol: u32, buffer: *const c_void) {
    let Some(c) = (unsafe { (controller as *const Controller).as_ref() }) else { return };
    if buffer.is_null() {
        return;
    }
    if protocol == 0 && size as usize >= 4 {
        // Plain control value.
        if let Some(k) = c.link.control_in_ports.iter().position(|p| *p == port) {
            c.shared.params[k].set(unsafe { *(buffer as *const f32) });
        }
    } else if protocol == c.event_transfer {
        // A whole LV2_Atom for an atom input port (e.g. patch:Set messages).
        let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
        c.link.send_atom(port, bytes);
    }
}

/// Feature array for a GUI instance. Boxed so pointers stay valid.
struct UiFeatures {
    _map: Box<UridMapFeature>,
    _unmap: Box<UridUnmapFeature>,
    _resize: Box<ResizeFeature>,
    _resize_target: Box<ResizeTarget>,
    _opt_values: Box<[f32; 2]>,
    _options: Vec<Lv2Option>,
    _features: Vec<Lv2Feature>,
    ptrs: Vec<*const Lv2Feature>,
}

const URI_PARENT: &CStr = c"http://lv2plug.in/ns/extensions/ui#parent";
const URI_INSTANCE_ACCESS: &CStr = c"http://lv2plug.in/ns/ext/instance-access";
const URI_IDLE: &CStr = c"http://lv2plug.in/ns/extensions/ui#idleInterface";
const URI_RESIZE: &CStr = c"http://lv2plug.in/ns/extensions/ui#resize";

impl UiFeatures {
    fn new(window: Window, display: *mut Display, instance: P, sample_rate: f32) -> Box<Self> {
        let mut map = Box::new(UridMapFeature { handle: std::ptr::null_mut(), map: urid_map_cb });
        let mut unmap = Box::new(UridUnmapFeature { handle: std::ptr::null_mut(), unmap: urid_unmap_cb });
        let mut target = Box::new(ResizeTarget { display, window });
        let mut resize = Box::new(ResizeFeature { handle: &mut *target as *mut ResizeTarget as P, ui_resize });
        let values = Box::new([1.0f32, sample_rate]);
        let t_float = urid("http://lv2plug.in/ns/ext/atom#Float");
        let opt = |key: &str, value: *const f32| Lv2Option { context: 0, subject: 0, key: urid(key), size: 4, type_: t_float, value: value as *const c_void };
        let mut options = vec![
            opt("http://lv2plug.in/ns/extensions/ui#scaleFactor", &values[0]),
            opt("http://lv2plug.in/ns/ext/parameters#sampleRate", &values[1]),
            Lv2Option { context: 0, subject: 0, key: 0, size: 0, type_: 0, value: std::ptr::null() },
        ];
        let features = vec![
            Lv2Feature { uri: URI_MAP.as_ptr(), data: &mut *map as *mut _ as P },
            Lv2Feature { uri: URI_UNMAP.as_ptr(), data: &mut *unmap as *mut _ as P },
            Lv2Feature { uri: URI_PARENT.as_ptr(), data: window as P },
            Lv2Feature { uri: URI_INSTANCE_ACCESS.as_ptr(), data: instance },
            Lv2Feature { uri: URI_IDLE.as_ptr(), data: std::ptr::null_mut() },
            Lv2Feature { uri: URI_RESIZE.as_ptr(), data: &mut *resize as *mut _ as P },
            Lv2Feature { uri: URI_OPTIONS.as_ptr(), data: options.as_mut_ptr() as P },
        ];
        let mut ptrs: Vec<*const Lv2Feature> = features.iter().map(|f| f as *const _).collect();
        ptrs.push(std::ptr::null());
        Box::new(UiFeatures {
            _map: map,
            _unmap: unmap,
            _resize: resize,
            _resize_target: target,
            _opt_values: values,
            _options: options,
            _features: features,
            ptrs,
        })
    }
}

// ---------------------------------------------------------------------------

/// An open plugin GUI window. Must be dropped (closed) before the plugin
/// instance it is attached to is freed.
pub struct PluginWindow {
    /// Identifies the device this GUI belongs to.
    pub shared: Arc<DeviceShared>,
    display: *mut Display,
    window: Window,
    wm_delete: Atom,
    desc: *const UiDescriptor,
    handle: P,
    idle: Option<*const IdleInterface>,
    ctrl: Box<Controller>,
    _features: Box<UiFeatures>,
    /// Last values sent to the GUI: (port, value).
    sent: Vec<(u32, f32)>,
    closed: bool,
}

impl PluginWindow {
    pub fn open(link: Arc<Lv2Link>, shared: Arc<DeviceShared>, title: String, sample_rate: f32) -> Result<Self, String> {
        let ui = link.ui.clone().ok_or("this plugin has no X11 GUI")?;
        let x = x11()?;
        unsafe {
            let display = (x.open_display)(std::ptr::null());
            if display.is_null() {
                return Err("cannot open an X11 display (plugin GUIs need X11 or XWayland; is DISPLAY set?)".into());
            }
            let screen = (x.default_screen)(display);
            let root = (x.root_window)(display, screen);
            let black = (x.black_pixel)(display, screen);
            let window = (x.create_simple_window)(display, root, 0, 0, 800, 600, 0, black, black);
            let name = CString::new(title.clone()).unwrap_or_default();
            (x.store_name)(display, window, name.as_ptr());
            let mut wm_delete = (x.intern_atom)(display, c"WM_DELETE_WINDOW".as_ptr(), 0);
            (x.set_wm_protocols)(display, window, &mut wm_delete, 1);
            (x.select_input)(display, window, STRUCTURE_NOTIFY_MASK);

            let fail = |msg: String| {
                (x.destroy_window)(display, window);
                (x.close_display)(display);
                Err(msg)
            };

            // Plugin GUI code is never unloaded again: many toolkits (JUCE
            // included) do not survive dlclose.
            let lib = match Library::new(&ui.binary) {
                Ok(l) => l,
                Err(e) => return fail(format!("cannot load GUI {}: {e}", ui.binary.display())),
            };
            let get: libloading::Symbol<unsafe extern "C" fn(u32) -> *const UiDescriptor> = match lib.get(b"lv2ui_descriptor\0") {
                Ok(f) => f,
                Err(e) => return fail(format!("GUI has no lv2ui_descriptor: {e}")),
            };
            let mut desc = std::ptr::null();
            for i in 0..64 {
                let d = get(i);
                if d.is_null() {
                    break;
                }
                if CStr::from_ptr((*d).uri).to_string_lossy() == ui.uri {
                    desc = d;
                    break;
                }
            }
            std::mem::forget(lib);
            if desc.is_null() {
                return fail(format!("GUI {} not found in {}", ui.uri, ui.binary.display()));
            }

            let features = UiFeatures::new(window, display, link.handle as P, sample_rate);
            let ctrl = Box::new(Controller {
                link: link.clone(),
                shared: shared.clone(),
                event_transfer: urid("http://lv2plug.in/ns/ext/atom#eventTransfer"),
            });
            let plugin_uri = CString::new(link.plugin_uri.clone()).unwrap_or_default();
            let bundle = CString::new(ui.bundle.clone()).unwrap_or_default();
            let mut widget: P = std::ptr::null_mut();
            let handle = ((*desc).instantiate)(
                desc,
                plugin_uri.as_ptr(),
                bundle.as_ptr(),
                ui_write,
                &*ctrl as *const Controller as P,
                &mut widget,
                features.ptrs.as_ptr(),
            );
            if handle.is_null() {
                return fail("the plugin GUI failed to start".into());
            }
            let idle = (*desc)
                .extension_data
                .map(|ext| ext(URI_IDLE.as_ptr()) as *const IdleInterface)
                .filter(|p| !p.is_null());
            (x.map_raised)(display, window);
            (x.flush)(display);
            Ok(PluginWindow { shared, display, window, wm_delete, desc, handle, idle, ctrl, _features: features, sent: vec![], closed: false })
        }
    }

    /// Process window events, let the GUI run, and send it changed values.
    /// Returns false once the window was closed.
    pub fn pump(&mut self) -> bool {
        let Ok(x) = x11() else { return false };
        if self.closed {
            return false;
        }
        unsafe {
            let mut ev = XEvent { data: [0; 24] };
            while (x.pending)(self.display) > 0 {
                (x.next_event)(self.display, &mut ev);
                let kind = ev.data[0] as c_int;
                // XClientMessageEvent.data.l[0] is the 8th long of the union.
                if (kind == CLIENT_MESSAGE && ev.data[7] as Atom == self.wm_delete) || kind == DESTROY_NOTIFY {
                    self.closed = true;
                }
            }
            if self.closed {
                return false;
            }
            if let Some(idle) = self.idle
                && ((*idle).idle)(self.handle) != 0
            {
                self.closed = true;
                return false;
            }
            self.send_port_values();
            (x.flush)(self.display);
        }
        true
    }

    /// Notify the GUI about control values that changed elsewhere (generic
    /// sliders, plugin outputs such as meters).
    fn send_port_values(&mut self) {
        let Some(port_event) = (unsafe { (*self.desc).port_event }) else { return };
        let link = &self.ctrl.link;
        let ins = link.control_in_ports.iter().zip(&self.shared.params);
        let outs = link.control_out_ports.iter().zip(&self.shared.outputs);
        for (port, value) in ins.chain(outs) {
            let v = value.get();
            match self.sent.iter_mut().find(|(p, _)| p == port) {
                Some((_, last)) if *last == v => continue,
                Some((_, last)) => *last = v,
                None => self.sent.push((*port, v)),
            }
            unsafe { port_event(self.handle, *port, 4, 0, &v as *const f32 as *const c_void) };
        }
    }
}

impl Drop for PluginWindow {
    fn drop(&mut self) {
        let Ok(x) = x11() else { return };
        unsafe {
            ((*self.desc).cleanup)(self.handle);
            (x.destroy_window)(self.display, self.window);
            (x.flush)(self.display);
            (x.close_display)(self.display);
        }
    }
}
