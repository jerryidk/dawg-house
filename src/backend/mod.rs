//! Audio driver abstraction.
//!
//! The real-time mixer ([`crate::engine::Processor`]) does not know which
//! driver it runs on: each backend calls `Processor::process` from its audio
//! callback with a [`CycleIo`] giving access to that cycle's buffers. The UI
//! side talks to the backend through the [`Backend`] trait (inputs, outputs,
//! buffer size).
//!
//! * Linux: JACK — jackd or PipeWire's JACK emulation (`jack_backend`).
//! * Windows: cpal — WASAPI, or ASIO with the `asio` cargo feature
//!   (`cpal_backend`), built on the platform-neutral [`devio`] layer.
//! * Anywhere: [`NullBackend`] when no audio driver is available.

#[cfg(windows)]
mod cpal_backend;
// Used by the Windows backend; compiled (and unit-tested) everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
pub mod devio;
#[cfg(target_os = "linux")]
mod jack_backend;

use crate::engine::{EngineShared, Processor, TrackShared};
use crate::project::TrackKind;
use std::sync::Arc;

/// Source name meaning "every available source" (used for MIDI inputs).
pub const ALL_SOURCES: &str = "*";

/// Per-track input endpoints owned by the audio thread.
pub enum TrackPorts {
    /// No inputs (offline).
    None,
    /// JACK ports registered for this track.
    #[cfg(target_os = "linux")]
    Jack {
        audio: Option<[::jack::Port<::jack::AudioIn>; 2]>,
        midi: Option<::jack::Port<::jack::MidiIn>>,
    },
    /// Device channels / MIDI ports selected in the track's [`TrackShared`].
    #[cfg_attr(not(windows), allow(dead_code))]
    Device,
}

/// One audio cycle's buffers, as seen by the processor.
pub trait CycleIo {
    /// Frames in this cycle.
    fn frames(&self) -> usize;
    /// Audio input of a track: (left, right), each at least `frames()` long.
    fn track_audio<'a>(&'a self, ports: &'a TrackPorts, shared: &TrackShared) -> Option<(&'a [f32], &'a [f32])>;
    /// MIDI input of a track for this cycle: `f(frame, bytes)` per message.
    fn track_midi(&self, ports: &TrackPorts, shared: &TrackShared, f: &mut dyn FnMut(u32, &[u8]));
    /// Master output buffers, each at least `frames()` long.
    fn master_out(&mut self) -> (&mut [f32], &mut [f32]);
}

/// UI-side control of the audio driver.
pub trait Backend {
    /// Shown in the status bar, e.g. "PipeWire (JACK)" or "ASIO · Focusrite USB".
    fn name(&self) -> String;
    fn is_running(&self) -> bool;
    fn sample_rate(&self) -> u32;
    fn cpu_load(&self) -> f32;
    /// Request a period size. The applied size is reported in
    /// `EngineShared::buffer_size`.
    fn set_buffer_size(&mut self, frames: u32) -> Result<(), String>;
    /// Period sizes worth offering in the UI.
    fn buffer_sizes(&self) -> Vec<u32> {
        (5..=12).map(|p| 1u32 << p).collect()
    }
    /// Endpoints for a new track (called before it is handed to the audio thread).
    fn track_ports(&mut self, id: u64, kind: TrackKind) -> Result<TrackPorts, String>;
    /// Release endpoints of a removed track (UI thread).
    fn release_track_ports(&mut self, ports: TrackPorts);
    /// Selectable input sources: audio channels/ports, or MIDI ports.
    fn sources(&self, kind: TrackKind) -> Vec<String>;
    /// Route `source` (or nothing) to input `side` (0 = left/MIDI, 1 = right).
    /// [`ALL_SOURCES`] selects every MIDI source.
    fn connect_input(&mut self, track: u64, kind: TrackKind, shared: &TrackShared, side: usize, source: Option<&str>);
    /// Output devices: (name, channels/ports).
    fn playback_devices(&self) -> Vec<(String, Vec<String>)>;
    fn master_output(&self) -> Option<String>;
    /// Route the master to these channels/ports (empty = disconnect).
    fn set_master_output(&mut self, ports: &[String]);
}

/// Open the platform's audio backend, handing it the processor.
pub fn open(name: &str, processor: Processor, shared: Arc<EngineShared>) -> Result<Box<dyn Backend>, String> {
    #[cfg(target_os = "linux")]
    {
        jack_backend::JackBackend::open(name, processor, shared).map(|b| Box::new(b) as Box<dyn Backend>)
    }
    #[cfg(windows)]
    {
        let _ = name;
        cpal_backend::CpalBackend::open(processor, shared).map(|b| Box::new(b) as Box<dyn Backend>)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (name, processor, shared);
        Err("no audio backend for this platform yet".into())
    }
}

/// Used when no audio driver could be opened: the UI keeps working and the
/// engine emulates the transport.
pub struct NullBackend;

impl Backend for NullBackend {
    fn name(&self) -> String {
        "offline".into()
    }
    fn is_running(&self) -> bool {
        false
    }
    fn sample_rate(&self) -> u32 {
        48000
    }
    fn cpu_load(&self) -> f32 {
        0.0
    }
    fn set_buffer_size(&mut self, _: u32) -> Result<(), String> {
        Err("audio engine offline".into())
    }
    fn track_ports(&mut self, _: u64, _: TrackKind) -> Result<TrackPorts, String> {
        Ok(TrackPorts::None)
    }
    fn release_track_ports(&mut self, _: TrackPorts) {}
    fn sources(&self, _: TrackKind) -> Vec<String> {
        vec![]
    }
    fn connect_input(&mut self, _: u64, _: TrackKind, _: &TrackShared, _: usize, _: Option<&str>) {}
    fn playback_devices(&self) -> Vec<(String, Vec<String>)> {
        vec![]
    }
    fn master_output(&self) -> Option<String> {
        None
    }
    fn set_master_output(&mut self, _: &[String]) {}
}
