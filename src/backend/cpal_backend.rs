//! Windows backend: WASAPI (always available) or ASIO (with the `asio` cargo
//! feature and an installed ASIO driver) through cpal; MIDI input through
//! midir (WinMM, which Windows 11 routes through Windows MIDI Services).
//!
//! The audio work happens in [`DeviceRunner`] (platform neutral, unit-tested
//! everywhere); this module only opens devices and streams. The output
//! stream's callback runs the processor; the input stream's callback feeds
//! it captured audio through a ring buffer.
//!
//! Host selection: ASIO when compiled in and a driver is present (lowest
//! latency), unless `STUDIO_AUDIO=wasapi`; otherwise WASAPI shared mode.

use super::devio::{DeviceRunner, MidiInEvent};
use super::{ALL_SOURCES, Backend, TrackPorts};
use crate::engine::{EngineShared, Processor, TrackShared};
use crate::lv2::MAX_BLOCK;
use crate::project::TrackKind;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Separator between device name and channel in channel identifiers.
const CH_SEP: &str = " · Out ";

pub struct CpalBackend {
    host: cpal::Host,
    host_label: &'static str,
    output: cpal::Device,
    input: Option<cpal::Device>,
    runner: Arc<Mutex<DeviceRunner>>,
    streams: Vec<cpal::Stream>,
    alive: Arc<AtomicBool>,
    rate: u32,
    buffer: cpal::BufferSize,
    /// Buffer sizes the output device accepts (frames), if it says.
    buffer_range: Option<(u32, u32)>,
    in_channels: usize,
    midi_names: Vec<String>,
    _midi: Vec<midir::MidiInputConnection<()>>,
}

fn pick_host() -> (cpal::Host, &'static str) {
    #[cfg(feature = "asio")]
    {
        let wasapi_forced = std::env::var("STUDIO_AUDIO").is_ok_and(|v| v.eq_ignore_ascii_case("wasapi"));
        if !wasapi_forced
            && let Ok(h) = cpal::host_from_id(cpal::HostId::Asio)
            && h.output_devices().is_ok_and(|mut d| d.next().is_some())
        {
            return (h, "ASIO");
        }
    }
    (cpal::default_host(), "WASAPI")
}

impl CpalBackend {
    pub fn open(processor: Processor, shared: Arc<EngineShared>) -> Result<Self, String> {
        let (host, host_label) = pick_host();
        let output = host.default_output_device().ok_or("no audio output device found")?;
        let input = Self::input_for(&host, host_label, &output);
        let rate = output.default_output_config().map_err(|e| format!("{output}: {e}"))?.sample_rate();
        let (midi_tx, midi_rx) = rtrb::RingBuffer::new(4096);
        let runner = Arc::new(Mutex::new(DeviceRunner::new(processor, shared, rate, midi_rx)));
        let mut backend = CpalBackend {
            host,
            host_label,
            output,
            input,
            runner,
            streams: vec![],
            alive: Arc::new(AtomicBool::new(false)),
            rate,
            buffer: cpal::BufferSize::Default,
            buffer_range: None,
            in_channels: 0,
            midi_names: vec![],
            _midi: vec![],
        };
        backend.start_streams()?;
        backend.open_midi(midi_tx);
        Ok(backend)
    }

    /// ASIO drivers are full duplex (input on the same device); WASAPI uses
    /// the default capture device.
    fn input_for(host: &cpal::Host, label: &str, output: &cpal::Device) -> Option<cpal::Device> {
        if label == "ASIO" {
            output.supports_input().then(|| output.clone())
        } else {
            host.default_input_device()
        }
    }

    /// (Re)build and start the streams with the current device and buffer size.
    fn start_streams(&mut self) -> Result<(), String> {
        self.streams.clear(); // stops the old callbacks
        let out_cfg = self.output.default_output_config().map_err(|e| format!("{}: {e}", self.output))?;
        self.buffer_range = match out_cfg.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => Some((*min, *max)),
            _ => None,
        };
        let mut config = out_cfg.config();
        config.buffer_size = self.buffer;
        self.rate = config.sample_rate;
        let out_channels = config.channels as usize;

        // Capture stream (same sample rate only; otherwise no input).
        let mut input = None;
        let mut in_stream = None;
        self.in_channels = 0;
        if let Some(dev) = &self.input
            && let Ok(in_cfg) = dev.default_input_config()
            && in_cfg.sample_rate() == self.rate
        {
            let channels = in_cfg.channels() as usize;
            let mut icfg = in_cfg.config();
            icfg.buffer_size = self.buffer;
            let (tx, rx) = rtrb::RingBuffer::new(MAX_BLOCK * channels * 8);
            match build_input(dev, icfg, in_cfg.sample_format(), tx) {
                Ok(s) => {
                    input = Some((rx, channels));
                    in_stream = Some(s);
                    self.in_channels = channels;
                }
                Err(e) => eprintln!("studio: no audio input: {e}"),
            }
        }
        {
            let mut r = self.runner.lock().map_err(|_| "audio runner poisoned")?;
            r.set_input(input);
            r.set_rate(self.rate);
        }
        self.alive.store(true, Ordering::Relaxed);
        let out = build_output(&self.output, config, out_cfg.sample_format(), self.runner.clone(), out_channels, self.alive.clone())?;
        out.play().map_err(|e| e.to_string())?;
        if let Some(s) = &in_stream {
            let _ = s.play();
        }
        self.streams.push(out);
        self.streams.extend(in_stream);
        Ok(())
    }

    fn open_midi(&mut self, tx: rtrb::Producer<MidiInEvent>) {
        let tx = Arc::new(Mutex::new(tx));
        let Ok(probe) = midir::MidiInput::new("Studio") else { return };
        let count = probe.ports().len();
        for i in 0..count {
            // `connect` consumes the MidiInput, so use one per port.
            let Ok(input) = midir::MidiInput::new("Studio") else { continue };
            let ports = input.ports();
            let Some(port) = ports.get(i) else { continue };
            let name = input.port_name(port).unwrap_or_else(|_| format!("MIDI {}", i + 1));
            let tx = tx.clone();
            let source = i as i32;
            let conn = input.connect(
                port,
                "studio-in",
                move |_, msg, _| {
                    if let Some(e) = MidiInEvent::new(source, msg)
                        && let Ok(mut t) = tx.lock()
                    {
                        let _ = t.push(e);
                    }
                },
                (),
            );
            match conn {
                Ok(c) => {
                    self._midi.push(c);
                    self.midi_names.push(name);
                }
                Err(e) => eprintln!("studio: MIDI input {name}: {e}"),
            }
        }
    }
}

fn build_output(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    format: SampleFormat,
    runner: Arc<Mutex<DeviceRunner>>,
    channels: usize,
    alive: Arc<AtomicBool>,
) -> Result<cpal::Stream, String> {
    match format {
        SampleFormat::F32 => output_stream::<f32>(dev, config, runner, channels, alive),
        SampleFormat::I32 => output_stream::<i32>(dev, config, runner, channels, alive),
        SampleFormat::I16 => output_stream::<i16>(dev, config, runner, channels, alive),
        other => Err(format!("unsupported output sample format {other}")),
    }
}

fn output_stream<T>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    runner: Arc<Mutex<DeviceRunner>>,
    channels: usize,
    alive: Arc<AtomicBool>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    // Rendered in f32, then converted to the device format.
    let mut scratch = vec![0f32; MAX_BLOCK * channels.max(1) * 2];
    dev.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            let n = data.len();
            if n > scratch.len() {
                return; // larger than any real period; leave the silence cpal pre-filled
            }
            match runner.try_lock() {
                Ok(mut r) => r.render(&mut scratch[..n], channels),
                Err(_) => scratch[..n].fill(0.0), // being reconfigured
            }
            for (d, s) in data.iter_mut().zip(&scratch[..n]) {
                *d = T::from_sample(*s);
            }
        },
        move |e| {
            eprintln!("studio: audio output error: {e}");
            alive.store(false, Ordering::Relaxed);
        },
        None,
    )
    .map_err(|e| format!("{dev}: {e}"))
}

fn build_input(dev: &cpal::Device, config: cpal::StreamConfig, format: SampleFormat, tx: rtrb::Producer<f32>) -> Result<cpal::Stream, String> {
    match format {
        SampleFormat::F32 => input_stream::<f32>(dev, config, tx),
        SampleFormat::I32 => input_stream::<i32>(dev, config, tx),
        SampleFormat::I16 => input_stream::<i16>(dev, config, tx),
        other => Err(format!("unsupported input sample format {other}")),
    }
}

fn input_stream<T>(dev: &cpal::Device, config: cpal::StreamConfig, mut tx: rtrb::Producer<f32>) -> Result<cpal::Stream, String>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    dev.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            // Drop what does not fit; the runner bounds latency on its side.
            let n = tx.slots().min(data.len());
            if let Ok(chunk) = tx.write_chunk_uninit(n) {
                chunk.fill_from_iter(data.iter().map(|s| s.to_sample::<f32>()));
            }
        },
        |e| eprintln!("studio: audio input error: {e}"),
        None,
    )
    .map_err(|e| format!("{dev}: {e}"))
}

impl Backend for CpalBackend {
    fn name(&self) -> String {
        format!("{} · {}", self.host_label, self.output)
    }

    fn is_running(&self) -> bool {
        !self.streams.is_empty() && self.alive.load(Ordering::Relaxed)
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn cpu_load(&self) -> f32 {
        self.runner.lock().map_or(0.0, |r| r.shared().dsp_load.get() * 100.0)
    }

    fn set_buffer_size(&mut self, frames: u32) -> Result<(), String> {
        if let Some((min, max)) = self.buffer_range
            && !(min..=max).contains(&frames)
        {
            return Err(format!("{} supports {min}-{max} frames", self.output));
        }
        let previous = self.buffer;
        self.buffer = cpal::BufferSize::Fixed(frames);
        if let Err(e) = self.start_streams() {
            self.buffer = previous;
            let _ = self.start_streams();
            return Err(format!("could not use {frames} frames: {e}"));
        }
        Ok(())
    }

    /// Powers of two the device accepts, plus its own minimum and maximum
    /// (e.g. WASAPI's 480-frame/10 ms default period is not a power of two).
    fn buffer_sizes(&self) -> Vec<u32> {
        let all: Vec<u32> = (5..=12).map(|p| 1u32 << p).collect();
        match self.buffer_range {
            Some((min, max)) => {
                let mut v: Vec<u32> = all.into_iter().filter(|n| (min..=max).contains(n)).collect();
                v.extend([min.max(16), max.min(8192)]);
                v.sort_unstable();
                v.dedup();
                v
            }
            None => all,
        }
    }

    fn track_ports(&mut self, _: u64, _: TrackKind) -> Result<TrackPorts, String> {
        Ok(TrackPorts::Device)
    }

    fn release_track_ports(&mut self, _: TrackPorts) {}

    fn sources(&self, kind: TrackKind) -> Vec<String> {
        match kind {
            TrackKind::Audio => (1..=self.in_channels).map(|c| format!("In {c}")).collect(),
            TrackKind::Instrument => self.midi_names.clone(),
        }
    }

    fn connect_input(&mut self, _: u64, kind: TrackKind, shared: &TrackShared, side: usize, source: Option<&str>) {
        match kind {
            TrackKind::Audio => {
                let ch = source.and_then(|s| s.strip_prefix("In ")).and_then(|n| n.parse::<i32>().ok()).map_or(-1, |n| n - 1);
                shared.in_channels[side.min(1)].store(ch, Ordering::Relaxed);
            }
            TrackKind::Instrument => {
                let src = match source {
                    Some(ALL_SOURCES) => -2,
                    Some(name) => self.midi_names.iter().position(|n| n == name).map_or(-1, |i| i as i32),
                    None => -1,
                };
                shared.midi_source.store(src, Ordering::Relaxed);
            }
        }
    }

    fn playback_devices(&self) -> Vec<(String, Vec<String>)> {
        let Ok(devices) = self.host.output_devices() else { return vec![] };
        devices
            .map(|d| {
                let name = d.to_string();
                let channels = d.default_output_config().map_or(2, |c| c.channels());
                let ports = (1..=channels).map(|c| format!("{name}{CH_SEP}{c}")).collect();
                (name, ports)
            })
            .collect()
    }

    fn master_output(&self) -> Option<String> {
        Some(self.output.to_string())
    }

    /// Switch the output device (the master always uses its first two channels).
    fn set_master_output(&mut self, ports: &[String]) {
        let Some(name) = ports.first().and_then(|p| p.rsplit_once(CH_SEP)).map(|(n, _)| n.to_string()) else { return };
        let Some(dev) = self.host.output_devices().ok().and_then(|mut ds| ds.find(|d| d.to_string() == name)) else { return };
        let previous = std::mem::replace(&mut self.output, dev);
        self.input = Self::input_for(&self.host, self.host_label, &self.output).or(self.input.take());
        if let Err(e) = self.start_streams() {
            eprintln!("studio: cannot use {name}: {e}");
            self.output = previous;
            let _ = self.start_streams();
        }
    }
}
