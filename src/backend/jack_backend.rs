//! JACK backend (Linux): works with jackd and with PipeWire's JACK emulation.
//!
//! Every track gets its own JACK ports (`trackN_in_L/R`, `trackN_midi_in`),
//! connected to sources by name; the master goes out on `master_out_L/R`.

use super::{ALL_SOURCES, Backend, CycleIo, TrackPorts};
use crate::engine::{EngineShared, Processor, TrackShared};
use crate::project::TrackKind;
use jack::{AudioIn, AudioOut, Client, ClientOptions, Control, MidiIn, Port, PortFlags, ProcessScope};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const AUDIO_PORT: &str = "32 bit float mono audio";
const MIDI_PORT: &str = "8 bit raw midi";

struct JackIo<'a> {
    ps: &'a ProcessScope,
    out_l: &'a mut Port<AudioOut>,
    out_r: &'a mut Port<AudioOut>,
}

impl CycleIo for JackIo<'_> {
    fn frames(&self) -> usize {
        self.ps.n_frames() as usize
    }

    fn track_audio<'a>(&'a self, ports: &'a TrackPorts, _: &TrackShared) -> Option<(&'a [f32], &'a [f32])> {
        match ports {
            TrackPorts::Jack { audio: Some([l, r]), .. } => Some((l.as_slice(self.ps), r.as_slice(self.ps))),
            _ => None,
        }
    }

    fn track_midi(&self, ports: &TrackPorts, _: &TrackShared, f: &mut dyn FnMut(u32, &[u8])) {
        if let TrackPorts::Jack { midi: Some(port), .. } = ports {
            for raw in port.iter(self.ps) {
                f(raw.time, raw.bytes);
            }
        }
    }

    fn master_out(&mut self) -> (&mut [f32], &mut [f32]) {
        (self.out_l.as_mut_slice(self.ps), self.out_r.as_mut_slice(self.ps))
    }
}

struct Handler {
    processor: Processor,
    out_l: Port<AudioOut>,
    out_r: Port<AudioOut>,
}

impl jack::ProcessHandler for Handler {
    fn buffer_size(&mut self, _: &Client, size: jack::Frames) -> Control {
        self.processor.shared().buffer_size.store(size, Ordering::Relaxed);
        Control::Continue
    }

    fn process(&mut self, _: &Client, ps: &ProcessScope) -> Control {
        let mut io = JackIo { ps, out_l: &mut self.out_l, out_r: &mut self.out_r };
        self.processor.process(&mut io);
        Control::Continue
    }
}

struct Notifications {
    shared: Arc<EngineShared>,
}

impl jack::NotificationHandler for Notifications {
    fn xrun(&mut self, _: &Client) -> Control {
        self.shared.xruns.fetch_add(1, Ordering::Relaxed);
        Control::Continue
    }
    unsafe fn shutdown(&mut self, _: jack::ClientStatus, _: &str) {
        self.shared.server_gone.store(true, Ordering::Relaxed);
    }
}

pub struct JackBackend {
    client: jack::AsyncClient<Notifications, Handler>,
    shared: Arc<EngineShared>,
    /// True when libjack is PipeWire's JACK implementation.
    pipewire: bool,
}

impl JackBackend {
    pub fn open(name: &str, mut processor: Processor, shared: Arc<EngineShared>) -> Result<Self, String> {
        let (client, _status) = Client::new(name, ClientOptions::NO_START_SERVER)
            .map_err(|e| format!("Could not connect to JACK/PipeWire: {e}. Is pipewire-jack installed (or jackd running)?"))?;
        let out_l = client.register_port("master_out_L", AudioOut::default()).map_err(|e| e.to_string())?;
        let out_r = client.register_port("master_out_R", AudioOut::default()).map_err(|e| e.to_string())?;
        processor.set_sample_rate(client.sample_rate() as f64);
        shared.buffer_size.store(client.buffer_size(), Ordering::Relaxed);
        let client = client
            .activate_async(Notifications { shared: shared.clone() }, Handler { processor, out_l, out_r })
            .map_err(|e| e.to_string())?;
        let backend = JackBackend { client, shared, pipewire: libjack_is_pipewire() };
        if let Some((_, ports)) = backend.playback_devices().into_iter().next() {
            backend.connect_master(&ports);
        }
        Ok(backend)
    }

    fn client(&self) -> &Client {
        self.client.as_client()
    }

    fn port_name(&self, short: &str) -> String {
        format!("{}:{short}", self.client().name())
    }

    fn connect_master(&self, ports: &[String]) {
        let c = self.client();
        for (i, side) in ["master_out_L", "master_out_R"].iter().enumerate() {
            let name = self.port_name(side);
            if let Some(p) = c.port_by_name(&name) {
                let _ = c.disconnect(&p);
            }
            if let Some(dst) = ports.get(i).or(ports.first()) {
                let _ = c.connect_ports_by_name(&name, dst);
            }
        }
    }
}

impl Backend for JackBackend {
    fn name(&self) -> String {
        if self.pipewire { "PipeWire (JACK)".into() } else { "JACK".into() }
    }

    fn is_running(&self) -> bool {
        !self.shared.server_gone.load(Ordering::Relaxed)
    }

    fn sample_rate(&self) -> u32 {
        self.client().sample_rate() as u32
    }

    fn cpu_load(&self) -> f32 {
        self.client().cpu_load()
    }

    /// Under PipeWire this forces the graph quantum.
    fn set_buffer_size(&mut self, frames: u32) -> Result<(), String> {
        self.client().set_buffer_size(frames).map_err(|e| format!("could not set buffer size to {frames}: {e}"))
    }

    fn track_ports(&mut self, id: u64, kind: TrackKind) -> Result<TrackPorts, String> {
        let c = self.client();
        Ok(match kind {
            TrackKind::Audio => {
                let l = c.register_port(&format!("track{id}_in_L"), AudioIn::default()).map_err(|e| e.to_string())?;
                let r = c.register_port(&format!("track{id}_in_R"), AudioIn::default()).map_err(|e| e.to_string())?;
                TrackPorts::Jack { audio: Some([l, r]), midi: None }
            }
            TrackKind::Instrument => {
                let m = c.register_port(&format!("track{id}_midi_in"), MidiIn::default()).map_err(|e| e.to_string())?;
                TrackPorts::Jack { audio: None, midi: Some(m) }
            }
        })
    }

    fn release_track_ports(&mut self, ports: TrackPorts) {
        let c = self.client();
        if let TrackPorts::Jack { audio, midi } = ports {
            if let Some([l, r]) = audio {
                let _ = c.unregister_port(l);
                let _ = c.unregister_port(r);
            }
            if let Some(m) = midi {
                let _ = c.unregister_port(m);
            }
        }
    }

    /// Output ports of other clients, physical ones first.
    fn sources(&self, kind: TrackKind) -> Vec<String> {
        let c = self.client();
        let port_type = if kind == TrackKind::Audio { AUDIO_PORT } else { MIDI_PORT };
        let own = format!("{}:", c.name());
        let mut ports: Vec<String> =
            c.ports(None, Some(port_type), PortFlags::IS_OUTPUT).into_iter().filter(|p| !p.starts_with(&own)).collect();
        let physical = c.ports(None, Some(port_type), PortFlags::IS_OUTPUT | PortFlags::IS_PHYSICAL);
        ports.sort_by_key(|p| !physical.contains(p));
        ports
    }

    fn connect_input(&mut self, track: u64, kind: TrackKind, _: &TrackShared, side: usize, source: Option<&str>) {
        let short = match (kind, side) {
            (TrackKind::Audio, 0) => format!("track{track}_in_L"),
            (TrackKind::Audio, _) => format!("track{track}_in_R"),
            (TrackKind::Instrument, _) => format!("track{track}_midi_in"),
        };
        let c = self.client();
        let dst = self.port_name(&short);
        if let Some(p) = c.port_by_name(&dst) {
            let _ = c.disconnect(&p);
        }
        match source {
            Some(ALL_SOURCES) => {
                for src in self.sources(kind) {
                    let _ = c.connect_ports_by_name(&src, &dst);
                }
            }
            Some(src) => {
                let _ = c.connect_ports_by_name(src, &dst);
            }
            None => {}
        }
    }

    /// Hardware playback ports grouped by client, in order.
    fn playback_devices(&self) -> Vec<(String, Vec<String>)> {
        let mut devices: Vec<(String, Vec<String>)> = vec![];
        for port in self.client().ports(None, Some(AUDIO_PORT), PortFlags::IS_INPUT | PortFlags::IS_PHYSICAL) {
            let client = port.split(':').next().unwrap_or("").to_string();
            match devices.iter_mut().find(|(d, _)| *d == client) {
                Some((_, ports)) => ports.push(port),
                None => devices.push((client, vec![port])),
            }
        }
        devices
    }

    fn master_output(&self) -> Option<String> {
        let port = self.client().port_by_name(&self.port_name("master_out_L"))?;
        port.get_connections().first().map(|p| p.split(':').next().unwrap_or("").to_string())
    }

    fn set_master_output(&mut self, ports: &[String]) {
        self.connect_master(ports);
    }
}

fn libjack_is_pipewire() -> bool {
    std::fs::read_to_string("/proc/self/maps")
        .map(|m| m.lines().any(|l| l.contains("libjack") && l.contains("pipewire")))
        .unwrap_or(false)
}
