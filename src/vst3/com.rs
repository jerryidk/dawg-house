//! COM objects the host implements for VST3 plugins.
//!
//! * [`HostApp`]: `IHostApplication`, which also creates [`Message`]s and
//!   [`AttributeList`]s for component ↔ controller messaging.
//! * [`MemStream`]: `IBStream` over a byte vector (plugin state).
//! * [`Handler`]: `IComponentHandler`; GUI edits go to the device's shared
//!   parameters, which the audio thread forwards to the processor.
//! * [`ParamChanges`] / [`ParamQueue`] and [`EventList`]: per-block
//!   parameter changes and note events. Preallocated, so filling them on the
//!   audio thread never allocates.
//!
//! The objects use interior mutability through `&self` (COM methods take
//! `this` by shared reference). Each one is used from one thread at a time:
//! the plugin thread or, for the per-block lists, the audio thread.

use super::ParamSync;
use std::cell::{Cell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper, Interface};

fn iid_eq(tuid: *const TUID, guid: &[u8; 16]) -> bool {
    !tuid.is_null() && unsafe { std::slice::from_raw_parts(tuid as *const u8, 16) } == guid
}

/// Store an owned interface pointer in `out`.
fn hand_out<I: Interface>(ptr: Option<vst3::ComPtr<I>>, out: *mut *mut c_void) -> tresult {
    match ptr {
        Some(p) => {
            unsafe { *out = p.into_raw() as *mut c_void };
            kResultOk
        }
        None => kNoInterface,
    }
}

pub fn write_wstring(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (d, s) in dst.iter_mut().zip(src.encode_utf16()) {
        *d = s as TChar;
        n += 1;
    }
    if let Some(end) = dst.get_mut(n.min(dst.len().saturating_sub(1))) {
        *end = 0;
    }
}

pub fn read_wstring(src: &[TChar]) -> String {
    let len = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    String::from_utf16_lossy(&src[..len])
}

pub fn read_cstring(src: &[c_char]) -> String {
    let bytes: Vec<u8> = src.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---------------------------------------------------------------------------
// Host application, messages, attributes

pub struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        match unsafe { name.as_mut() } {
            Some(n) => {
                write_wstring("Studio", n);
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        if obj.is_null() {
            return kInvalidArgument;
        }
        if iid_eq(cid, &IMessage::IID) && iid_eq(iid, &IMessage::IID) {
            return hand_out(ComWrapper::new(Message::new()).to_com_ptr::<IMessage>(), obj);
        }
        if iid_eq(cid, &IAttributeList::IID) && iid_eq(iid, &IAttributeList::IID) {
            return hand_out(ComWrapper::new(AttributeList::default()).to_com_ptr::<IAttributeList>(), obj);
        }
        unsafe { *obj = std::ptr::null_mut() };
        kResultFalse
    }
}

pub struct Message {
    id: Mutex<Option<CString>>,
    attributes: ComWrapper<AttributeList>,
}

impl Message {
    fn new() -> Self {
        Message { id: Mutex::new(None), attributes: ComWrapper::new(AttributeList::default()) }
    }
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.lock().ok().and_then(|id| id.as_ref().map(|s| s.as_ptr())).unwrap_or(std::ptr::null())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let value = (!id.is_null()).then(|| unsafe { CStr::from_ptr(id) }.to_owned());
        if let Ok(mut slot) = self.id.lock() {
            *slot = value;
        }
    }

    /// Not add-ref'ed: the list lives as long as the message (SDK convention).
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attributes.as_com_ref::<IAttributeList>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

enum Attr {
    Int(i64),
    Float(f64),
    /// UTF-16 including the terminating zero.
    Text(Vec<TChar>),
    Binary(Vec<u8>),
}

#[derive(Default)]
pub struct AttributeList {
    values: Mutex<HashMap<CString, Attr>>,
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

impl AttributeList {
    fn set(&self, id: AttrID, v: Attr) -> tresult {
        if id.is_null() {
            return kInvalidArgument;
        }
        let key = unsafe { CStr::from_ptr(id) }.to_owned();
        match self.values.lock() {
            Ok(mut m) => {
                m.insert(key, v);
                kResultOk
            }
            Err(_) => kResultFalse,
        }
    }

    fn with<R>(&self, id: AttrID, f: impl FnOnce(&Attr) -> Option<R>) -> Option<R> {
        if id.is_null() {
            return None;
        }
        let m = self.values.lock().ok()?;
        m.get(unsafe { CStr::from_ptr(id) }).and_then(f)
    }
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.set(id, Attr::Int(value))
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match self.with(id, |a| if let Attr::Int(v) = a { Some(*v) } else { None }) {
            Some(v) if !value.is_null() => {
                unsafe { *value = v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.set(id, Attr::Float(value))
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match self.with(id, |a| if let Attr::Float(v) = a { Some(*v) } else { None }) {
            Some(v) if !value.is_null() => {
                unsafe { *value = v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut v = vec![];
        let mut p = string;
        loop {
            let c = unsafe { *p };
            v.push(c);
            if c == 0 {
                break;
            }
            p = unsafe { p.add(1) };
        }
        self.set(id, Attr::Text(v))
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let cap = size_in_bytes as usize / std::mem::size_of::<TChar>();
        if string.is_null() || cap == 0 {
            return kInvalidArgument;
        }
        let copied = self.with(id, |a| {
            let Attr::Text(t) = a else { return None };
            let n = t.len().min(cap);
            unsafe {
                std::ptr::copy_nonoverlapping(t.as_ptr(), string, n);
                *string.add(n.min(cap - 1)) = 0;
            }
            Some(())
        });
        if copied.is_some() { kResultOk } else { kResultFalse }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let bytes = if data.is_null() { vec![] } else { unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }.to_vec() };
        self.set(id, Attr::Binary(bytes))
    }

    /// The pointer stays valid while the list lives and the value is not replaced.
    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        if data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        let found = self.with(id, |a| if let Attr::Binary(b) = a { Some((b.as_ptr(), b.len())) } else { None });
        match found {
            Some((p, n)) => {
                unsafe {
                    *data = p as *const c_void;
                    *size_in_bytes = n as u32;
                }
                kResultOk
            }
            None => kResultFalse,
        }
    }
}

// ---------------------------------------------------------------------------
// Streams

/// Largest stream a plugin can make Studio hold: state is far smaller, but
/// a plugin may seek anywhere and then write.
pub const MAX_STREAM: usize = 256 << 20;

/// `IBStream` over memory: reading saved state or collecting new state.
pub struct MemStream {
    data: Mutex<(Vec<u8>, usize)>,
}

impl Class for MemStream {
    type Interfaces = (IBStream,);
}

impl MemStream {
    pub fn new(data: Vec<u8>) -> ComWrapper<MemStream> {
        ComWrapper::new(MemStream { data: Mutex::new((data, 0)) })
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.data.lock().map(|d| d.0.clone()).unwrap_or_default()
    }
}

impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        let Ok(mut d) = self.data.lock() else { return kResultFalse };
        let (data, pos) = &mut *d;
        let n = (num_bytes.max(0) as usize).min(data.len().saturating_sub(*pos));
        if n > 0 && !buffer.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(*pos), buffer as *mut u8, n) };
        }
        *pos += n;
        if !num_read.is_null() {
            unsafe { *num_read = n as int32 };
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_written: *mut int32) -> tresult {
        let Ok(mut d) = self.data.lock() else { return kResultFalse };
        let (data, pos) = &mut *d;
        let n = num_bytes.max(0) as usize;
        if n > 0 && buffer.is_null() {
            return kInvalidArgument;
        }
        if pos.saturating_add(n) > MAX_STREAM {
            return kResultFalse;
        }
        if data.len() < *pos + n {
            data.resize(*pos + n, 0);
        }
        if n > 0 {
            unsafe { std::ptr::copy_nonoverlapping(buffer as *const u8, data.as_mut_ptr().add(*pos), n) };
        }
        *pos += n;
        if !num_written.is_null() {
            unsafe { *num_written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let Ok(mut d) = self.data.lock() else { return kResultFalse };
        // The enum's integer type differs between platforms.
        use IBStream_::IStreamSeekMode_::*;
        let mode = mode as i64;
        let base = if mode == kIBSeekSet as i64 {
            0
        } else if mode == kIBSeekCur as i64 {
            d.1 as i64
        } else if mode == kIBSeekEnd as i64 {
            d.0.len() as i64
        } else {
            return kInvalidArgument;
        };
        let target = base.saturating_add(pos);
        if !(0..=MAX_STREAM as i64).contains(&target) {
            return kInvalidArgument;
        }
        d.1 = target as usize;
        if !result.is_null() {
            unsafe { *result = target };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        let Ok(d) = self.data.lock() else { return kResultFalse };
        if pos.is_null() {
            return kInvalidArgument;
        }
        unsafe { *pos = d.1 as int64 };
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// Component handler

/// Receives edits made in the plugin's own GUI (on the plugin thread).
pub struct Handler {
    pub sync: Arc<ParamSync>,
    /// `RestartFlags` requested by the plugin, handled by the plugin thread.
    pub restart: AtomicI32,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.sync.set_from_controller(id, value);
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.restart.fetch_or(flags, Ordering::Relaxed);
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// Per-block parameter changes and events

const MAX_POINTS: usize = 64;

pub struct ParamQueue {
    id: Cell<ParamID>,
    points: UnsafeCell<Vec<(i32, f64)>>,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl ParamQueue {
    fn new() -> Self {
        ParamQueue { id: Cell::new(0), points: UnsafeCell::new(Vec::with_capacity(MAX_POINTS)) }
    }

    fn with_points<R>(&self, f: impl FnOnce(&mut Vec<(i32, f64)>) -> R) -> R {
        // SAFETY: one thread uses a queue at a time and `f` cannot reach
        // the queue again, so this is the only reference.
        f(unsafe { &mut *self.points.get() })
    }

    /// Value of the last point.
    pub fn last(&self) -> Option<f64> {
        self.with_points(|p| p.last().map(|p| p.1))
    }

    pub fn id(&self) -> ParamID {
        self.id.get()
    }
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.with_points(|p| p.len() as int32)
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        match usize::try_from(index).ok().and_then(|i| self.with_points(|p| p.get(i).copied())) {
            Some((o, v)) if !offset.is_null() && !value.is_null() => {
                unsafe {
                    *offset = o;
                    *value = v;
                }
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        let Some(i) = self.with_points(|p| {
            (p.len() < MAX_POINTS).then(|| {
                p.push((offset, value));
                p.len() - 1
            })
        }) else {
            return kResultFalse;
        };
        if !index.is_null() {
            unsafe { *index = i as int32 };
        }
        kResultOk
    }
}

/// `IParameterChanges` with a fixed pool of queues.
pub struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    used: Cell<usize>,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl ParamChanges {
    pub fn new(capacity: usize) -> ComWrapper<ParamChanges> {
        ComWrapper::new(ParamChanges { queues: (0..capacity.max(1)).map(|_| ComWrapper::new(ParamQueue::new())).collect(), used: Cell::new(0) })
    }

    pub fn clear(&self) {
        self.used.set(0);
    }

    /// Queues filled this block.
    pub fn queues(&self) -> &[ComWrapper<ParamQueue>] {
        &self.queues[..self.used.get()]
    }

    /// Add a point for `id` (host side); dropped when the pool is full.
    pub fn add(&self, id: ParamID, offset: i32, value: f64) {
        if let Some(i) = self.queue_index(id) {
            self.queues[i].with_points(|p| {
                if p.len() < MAX_POINTS {
                    p.push((offset, value));
                }
            });
        }
    }

    /// The queue of `id`, taking a free one if it has none yet.
    fn queue_index(&self, id: ParamID) -> Option<usize> {
        let used = self.used.get();
        if let Some(i) = self.queues[..used].iter().position(|q| q.id.get() == id) {
            return Some(i);
        }
        let q = self.queues.get(used)?;
        q.id.set(id);
        q.with_points(|p| p.clear());
        self.used.set(used + 1);
        Some(used)
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        match usize::try_from(index).ok().filter(|&i| i < self.used.get()) {
            Some(i) => self.queues[i].as_com_ref::<IParamValueQueue>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        let Some(i) = self.queue_index(unsafe { *id }) else { return std::ptr::null_mut() };
        if !index.is_null() {
            unsafe { *index = i as int32 };
        }
        self.queues[i].as_com_ref::<IParamValueQueue>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

const MAX_EVENTS: usize = 1024;

pub struct EventList {
    events: UnsafeCell<Vec<Event>>,
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl EventList {
    pub fn new() -> ComWrapper<EventList> {
        ComWrapper::new(EventList { events: UnsafeCell::new(Vec::with_capacity(MAX_EVENTS)) })
    }

    fn with_events<R>(&self, f: impl FnOnce(&mut Vec<Event>) -> R) -> R {
        // SAFETY: as for `ParamQueue::with_points`.
        f(unsafe { &mut *self.events.get() })
    }

    pub fn clear(&self) {
        self.with_events(|e| e.clear());
    }

    /// Add an event; dropped when the list is full.
    pub fn push(&self, e: Event) -> bool {
        self.with_events(|events| {
            let room = events.len() < MAX_EVENTS;
            if room {
                events.push(e);
            }
            room
        })
    }
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        self.with_events(|e| e.len() as int32)
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        let copied = self.with_events(|events| match usize::try_from(index).ok().and_then(|i| events.get(i)) {
            Some(ev) => {
                unsafe { std::ptr::copy_nonoverlapping(ev, e, 1) };
                true
            }
            None => false,
        });
        if copied { kResultOk } else { kInvalidArgument }
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        if self.push(unsafe { std::ptr::read(e) }) { kResultOk } else { kResultFalse }
    }
}
