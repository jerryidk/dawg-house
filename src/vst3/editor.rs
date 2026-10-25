//! VST3 editor windows. Studio makes a top-level window the plugin's view
//! attaches to (an X11 window on Linux, an HWND on Windows) and gives the
//! view an `IPlugFrame` to resize it through. On Linux the frame is also the
//! `IRunLoop` that drives the GUI's timers and file descriptors (JUCE and
//! VSTGUI need it). Everything here runs on the plugin thread.

#[cfg(not(any(target_os = "linux", windows)))]
use std::ffi::c_void;
use std::rc::Rc;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};

/// An open editor: the plugin's view in its window.
pub struct Editor {
    view: ComPtr<IPlugView>,
    _frame: ComWrapper<Frame>,
    window: Rc<NativeWindow>,
}

impl Editor {
    pub fn open(view: ComPtr<IPlugView>, title: &str) -> Result<Editor, String> {
        unsafe {
            if view.isPlatformTypeSupported(PLATFORM) != kResultOk {
                return Err("the editor does not support this platform's windows".into());
            }
            let mut rect = ViewRect { left: 0, top: 0, right: 640, bottom: 480 };
            view.getSize(&mut rect);
            let (w, h) = ((rect.right - rect.left).max(64), (rect.bottom - rect.top).max(64));
            let window = Rc::new(NativeWindow::open(title, w, h)?);
            let frame = ComWrapper::new(Frame { window: window.clone() });
            let frame_ptr = frame.as_com_ref::<IPlugFrame>().map_or(std::ptr::null_mut(), |r| r.as_ptr());
            view.setFrame(frame_ptr);
            if view.attached(window.handle(), PLATFORM) != kResultOk {
                view.setFrame(std::ptr::null_mut());
                return Err("the editor could not attach to its window".into());
            }
            // Some views only know their size once attached.
            if view.getSize(&mut rect) == kResultOk {
                window.resize(rect.right - rect.left, rect.bottom - rect.top);
            }
            Ok(Editor { view, _frame: frame, window })
        }
    }

    /// Handle window events; false once the user closed the window.
    pub fn pump(&mut self) -> bool {
        self.window.pump()
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            self.view.removed();
            self.view.setFrame(std::ptr::null_mut());
        }
    }
}

/// `IPlugFrame` (and `IRunLoop` on Linux) for one editor.
struct Frame {
    window: Rc<NativeWindow>,
}

#[cfg(target_os = "linux")]
impl Class for Frame {
    type Interfaces = (IPlugFrame, Linux::IRunLoop);
}

#[cfg(not(target_os = "linux"))]
impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, size: *mut ViewRect) -> tresult {
        let (Some(view), Some(rect)) = (unsafe { vst3::ComRef::from_raw(view) }, unsafe { size.as_mut() }) else { return kInvalidArgument };
        self.window.resize(rect.right - rect.left, rect.bottom - rect.top);
        unsafe { view.onSize(rect) };
        kResultOk
    }
}

/// Run the platform's event handling for every editor: X11 GUIs' timers
/// and descriptors (Linux) or the thread's window messages (Windows).
pub fn pump_os() {
    #[cfg(target_os = "linux")]
    run_loop::pump();
    #[cfg(windows)]
    win::pump_messages();
}

// ---------------------------------------------------------------------------
// Linux: X11 window and run loop

#[cfg(target_os = "linux")]
const PLATFORM: FIDString = kPlatformTypeX11EmbedWindowID;
#[cfg(windows)]
const PLATFORM: FIDString = kPlatformTypeHWND;
#[cfg(not(any(target_os = "linux", windows)))]
const PLATFORM: FIDString = kPlatformTypeNSView;

#[cfg(target_os = "linux")]
use x11_window::NativeWindow;

#[cfg(target_os = "linux")]
mod x11_window {
    use crate::lv2ui::{Atom, CLIENT_MESSAGE, DESTROY_NOTIFY, Display, STRUCTURE_NOTIFY_MASK, Window, XEvent, x11};
    use std::cell::Cell;
    use std::ffi::{CString, c_int, c_void};

    pub struct NativeWindow {
        display: *mut Display,
        window: Window,
        wm_delete: Atom,
        closed: Cell<bool>,
    }

    impl NativeWindow {
        pub fn open(title: &str, w: i32, h: i32) -> Result<Self, String> {
            let x = x11()?;
            unsafe {
                let display = (x.open_display)(std::ptr::null());
                if display.is_null() {
                    return Err("cannot open an X11 display (plugin GUIs need X11 or XWayland; is DISPLAY set?)".into());
                }
                let screen = (x.default_screen)(display);
                let root = (x.root_window)(display, screen);
                let black = (x.black_pixel)(display, screen);
                let window = (x.create_simple_window)(display, root, 0, 0, w as u32, h as u32, 0, black, black);
                // XStoreName takes Latin-1.
                let ascii: String = title.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect();
                let name = CString::new(ascii).unwrap_or_default();
                (x.store_name)(display, window, name.as_ptr());
                let mut wm_delete = (x.intern_atom)(display, c"WM_DELETE_WINDOW".as_ptr(), 0);
                (x.set_wm_protocols)(display, window, &mut wm_delete, 1);
                (x.select_input)(display, window, STRUCTURE_NOTIFY_MASK);
                (x.map_raised)(display, window);
                (x.flush)(display);
                Ok(NativeWindow { display, window, wm_delete, closed: Cell::new(false) })
            }
        }

        pub fn handle(&self) -> *mut c_void {
            self.window as usize as *mut c_void
        }

        pub fn resize(&self, w: i32, h: i32) {
            let Ok(x) = x11() else { return };
            unsafe {
                (x.resize_window)(self.display, self.window, w.max(1) as u32, h.max(1) as u32);
                (x.flush)(self.display);
            }
        }

        pub fn pump(&self) -> bool {
            let Ok(x) = x11() else { return false };
            unsafe {
                let mut ev = XEvent { data: [0; 24] };
                while (x.pending)(self.display) > 0 {
                    (x.next_event)(self.display, &mut ev);
                    let kind = ev.data[0] as c_int;
                    // XClientMessageEvent.data.l[0] is the 8th long of the union.
                    if (kind == CLIENT_MESSAGE && ev.data[7] as Atom == self.wm_delete) || kind == DESTROY_NOTIFY {
                        self.closed.set(true);
                    }
                }
            }
            !self.closed.get()
        }
    }

    impl Drop for NativeWindow {
        fn drop(&mut self) {
            let Ok(x) = x11() else { return };
            unsafe {
                (x.destroy_window)(self.display, self.window);
                (x.flush)(self.display);
                (x.close_display)(self.display);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod run_loop {
    use super::Frame;
    use std::cell::RefCell;
    use std::time::{Duration, Instant};
    use vst3::Steinberg::Linux::*;
    use vst3::Steinberg::*;
    use vst3::{ComPtr, ComRef};

    struct Timer {
        handler: ComPtr<ITimerHandler>,
        every: Duration,
        next: Instant,
    }

    #[derive(Default)]
    struct State {
        fds: Vec<(FileDescriptor, ComPtr<IEventHandler>)>,
        timers: Vec<Timer>,
    }

    thread_local! {
        static STATE: RefCell<State> = RefCell::default();
    }

    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    const POLLIN: i16 = 1;

    unsafe extern "C" {
        fn poll(fds: *mut PollFd, n: std::ffi::c_ulong, timeout: i32) -> i32;
    }

    /// Call handlers whose descriptor is readable and timers that are due.
    /// Handlers may (un)register others, so they run on a snapshot.
    pub fn pump() {
        let fds: Vec<(FileDescriptor, ComPtr<IEventHandler>)> = STATE.with(|s| s.borrow().fds.clone());
        if !fds.is_empty() {
            let mut polled: Vec<PollFd> = fds.iter().map(|(fd, _)| PollFd { fd: *fd, events: POLLIN, revents: 0 }).collect();
            if unsafe { poll(polled.as_mut_ptr(), polled.len() as _, 0) } > 0 {
                for (p, (fd, h)) in polled.iter().zip(&fds) {
                    if p.revents != 0 {
                        unsafe { h.onFDIsSet(*fd) };
                    }
                }
            }
        }
        let now = Instant::now();
        let due: Vec<ComPtr<ITimerHandler>> = STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.timers
                .iter_mut()
                .filter(|t| t.next <= now)
                .map(|t| {
                    t.next = now + t.every;
                    t.handler.clone()
                })
                .collect()
        });
        for h in due {
            unsafe { h.onTimer() };
        }
    }

    impl IRunLoopTrait for Frame {
        unsafe fn registerEventHandler(&self, handler: *mut IEventHandler, fd: FileDescriptor) -> tresult {
            let Some(h) = (unsafe { ComRef::from_raw(handler) }) else { return kInvalidArgument };
            STATE.with(|s| s.borrow_mut().fds.push((fd, h.to_com_ptr())));
            kResultOk
        }

        unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
            STATE.with(|s| s.borrow_mut().fds.retain(|(_, h)| h.as_ptr() != handler));
            kResultOk
        }

        unsafe fn registerTimer(&self, handler: *mut ITimerHandler, ms: TimerInterval) -> tresult {
            let Some(h) = (unsafe { ComRef::from_raw(handler) }) else { return kInvalidArgument };
            let every = Duration::from_millis(ms.max(1));
            STATE.with(|s| s.borrow_mut().timers.push(Timer { handler: h.to_com_ptr(), every, next: Instant::now() + every }));
            kResultOk
        }

        unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
            STATE.with(|s| s.borrow_mut().timers.retain(|t| t.handler.as_ptr() != handler));
            kResultOk
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: Win32 window

#[cfg(windows)]
use win::NativeWindow;

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::ffi::c_void;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{PCWSTR, w};

    const CLASS: PCWSTR = w!("StudioPluginEditor");
    const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);

    thread_local! {
        /// Windows the user asked to close.
        static CLOSED: RefCell<HashSet<isize>> = RefCell::default();
        static REGISTERED: RefCell<bool> = const { RefCell::new(false) };
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_CLOSE {
            CLOSED.with(|c| c.borrow_mut().insert(hwnd.0 as isize));
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    pub fn pump_messages() {
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub struct NativeWindow {
        hwnd: HWND,
    }

    fn outer_size(w: i32, h: i32) -> (i32, i32) {
        let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
        let _ = unsafe { AdjustWindowRectEx(&mut r, STYLE, false, WINDOW_EX_STYLE(0)) };
        (r.right - r.left, r.bottom - r.top)
    }

    impl NativeWindow {
        pub fn open(title: &str, w: i32, h: i32) -> Result<Self, String> {
            unsafe {
                let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
                if !REGISTERED.with(|r| *r.borrow()) {
                    let class = WNDCLASSEXW {
                        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                        lpfnWndProc: Some(wndproc),
                        hInstance: instance.into(),
                        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                        lpszClassName: CLASS,
                        ..Default::default()
                    };
                    RegisterClassExW(&class);
                    REGISTERED.with(|r| *r.borrow_mut() = true);
                }
                let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
                let (ow, oh) = outer_size(w, h);
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    CLASS,
                    PCWSTR(title.as_ptr()),
                    STYLE,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    ow,
                    oh,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
                .map_err(|e| format!("cannot create the editor window: {e}"))?;
                let _ = ShowWindow(hwnd, SW_SHOW);
                Ok(NativeWindow { hwnd })
            }
        }

        pub fn handle(&self) -> *mut c_void {
            self.hwnd.0
        }

        pub fn resize(&self, w: i32, h: i32) {
            let (ow, oh) = outer_size(w, h);
            let _ = unsafe { SetWindowPos(self.hwnd, None, 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
        }

        pub fn pump(&self) -> bool {
            !CLOSED.with(|c| c.borrow().contains(&(self.hwnd.0 as isize)))
        }
    }

    impl Drop for NativeWindow {
        fn drop(&mut self) {
            CLOSED.with(|c| c.borrow_mut().remove(&(self.hwnd.0 as isize)));
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
struct NativeWindow;

#[cfg(not(any(target_os = "linux", windows)))]
impl NativeWindow {
    fn open(_: &str, _: i32, _: i32) -> Result<Self, String> {
        Err("plugin editors are not supported on this platform".into())
    }
    fn handle(&self) -> *mut c_void {
        std::ptr::null_mut()
    }
    fn resize(&self, _: i32, _: i32) {}
    fn pump(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::ffi::c_void;

    /// A minimal plugin view: records what the host does to it.
    struct TestView {
        attached: Cell<bool>,
        frame: Cell<*mut IPlugFrame>,
        size: Cell<(i32, i32)>,
    }

    impl Class for TestView {
        type Interfaces = (IPlugView,);
    }

    impl IPlugViewTrait for TestView {
        unsafe fn isPlatformTypeSupported(&self, _: FIDString) -> tresult {
            kResultOk
        }
        unsafe fn attached(&self, parent: *mut c_void, _: FIDString) -> tresult {
            assert!(!parent.is_null(), "a native parent window");
            self.attached.set(true);
            kResultOk
        }
        unsafe fn removed(&self) -> tresult {
            self.attached.set(false);
            kResultOk
        }
        unsafe fn onWheel(&self, _: f32) -> tresult {
            kResultFalse
        }
        unsafe fn onKeyDown(&self, _: char16, _: int16, _: int16) -> tresult {
            kResultFalse
        }
        unsafe fn onKeyUp(&self, _: char16, _: int16, _: int16) -> tresult {
            kResultFalse
        }
        unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
            let (w, h) = self.size.get();
            unsafe { *size = ViewRect { left: 0, top: 0, right: w, bottom: h } };
            kResultOk
        }
        unsafe fn onSize(&self, r: *mut ViewRect) -> tresult {
            let r = unsafe { &*r };
            self.size.set((r.right - r.left, r.bottom - r.top));
            kResultOk
        }
        unsafe fn onFocus(&self, _: TBool) -> tresult {
            kResultOk
        }
        unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
            self.frame.set(frame);
            kResultOk
        }
        unsafe fn canResize(&self) -> tresult {
            kResultFalse
        }
        unsafe fn checkSizeConstraint(&self, _: *mut ViewRect) -> tresult {
            kResultOk
        }
    }

    /// Window, frame and resize path with a fake view. Needs a display
    /// (X11 on Linux; Wine provides one for the Windows build):
    ///   cargo test editor_window -- --ignored
    #[test]
    #[ignore]
    fn editor_window_attach_resize_close() {
        let view = ComWrapper::new(TestView { attached: Cell::new(false), frame: Cell::new(std::ptr::null_mut()), size: Cell::new((320, 200)) });
        let ptr = view.to_com_ptr::<IPlugView>().unwrap();
        let view_ptr = ptr.as_ptr();
        let mut editor = Editor::open(ptr, "Studio editor test").unwrap();
        assert!(view.attached.get());
        let frame = unsafe { vst3::ComRef::from_raw(view.frame.get()) }.expect("the view got a frame");
        let mut rect = ViewRect { left: 0, top: 0, right: 500, bottom: 300 };
        assert_eq!(unsafe { frame.resizeView(view_ptr, &mut rect) }, kResultOk);
        assert_eq!(view.size.get(), (500, 300), "the view is told its new size");
        for _ in 0..5 {
            pump_os();
            assert!(editor.pump(), "nobody closed the window");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(editor);
        assert!(!view.attached.get(), "the view is removed before its window goes");
        assert!(view.frame.get().is_null());
    }
}
