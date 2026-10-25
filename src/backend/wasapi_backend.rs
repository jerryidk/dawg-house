//! Windows backend: WASAPI, driven directly through the Windows API.
//!
//! * **Shared mode** (default): plays alongside other applications. When the
//!   engine runs at the Windows mixer's rate, streams open through
//!   `IAudioClient3` with a short period (Windows 10+; typically down to
//!   128 frames ≈ 2.7 ms with the inbox HD Audio driver, depending on the
//!   driver). At other rates the Windows resampler sits in between and the
//!   period is the mixer's (usually 10 ms).
//! * **Exclusive mode**: Studio owns the output device, bypassing the mixer:
//!   any rate and period the driver supports (often 3 ms or less); other
//!   applications go silent on that device.
//! * **Inputs**: every active capture device is a source. A device's capture
//!   stream opens when a track uses one of its channels, in the same mode
//!   (falling back to shared mode with resampling).
//!
//! Each stream runs on its own event-driven thread in the MMCSS "Pro Audio"
//! class. The output thread runs the processor ([`DeviceRunner`]); capture
//! threads feed it through ring buffers.

use super::devio::{DeviceInput, DeviceRunner, MAX_DEVICE_CHANNELS, channel_id};
use super::winmidi::MidiInputs;
use super::{Backend, TrackPorts};
use crate::engine::{EngineShared, Processor, TrackShared};
use crate::lv2::MAX_BLOCK;
use crate::project::TrackKind;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, PROPERTYKEY, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::core::{Interface, PCWSTR, w};

/// Separator between device name and channel in source names.
const IN_SEP: &str = " · In ";
const OUT_SEP: &str = " · Out ";
const SHARED: &str = "Shared";
const EXCLUSIVE: &str = "Exclusive";
/// Rates offered besides the device's own.
const COMMON_RATES: [u32; 6] = [44100, 48000, 88200, 96000, 176400, 192000];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Mode {
    Shared,
    Exclusive,
}

/// An audio endpoint: id (stable across runs), name and mixer format.
#[derive(Clone, Debug)]
struct Endpoint {
    id: String,
    name: String,
    channels: usize,
    rate: u32,
}

/// Driver settings, remembered between runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Settings {
    mode: Mode,
    /// 0 = the output device's mixer rate.
    rate: u32,
    /// Requested period in frames (0 = driver default).
    period: u32,
    /// Output endpoint id (None = the Windows default device).
    output: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { mode: Mode::Shared, rate: 0, period: 0, output: None }
    }
}

impl Settings {
    fn path() -> std::path::PathBuf {
        let base = std::env::var_os("APPDATA").map(std::path::PathBuf::from).unwrap_or_else(crate::util::home_dir);
        base.join("Studio").join("audio.json")
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, s);
        }
    }
}

pub struct WasapiBackend {
    runner: Arc<Mutex<DeviceRunner>>,
    alive: Arc<AtomicBool>,
    settings: Settings,
    rate: u32,
    outputs: Vec<Endpoint>,
    /// Capture devices; a channel id's device part indexes this list, so it
    /// only grows while Studio runs.
    inputs: Vec<Endpoint>,
    /// The Windows default capture device (listed first, so new tracks use it).
    default_input: Option<String>,
    output: Option<(Stream, Opened)>,
    captures: Vec<(usize, Stream)>,
    /// Input device each track side listens to: (track, side) → index into `inputs`.
    routes: HashMap<(u64, usize), usize>,
    midi: MidiInputs,
}

impl WasapiBackend {
    pub fn open(processor: Processor, shared: Arc<EngineShared>) -> Result<Self, String> {
        let outputs = on_com_thread(|| enumerate(eRender))?;
        let inputs = on_com_thread(|| enumerate(eCapture)).unwrap_or_default();
        if outputs.is_empty() {
            return Err("no audio output device found".into());
        }
        let (midi_tx, midi_rx) = rtrb::RingBuffer::new(4096);
        let mut backend = WasapiBackend {
            runner: Arc::new(Mutex::new(DeviceRunner::new(processor, shared, 48000, midi_rx))),
            alive: Arc::new(AtomicBool::new(false)),
            settings: Settings::load(),
            rate: 48000,
            outputs,
            inputs,
            default_input: on_com_thread(|| default_endpoint(eCapture)).ok().flatten(),
            output: None,
            captures: vec![],
            routes: HashMap::new(),
            midi: MidiInputs::open(midi_tx),
        };
        if let Err(e) = backend.start() {
            // Saved settings may no longer fit the hardware: retry with defaults.
            eprintln!("studio: WASAPI: {e}; retrying with default settings");
            backend.settings = Settings::default();
            backend.start()?;
        }
        Ok(backend)
    }

    /// The output endpoint in use: the chosen one, else the Windows default.
    fn output_endpoint(&self) -> Option<Endpoint> {
        let chosen = self.settings.output.as_ref().and_then(|id| self.outputs.iter().find(|e| &e.id == id));
        chosen.cloned().or_else(|| {
            let default = on_com_thread(|| default_endpoint(eRender)).ok().flatten();
            default.and_then(|id| self.outputs.iter().find(|e| e.id == id).cloned()).or_else(|| self.outputs.first().cloned())
        })
    }

    /// (Re)open the output and the capture streams tracks use.
    fn start(&mut self) -> Result<(), String> {
        self.captures.clear();
        self.output = None; // joins the old output thread
        let ep = self.output_endpoint().ok_or("no audio output device found")?;
        let rate = if self.settings.rate > 0 { self.settings.rate } else { ep.rate };
        let config = StreamConfig { mode: self.settings.mode, rate, period: self.settings.period };
        self.alive.store(true, Ordering::Relaxed);
        let io = StreamIo::Render { runner: self.runner.clone(), alive: self.alive.clone() };
        // The runner must know the rate before the first callback.
        if let Ok(mut r) = self.runner.lock() {
            r.set_rate(rate);
        }
        let (stream, opened) = Stream::spawn(ep.id.clone(), config, io).map_err(|e| format!("{}: {e}", ep.name))?;
        self.rate = opened.rate;
        if let Ok(mut r) = self.runner.lock() {
            r.set_rate(opened.rate);
            r.set_period(opened.period);
        }
        self.output = Some((stream, opened));
        self.update_captures(true);
        Ok(())
    }

    /// Open capture streams for the input devices tracks use and close the
    /// rest. With `force`, reopen all of them.
    fn update_captures(&mut self, force: bool) {
        let wanted: BTreeSet<usize> = self.routes.values().copied().filter(|&d| d < self.inputs.len()).collect();
        let open: BTreeSet<usize> = self.captures.iter().map(|(d, _)| *d).collect();
        if wanted == open && !force {
            return;
        }
        let Some((_, out)) = &self.output else { return };
        let mode = self.settings.mode;
        let (rate, period) = (out.rate, out.period);
        // Stop the old streams first: a device can only be opened once in
        // exclusive mode.
        self.captures.clear();
        let mut inputs = vec![];
        for d in wanted {
            let ep = &self.inputs[d];
            let channels = ep.channels.clamp(1, MAX_DEVICE_CHANNELS);
            let (tx, rx) = rtrb::RingBuffer::new(MAX_BLOCK * channels * 8);
            let config = StreamConfig { mode, rate, period };
            let tx = Arc::new(Mutex::new(Some(tx)));
            let mut result = Stream::spawn(ep.id.clone(), config, StreamIo::Capture { tx: tx.clone() });
            if result.is_err() && mode == Mode::Exclusive {
                // E.g. the device lacks this rate: share it and resample.
                result = Stream::spawn(ep.id.clone(), StreamConfig { mode: Mode::Shared, ..config }, StreamIo::Capture { tx });
            }
            match result {
                Ok((stream, _)) => {
                    inputs.push(DeviceInput { device: d, channels, rx });
                    self.captures.push((d, stream));
                }
                Err(e) => eprintln!("studio: input {}: {e}", ep.name),
            }
        }
        if let Ok(mut r) = self.runner.lock() {
            r.set_inputs(inputs);
        }
    }

    /// Refresh the device lists. Known capture devices keep their index.
    fn refresh_endpoints(&mut self) {
        if let Ok(outs) = on_com_thread(|| enumerate(eRender)) {
            self.outputs = outs;
        }
        self.default_input = on_com_thread(|| default_endpoint(eCapture)).ok().flatten();
        if let Ok(ins) = on_com_thread(|| enumerate(eCapture)) {
            for ep in ins {
                match self.inputs.iter_mut().find(|e| e.id == ep.id) {
                    Some(known) => *known = ep,
                    None => self.inputs.push(ep),
                }
            }
        }
    }

    /// Apply new settings, going back to the old ones if the device refuses.
    fn reconfigure(&mut self, settings: Settings) -> Result<(), String> {
        let previous = std::mem::replace(&mut self.settings, settings);
        if let Err(e) = self.start() {
            self.settings = previous;
            let _ = self.start();
            return Err(e);
        }
        self.settings.save();
        Ok(())
    }

    fn parse_input(&self, source: &str) -> Option<(usize, usize)> {
        let (name, ch) = source.rsplit_once(IN_SEP)?;
        let ch: usize = ch.parse().ok().filter(|&c| c >= 1)?;
        let dev = self.inputs.iter().position(|e| e.name == name)?;
        Some((dev, ch - 1))
    }
}

impl Backend for WasapiBackend {
    fn name(&self) -> String {
        let device = self.output_endpoint().map_or_else(String::new, |e| e.name);
        let mode = match (self.settings.mode, self.output.as_ref().map(|(_, o)| o.low_latency)) {
            (Mode::Exclusive, _) => "exclusive",
            (Mode::Shared, Some(true)) => "shared, low latency",
            (Mode::Shared, _) => "shared",
        };
        format!("WASAPI {mode} · {device}")
    }

    fn is_running(&self) -> bool {
        self.output.is_some() && self.alive.load(Ordering::Relaxed)
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn cpu_load(&self) -> f32 {
        self.runner.lock().map_or(0.0, |r| r.shared().dsp_load.get() * 100.0)
    }

    fn set_buffer_size(&mut self, frames: u32) -> Result<(), String> {
        let settings = Settings { period: frames, ..self.settings.clone() };
        self.reconfigure(settings).map_err(|e| format!("could not use {frames} frames: {e}"))
    }

    fn buffer_sizes(&self) -> Vec<u32> {
        self.output.as_ref().map_or_else(Vec::new, |(_, o)| o.sizes.clone())
    }

    fn modes(&self) -> Vec<String> {
        vec![SHARED.into(), EXCLUSIVE.into()]
    }

    fn mode(&self) -> Option<String> {
        Some(match self.settings.mode {
            Mode::Shared => SHARED.into(),
            Mode::Exclusive => EXCLUSIVE.into(),
        })
    }

    fn set_mode(&mut self, mode: &str) -> Result<(), String> {
        let mode = if mode == EXCLUSIVE { Mode::Exclusive } else { Mode::Shared };
        // Periods differ between modes: start from the driver default.
        let settings = Settings { mode, period: 0, ..self.settings.clone() };
        self.reconfigure(settings)
    }

    fn sample_rates(&self) -> Vec<u32> {
        let mut rates: Vec<u32> = COMMON_RATES.to_vec();
        if let Some(ep) = self.output_endpoint() {
            rates.push(ep.rate);
        }
        rates.sort_unstable();
        rates.dedup();
        rates
    }

    fn set_sample_rate(&mut self, rate: u32) -> Result<(), String> {
        let settings = Settings { rate, ..self.settings.clone() };
        self.reconfigure(settings).map_err(|e| format!("could not run at {rate} Hz: {e}"))
    }

    fn track_ports(&mut self, _: u64, _: TrackKind) -> Result<TrackPorts, String> {
        Ok(TrackPorts::Device)
    }

    fn release_track_ports(&mut self, _: TrackPorts) {}

    fn sources(&self, kind: TrackKind) -> Vec<String> {
        match kind {
            TrackKind::Audio => {
                let default = self.inputs.iter().filter(|e| Some(&e.id) == self.default_input.as_ref());
                let others = self.inputs.iter().filter(|e| Some(&e.id) != self.default_input.as_ref());
                default.chain(others).flat_map(|e| (1..=e.channels.min(MAX_DEVICE_CHANNELS)).map(move |c| format!("{}{IN_SEP}{c}", e.name))).collect()
            }
            TrackKind::Instrument => self.midi.names.clone(),
        }
    }

    fn connect_input(&mut self, track: u64, kind: TrackKind, shared: &TrackShared, side: usize, source: Option<&str>) {
        let side = side.min(1);
        match kind {
            TrackKind::Audio => {
                if source.is_some_and(|s| self.parse_input(s).is_none()) {
                    self.refresh_endpoints(); // a device plugged in since
                }
                let route = source.and_then(|s| self.parse_input(s));
                shared.in_channels[side].store(route.map_or(-1, |(d, c)| channel_id(d, c)), Ordering::Relaxed);
                match route {
                    Some((d, _)) => self.routes.insert((track, side), d),
                    None => self.routes.remove(&(track, side)),
                };
                self.update_captures(false);
            }
            TrackKind::Instrument => shared.midi_source.store(self.midi.source_index(source), Ordering::Relaxed),
        }
    }

    fn playback_devices(&self) -> Vec<(String, Vec<String>)> {
        self.outputs
            .iter()
            .map(|e| (e.name.clone(), (1..=e.channels).map(|c| format!("{}{OUT_SEP}{c}", e.name)).collect()))
            .collect()
    }

    fn master_output(&self) -> Option<String> {
        self.output_endpoint().map(|e| e.name)
    }

    /// Switch the output device (the master always uses its first two channels).
    fn set_master_output(&mut self, ports: &[String]) {
        let Some(name) = ports.first().and_then(|p| p.rsplit_once(OUT_SEP)).map(|(n, _)| n) else { return };
        let Some(id) = self.outputs.iter().find(|e| e.name == name).map(|e| e.id.clone()) else { return };
        // A new device may not support the current period: use its default.
        let settings = Settings { output: Some(id), period: 0, ..self.settings.clone() };
        if let Err(e) = self.reconfigure(settings) {
            eprintln!("studio: cannot use {name}: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Streams

#[derive(Clone, Copy, Debug)]
struct StreamConfig {
    mode: Mode,
    rate: u32,
    /// Requested period in frames (0 = default).
    period: u32,
}

/// What an opened stream ended up with.
#[derive(Clone, Debug)]
struct Opened {
    rate: u32,
    period: u32,
    /// Periods worth offering for this device and mode.
    sizes: Vec<u32>,
    low_latency: bool,
}

enum StreamIo {
    /// Runs the processor.
    Render { runner: Arc<Mutex<DeviceRunner>>, alive: Arc<AtomicBool> },
    /// Captured audio goes here (taken by the thread that opens successfully).
    Capture { tx: Arc<Mutex<Option<rtrb::Producer<f32>>>> },
}

/// A stream's thread; dropping it stops and joins the thread.
struct Stream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Stream {
    /// Start a stream thread on endpoint `id` and wait until the device is
    /// open (or failed to open).
    fn spawn(id: String, config: StreamConfig, io: StreamIo) -> Result<(Stream, Opened), String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("studio-wasapi".into())
            .spawn(move || {
                com_init();
                let opened = unsafe { open_client(&id, &config) };
                match opened {
                    Ok(client) => {
                        let _ = tx.send(Ok(client.opened.clone()));
                        let result = unsafe { run(&client, io, &stop2) };
                        if let Err(e) = result {
                            eprintln!("studio: WASAPI stream stopped: {e}");
                        }
                        unsafe {
                            let _ = client.client.Stop();
                            let _ = CloseHandle(client.event);
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let stream = Stream { stop, thread: Some(thread) };
        match rx.recv() {
            Ok(Ok(opened)) => Ok((stream, opened)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("audio thread exited".into()),
        }
    }
}

/// Sample encodings Studio converts to and from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sample {
    F32,
    I16,
    /// Packed 24-bit.
    I24,
    /// 32-bit container (with 24 or 32 valid bits, MSB aligned).
    I32 { valid: u16 },
}

impl Sample {
    fn bytes(self) -> usize {
        match self {
            Sample::I16 => 2,
            Sample::I24 => 3,
            Sample::F32 | Sample::I32 { .. } => 4,
        }
    }
}

struct Client {
    client: IAudioClient,
    event: HANDLE,
    sample: Sample,
    channels: usize,
    /// Buffer size in frames.
    buffer: u32,
    exclusive: bool,
    opened: Opened,
}

fn hns(frames: u32, rate: u32) -> i64 {
    (10_000_000i64 * frames as i64 + rate as i64 / 2) / rate.max(1) as i64
}

fn frames(hns: i64, rate: u32) -> u32 {
    ((hns * rate as i64 + 5_000_000) / 10_000_000) as u32
}

fn format(sample: Sample, channels: usize, rate: u32, mask: u32) -> WAVEFORMATEXTENSIBLE {
    let bytes = sample.bytes() as u16;
    let channels = channels as u16;
    let valid = match sample {
        Sample::I32 { valid } => valid,
        _ => bytes * 8,
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * (channels * bytes) as u32,
            nBlockAlign: channels * bytes,
            wBitsPerSample: bytes * 8,
            cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: valid },
        dwChannelMask: mask,
        SubFormat: if sample == Sample::F32 { KSDATAFORMAT_SUBTYPE_IEEE_FLOAT } else { KSDATAFORMAT_SUBTYPE_PCM },
    }
}

/// The mixer format: (sample encoding, channels, rate, channel mask).
unsafe fn describe(fmt: *const WAVEFORMATEX) -> Option<(Sample, usize, u32, u32)> {
    let f = unsafe { std::ptr::read_unaligned(fmt) };
    let (float, mask, valid) = if f.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
        let x = unsafe { std::ptr::read_unaligned(fmt as *const WAVEFORMATEXTENSIBLE) };
        let sub = x.SubFormat;
        (sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, x.dwChannelMask, unsafe { x.Samples.wValidBitsPerSample })
    } else {
        (f.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT, 0, f.wBitsPerSample)
    };
    let sample = match (float, f.wBitsPerSample) {
        (true, 32) => Sample::F32,
        (false, 16) => Sample::I16,
        (false, 24) => Sample::I24,
        (false, 32) => Sample::I32 { valid },
        _ => return None,
    };
    Some((sample, f.nChannels as usize, f.nSamplesPerSec, mask))
}

/// Period sizes to offer from the grid `min + k * step` up to `max`: all of
/// them when there are few, else the minimum, the default and the powers of
/// two on the grid.
fn offered_sizes(min: u32, max: u32, step: u32, default: u32) -> Vec<u32> {
    let step = step.max(1);
    let max = max.max(min);
    let grid = (max - min) / step + 1;
    let mut v: Vec<u32> = if grid <= 10 {
        (0..grid).map(|k| min + k * step).collect()
    } else {
        (4..=13).map(|p| 1u32 << p).filter(|n| (min..=max).contains(n) && (n - min).is_multiple_of(step)).collect()
    };
    v.extend([min, default].into_iter().filter(|&n| n > 0 && n <= max));
    v.sort_unstable();
    v.dedup();
    v
}

unsafe fn activate(id: &str) -> Result<IAudioClient, String> {
    let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| e.to_string())?;
    let device = unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }.map_err(|e| format!("device gone: {e}"))?;
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.map_err(|e| e.to_string())
}

/// Open and initialize an audio client on endpoint `id`.
unsafe fn open_client(id: &str, config: &StreamConfig) -> Result<Client, String> {
    let client = unsafe { activate(id)? };
    let mix_ptr = unsafe { client.GetMixFormat() }.map_err(|e| e.to_string())?;
    let mix = unsafe { describe(mix_ptr) };
    let result = match mix {
        Some(mix) => unsafe {
            match config.mode {
                Mode::Shared => open_shared(id, client, mix_ptr, mix, config),
                Mode::Exclusive => open_exclusive(id, client, mix, config),
            }
        },
        None => Err("unsupported mixer format".into()),
    };
    unsafe { CoTaskMemFree(Some(mix_ptr as *const _)) };
    let mut c = result?;
    unsafe {
        c.event = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
        c.client.SetEventHandle(c.event).map_err(|e| e.to_string())?;
        c.buffer = c.client.GetBufferSize().map_err(|e| e.to_string())?;
    }
    if c.opened.period == 0 || c.exclusive {
        c.opened.period = c.buffer;
    }
    Ok(c)
}

unsafe fn open_shared(id: &str, client: IAudioClient, mix_ptr: *const WAVEFORMATEX, mix: (Sample, usize, u32, u32), config: &StreamConfig) -> Result<Client, String> {
    let (sample, channels, mix_rate, mask) = mix;
    let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    // Low latency: IAudioClient3 at the mixer's own format.
    if config.rate == mix_rate
        && let Ok(c3) = client.cast::<IAudioClient3>()
    {
        let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
        if unsafe { c3.GetSharedModeEnginePeriod(mix_ptr, &mut default, &mut fundamental, &mut min, &mut max) }.is_ok() && min > 0 {
            let fundamental = fundamental.max(1);
            let want = if config.period == 0 { default } else { config.period };
            // Periods are min + k * fundamental.
            let k = (want.clamp(min, max.max(min)) - min).div_ceil(fundamental);
            let period = (min + k * fundamental).min(max.max(min));
            let result = unsafe { c3.InitializeSharedAudioStream(flags, period, mix_ptr, None) };
            if let Err(e) = &result
                && config.period != 0
            {
                // A period the user picked: say so rather than quietly using 10 ms.
                return Err(format!("{period}-frame period refused: {e}"));
            }
            if result.is_ok() {
                return Ok(Client {
                    client,
                    event: HANDLE::default(),
                    sample,
                    channels,
                    buffer: 0,
                    exclusive: false,
                    opened: Opened { rate: mix_rate, period, sizes: offered_sizes(min, max, fundamental, default), low_latency: true },
                });
            }
        }
    }
    // Standard shared mode, with the Windows resampler for other rates.
    let client = if config.rate == mix_rate { client } else { unsafe { activate(id)? } };
    let (fmt_ptr, extra, fmt);
    if config.rate == mix_rate {
        fmt_ptr = mix_ptr;
        extra = 0;
    } else {
        fmt = format(Sample::F32, channels, config.rate, mask);
        fmt_ptr = &fmt.Format as *const _;
        extra = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    }
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags | extra, 0, 0, fmt_ptr, None) }.map_err(|e| e.to_string())?;
    let mut default = 0i64;
    let _ = unsafe { client.GetDevicePeriod(Some(&mut default), None) };
    let period = frames(default, config.rate);
    Ok(Client {
        client,
        event: HANDLE::default(),
        sample: if extra != 0 { Sample::F32 } else { sample },
        channels,
        buffer: 0,
        exclusive: false,
        opened: Opened { rate: config.rate, period, sizes: if period > 0 { vec![period] } else { vec![] }, low_latency: false },
    })
}

unsafe fn open_exclusive(id: &str, client: IAudioClient, mix: (Sample, usize, u32, u32), config: &StreamConfig) -> Result<Client, String> {
    let (_, channels, _, mask) = mix;
    let candidates = [Sample::F32, Sample::I32 { valid: 32 }, Sample::I32 { valid: 24 }, Sample::I24, Sample::I16];
    let (sample, fmt) = candidates
        .into_iter()
        .map(|s| (s, format(s, channels, config.rate, mask)))
        .find(|(_, f)| unsafe { client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &f.Format, None) } == windows::core::HRESULT(0))
        .ok_or_else(|| format!("{} Hz is not supported in exclusive mode", config.rate))?;
    let (mut default, mut min) = (0i64, 0i64);
    unsafe { client.GetDevicePeriod(Some(&mut default), Some(&mut min)) }.map_err(|e| e.to_string())?;
    let min_frames = frames(min, config.rate).max(1);
    let default_frames = frames(default, config.rate);
    let mut period = if config.period == 0 { hns(default_frames, config.rate) } else { hns(config.period, config.rate) }.max(min);
    let mut client = client;
    let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let mut result = unsafe { client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, period, period, &fmt.Format, None) };
    if let Err(e) = &result
        && e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
    {
        // Use the aligned size the driver suggests, on a fresh client.
        let aligned = unsafe { client.GetBufferSize() }.map_err(|e| e.to_string())?;
        period = hns(aligned, config.rate);
        client = unsafe { activate(id)? };
        result = unsafe { client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, period, period, &fmt.Format, None) };
    }
    result.map_err(|e| e.to_string())?;
    Ok(Client {
        client,
        event: HANDLE::default(),
        sample,
        channels,
        buffer: 0,
        exclusive: true,
        opened: Opened { rate: config.rate, period: 0, sizes: offered_sizes(min_frames, 4096, 1, default_frames), low_latency: true },
    })
}

/// Event loop of a stream thread until `stop`.
unsafe fn run(c: &Client, io: StreamIo, stop: &AtomicBool) -> Result<(), String> {
    let mut task = 0u32;
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) }.ok();
    let result = match io {
        StreamIo::Render { runner, alive } => {
            let r = unsafe { render_loop(c, &runner, stop) };
            if r.is_err() {
                alive.store(false, Ordering::Relaxed);
            }
            r
        }
        StreamIo::Capture { tx } => match tx.lock().ok().and_then(|mut t| t.take()) {
            Some(mut tx) => unsafe { capture_loop(c, &mut tx, stop) },
            None => Ok(()),
        },
    };
    if let Some(h) = mmcss {
        let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
    }
    result
}

unsafe fn render_loop(c: &Client, runner: &Mutex<DeviceRunner>, stop: &AtomicBool) -> Result<(), String> {
    let render: IAudioRenderClient = unsafe { c.client.GetService() }.map_err(|e| e.to_string())?;
    let ch = c.channels.max(1);
    let mut scratch = vec![0f32; c.buffer as usize * ch];
    // Start from a buffer of silence.
    unsafe {
        render.GetBuffer(c.buffer).map_err(|e| e.to_string())?;
        render.ReleaseBuffer(c.buffer, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).map_err(|e| e.to_string())?;
        c.client.Start().map_err(|e| e.to_string())?;
    }
    while !stop.load(Ordering::Relaxed) {
        if unsafe { WaitForSingleObject(c.event, 200) } != WAIT_OBJECT_0 {
            continue;
        }
        let n = if c.exclusive {
            c.buffer
        } else {
            c.buffer.saturating_sub(unsafe { c.client.GetCurrentPadding() }.map_err(|e| e.to_string())?)
        };
        if n == 0 {
            continue;
        }
        let samples = &mut scratch[..n as usize * ch];
        match runner.try_lock() {
            Ok(mut r) => r.render(samples, ch),
            Err(_) => samples.fill(0.0), // being reconfigured
        }
        unsafe {
            let data = render.GetBuffer(n).map_err(|e| e.to_string())?;
            write_samples(samples, c.sample, data);
            render.ReleaseBuffer(n, 0).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

unsafe fn capture_loop(c: &Client, tx: &mut rtrb::Producer<f32>, stop: &AtomicBool) -> Result<(), String> {
    let capture: IAudioCaptureClient = unsafe { c.client.GetService() }.map_err(|e| e.to_string())?;
    unsafe { c.client.Start() }.map_err(|e| e.to_string())?;
    let ch = c.channels.max(1);
    while !stop.load(Ordering::Relaxed) {
        if unsafe { WaitForSingleObject(c.event, 200) } != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            let packet = unsafe { capture.GetNextPacketSize() }.map_err(|e| e.to_string())?;
            if packet == 0 {
                break;
            }
            let (mut data, mut n, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            unsafe { capture.GetBuffer(&mut data, &mut n, &mut flags, None, None) }.map_err(|e| e.to_string())?;
            let count = (n as usize * ch).min(tx.slots());
            if let Ok(chunk) = tx.write_chunk_uninit(count) {
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    chunk.fill_from_iter(std::iter::repeat(0.0));
                } else {
                    let bytes = unsafe { std::slice::from_raw_parts(data, n as usize * ch * c.sample.bytes()) };
                    chunk.fill_from_iter(read_samples(bytes, c.sample));
                }
            }
            unsafe { capture.ReleaseBuffer(n) }.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Convert f32 samples into the device encoding at `out`.
unsafe fn write_samples(src: &[f32], sample: Sample, out: *mut u8) {
    let bytes = unsafe { std::slice::from_raw_parts_mut(out, src.len() * sample.bytes()) };
    encode(src, sample, bytes);
}

fn encode(src: &[f32], sample: Sample, bytes: &mut [u8]) {
    let w = sample.bytes();
    for (s, d) in src.iter().zip(bytes.chunks_exact_mut(w)) {
        let s = s.clamp(-1.0, 1.0);
        match sample {
            Sample::F32 => d.copy_from_slice(&s.to_le_bytes()),
            Sample::I16 => d.copy_from_slice(&((s * i16::MAX as f32) as i16).to_le_bytes()),
            Sample::I24 => d.copy_from_slice(&((s * 8_388_607.0) as i32).to_le_bytes()[..3]),
            Sample::I32 { .. } => d.copy_from_slice(&((s as f64 * i32::MAX as f64) as i32).to_le_bytes()),
        }
    }
}

fn read_samples(bytes: &[u8], sample: Sample) -> impl Iterator<Item = f32> + '_ {
    bytes.chunks_exact(sample.bytes()).map(move |b| match sample {
        Sample::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        Sample::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        Sample::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
        Sample::I32 { .. } => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
    })
}

// ---------------------------------------------------------------------------
// Device enumeration

fn com_init() {
    // S_FALSE / RPC_E_CHANGED_MODE are fine: COM is usable either way.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Run `f` on a fresh MTA thread (the UI thread is an OLE STA).
fn on_com_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    std::thread::spawn(move || {
        com_init();
        f()
    })
    .join()
    .unwrap_or_else(|_| Err("device enumeration panicked".into()))
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| e.to_string())
}

unsafe fn device_id(device: &IMMDevice) -> Option<String> {
    let id = unsafe { device.GetId() }.ok()?;
    let s = unsafe { id.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(id.0 as *const _)) };
    s
}

fn default_endpoint(flow: EDataFlow) -> Result<Option<String>, String> {
    let e = enumerator()?;
    Ok(unsafe { e.GetDefaultAudioEndpoint(flow, eConsole) }.ok().and_then(|d| unsafe { device_id(&d) }))
}

unsafe fn friendly_name(device: &IMMDevice) -> Option<String> {
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    let key = &DEVPKEY_Device_FriendlyName as *const _ as *const PROPERTYKEY;
    let mut value = unsafe { store.GetValue(key) }.ok()?;
    let inner = unsafe { &value.Anonymous.Anonymous };
    let name = if inner.vt == VT_LPWSTR { unsafe { inner.Anonymous.pwszVal.to_string() }.ok() } else { None };
    let _ = unsafe { PropVariantClear(&mut value) };
    name
}

/// Active endpoints of one direction with their mixer formats.
fn enumerate(flow: EDataFlow) -> Result<Vec<Endpoint>, String> {
    let e = enumerator()?;
    let list = unsafe { e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }.map_err(|e| e.to_string())?;
    let count = unsafe { list.GetCount() }.map_err(|e| e.to_string())?;
    let mut out: Vec<Endpoint> = vec![];
    for i in 0..count {
        let Ok(device) = (unsafe { list.Item(i) }) else { continue };
        let Some(id) = (unsafe { device_id(&device) }) else { continue };
        let mut name = unsafe { friendly_name(&device) }.unwrap_or_else(|| format!("Device {}", i + 1));
        // Names are how projects refer to devices: keep them unique.
        if out.iter().any(|o| o.name == name) {
            name = format!("{name} ({})", i + 1);
        }
        let Ok(client) = (unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }) else { continue };
        let Ok(mix) = (unsafe { client.GetMixFormat() }) else { continue };
        let described = unsafe { describe(mix) };
        unsafe { CoTaskMemFree(Some(mix as *const _)) };
        if let Some((_, channels, rate, _)) = described {
            out.push(Endpoint { id, name, channels, rate });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_conversion_round_trips() {
        let src = [0.0f32, 0.5, -0.5, 0.999, -1.0];
        for sample in [Sample::F32, Sample::I16, Sample::I24, Sample::I32 { valid: 24 }] {
            let mut bytes = vec![0u8; src.len() * sample.bytes()];
            encode(&src, sample, &mut bytes);
            let back: Vec<f32> = read_samples(&bytes, sample).collect();
            for (a, b) in src.iter().zip(&back) {
                assert!((a - b).abs() < 1e-3, "{sample:?}: {a} -> {b}");
            }
        }
    }

    #[test]
    fn period_sizes_follow_the_engine_grid() {
        // Shared-mode engine grids (min, max, fundamental, default).
        assert_eq!(offered_sizes(128, 480, 32, 480), [128, 256, 480]);
        assert_eq!(offered_sizes(144, 480, 48, 480), [144, 192, 240, 288, 336, 384, 432, 480]);
        assert_eq!(offered_sizes(128, 4096, 1, 480), [128, 256, 480, 512, 1024, 2048, 4096]);
        assert_eq!(frames(hns(441, 44100), 44100), 441);
    }
}
