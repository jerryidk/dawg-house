//! Device abstraction (in the spirit of Bitwig's device chain).
//!
//! Everything that sits in a track's chain — LV2 plugins, built-in
//! instruments and effects — implements [`Device`]. The engine only knows
//! about this trait, so new device kinds (CLAP, built-in modulators, ...) can
//! be added by implementing it and registering a factory in
//! [`DeviceRegistry`].

use crate::builtin;
use crate::lv2::{Lv2Host, Lv2Link, StateEntry};
use crate::util::AtomicF32;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// A short MIDI message with its frame offset inside the current block.
#[derive(Clone, Copy, Debug, Default)]
pub struct MidiEvent {
    pub frame: u32,
    pub len: u8,
    pub data: [u8; 3],
}

impl MidiEvent {
    pub fn new(frame: u32, data: &[u8]) -> Self {
        let mut d = [0u8; 3];
        let len = data.len().min(3);
        d[..len].copy_from_slice(&data[..len]);
        MidiEvent { frame, len: len as u8, data: d }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

/// Everything a device sees for one block. Audio is processed in place.
pub struct ProcessContext<'a> {
    pub left: &'a mut [f32],
    pub right: &'a mut [f32],
    /// Events sorted by frame.
    pub midi: &'a [MidiEvent],
}

pub trait Device: Send {
    /// Process one block. Must be real-time safe (no allocation, no locks).
    fn process(&mut self, ctx: &mut ProcessContext);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceCategory {
    Instrument,
    Effect,
}

/// Description of an automatable parameter (or read-only output value).
#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub symbol: String,
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub toggled: bool,
    pub integer: bool,
    pub logarithmic: bool,
    pub scale_points: Vec<(String, f32)>,
}

impl ParamInfo {
    pub fn new(symbol: &str, name: &str, min: f32, max: f32, default: f32) -> Self {
        ParamInfo {
            symbol: symbol.into(),
            name: name.into(),
            min,
            max,
            default,
            toggled: false,
            integer: false,
            logarithmic: false,
            scale_points: vec![],
        }
    }
    pub fn log(mut self) -> Self {
        self.logarithmic = true;
        self
    }
    pub fn choices(mut self, labels: &[&str]) -> Self {
        self.integer = true;
        self.scale_points = labels.iter().enumerate().map(|(i, l)| (l.to_string(), i as f32)).collect();
        self
    }
}

/// Static description of a device type, used by the browser and the UI.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// Stable identifier saved in projects: `lv2:<uri>` or `builtin:<name>`.
    pub id: String,
    pub name: String,
    pub category: DeviceCategory,
    /// Free-form sub-category (e.g. LV2 class label).
    pub class: String,
    pub params: Vec<ParamInfo>,
    pub outputs: Vec<ParamInfo>,
    /// `None` when the device can be created, otherwise why not.
    pub unsupported: Option<String>,
    /// Ships its own GUI (opened in a separate window).
    pub has_gui: bool,
}

/// Values shared between the UI and the device running on the audio thread.
pub struct DeviceShared {
    pub params: Vec<AtomicF32>,
    pub outputs: Vec<AtomicF32>,
    pub bypass: AtomicBool,
}

impl DeviceShared {
    pub fn new(info: &DeviceInfo) -> Arc<Self> {
        Arc::new(DeviceShared {
            params: info.params.iter().map(|p| AtomicF32::new(p.default)).collect(),
            outputs: info.outputs.iter().map(|_| AtomicF32::new(0.0)).collect(),
            bypass: AtomicBool::new(false),
        })
    }
}

/// Catalogue of every device the host can create.
pub struct DeviceRegistry {
    pub devices: Vec<DeviceInfo>,
    lv2: Lv2Host,
}

impl DeviceRegistry {
    pub fn new(sample_rate: f32) -> Self {
        let lv2 = Lv2Host::new(sample_rate);
        let mut devices = builtin::catalogue();
        devices.extend(lv2.plugins.iter().map(|p| p.info.clone()));
        DeviceRegistry { devices, lv2 }
    }

    pub fn lv2_error(&self) -> Option<&str> {
        self.lv2.error.as_deref()
    }

    pub fn get(&self, id: &str) -> Option<&DeviceInfo> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// Instantiate a device, restoring saved plugin `state` if given.
    /// Runs on the UI thread.
    pub fn create(&self, id: &str, sample_rate: f64, shared: Arc<DeviceShared>, state: Option<&[StateEntry]>) -> Result<Created, String> {
        if let Some(name) = id.strip_prefix("builtin:") {
            let device = builtin::create(name, sample_rate as f32, shared).ok_or_else(|| format!("unknown device {id}"))?;
            return Ok(Created { device, lv2: None });
        }
        if let Some(uri) = id.strip_prefix("lv2:") {
            let idx = self.lv2.find(uri).ok_or_else(|| format!("LV2 plugin not installed: {uri}"))?;
            let (runtime, link) = self.lv2.instantiate(idx, sample_rate, shared, state)?;
            return Ok(Created { device: Box::new(runtime), lv2: Some(link) });
        }
        Err(format!("unknown device {id}"))
    }
}

/// A freshly created device. LV2 plugins also return a link used for their
/// GUI and state, which stays on the UI thread.
pub struct Created {
    pub device: Box<dyn Device>,
    pub lv2: Option<Arc<Lv2Link>>,
}
