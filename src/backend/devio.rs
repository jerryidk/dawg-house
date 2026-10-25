//! Device-style audio I/O for backends that drive a sound card directly
//! (WASAPI / ASIO through cpal on Windows). Platform independent, so it is
//! unit-tested on every OS.
//!
//! Model: one output device with M channels and any number of input devices
//! with N channels each. A track picks its input channels
//! (`TrackShared::in_channels`, see [`channel_id`]) and MIDI source
//! (`TrackShared::midi_source`); the master goes to the first two output
//! channels. Input and output arrive in separate driver callbacks, so captured
//! input reaches the output callback, which runs the processor, through one
//! ring buffer per input device.

use super::{CycleIo, TrackPorts};
use crate::engine::{EngineShared, Processor, TrackShared};
use crate::lv2::MAX_BLOCK;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Input kept queued at most, in blocks: older input is dropped so the
/// input → output latency cannot grow when the two device clocks drift.
const MAX_QUEUED_BLOCKS: usize = 4;
const MAX_MIDI_PER_CYCLE: usize = 1024;
/// Channels per input device in a channel id.
pub const MAX_DEVICE_CHANNELS: usize = 64;

/// Channel id stored in `TrackShared::in_channels`: input device `device`
/// (an index into the backend's input device list), channel `channel`.
pub fn channel_id(device: usize, channel: usize) -> i32 {
    (device * MAX_DEVICE_CHANNELS + channel.min(MAX_DEVICE_CHANNELS - 1)) as i32
}

/// A capture stream feeding the runner: interleaved samples of `channels`
/// channels from input device `device`.
pub struct DeviceInput {
    pub device: usize,
    pub channels: usize,
    pub rx: rtrb::Consumer<f32>,
}

/// An input device's ring and this cycle's deinterleaved channels.
pub struct InputState {
    device: usize,
    channels: usize,
    rx: rtrb::Consumer<f32>,
    bufs: Vec<Vec<f32>>,
    /// Input has arrived at least once (underruns before that are start-up).
    started: bool,
}

/// A short MIDI message from MIDI input port `source`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiInEvent {
    pub source: i32,
    pub len: u8,
    pub data: [u8; 3],
}

impl MidiInEvent {
    /// `None` for SysEx and other messages longer than three bytes.
    pub fn new(source: i32, bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > 3 {
            return None;
        }
        let mut data = [0u8; 3];
        data[..bytes.len()].copy_from_slice(bytes);
        Some(MidiInEvent { source, len: bytes.len() as u8, data })
    }
}

/// Split interleaved samples (`a` then `b`: one possibly wrapped ring read)
/// into per-channel buffers.
pub fn deinterleave(a: &[f32], b: &[f32], ch: usize, bufs: &mut [Vec<f32>]) {
    for (i, s) in a.iter().chain(b).enumerate() {
        bufs[i % ch][i / ch] = *s;
    }
}

/// One cycle of device buffers.
pub struct DeviceIo<'a> {
    pub frames: usize,
    /// Captured input of every open input device.
    pub inputs: &'a [InputState],
    pub midi: &'a [MidiInEvent],
    pub out_l: &'a mut [f32],
    pub out_r: &'a mut [f32],
}

impl CycleIo for DeviceIo<'_> {
    fn frames(&self) -> usize {
        self.frames
    }

    fn track_audio<'a>(&'a self, ports: &'a TrackPorts, shared: &TrackShared) -> Option<(&'a [f32], &'a [f32])> {
        if !matches!(ports, TrackPorts::Device) {
            return None;
        }
        let channel = |side: usize| {
            let id = usize::try_from(shared.in_channels[side].load(Ordering::Relaxed)).ok()?;
            let (dev, ch) = (id / MAX_DEVICE_CHANNELS, id % MAX_DEVICE_CHANNELS);
            let input = self.inputs.iter().find(|i| i.device == dev)?;
            input.bufs.get(ch).map(|b| &b[..self.frames])
        };
        let l = channel(0)?;
        Some((l, channel(1).unwrap_or(l)))
    }

    fn track_midi(&self, ports: &TrackPorts, shared: &TrackShared, f: &mut dyn FnMut(u32, &[u8])) {
        if !matches!(ports, TrackPorts::Device) {
            return;
        }
        let source = shared.midi_source.load(Ordering::Relaxed);
        for e in self.midi.iter().filter(|e| source == -2 || (source >= 0 && e.source == source)) {
            f(0, &e.data[..e.len as usize]);
        }
    }

    fn master_out(&mut self) -> (&mut [f32], &mut [f32]) {
        (&mut *self.out_l, &mut *self.out_r)
    }
}

impl InputState {
    /// Channel buffers of `block` frames for one input device.
    pub(crate) fn new(input: DeviceInput, block: usize) -> Self {
        let channels = input.channels.max(1);
        InputState { device: input.device, channels, rx: input.rx, bufs: vec![vec![0.0; block]; channels], started: false }
    }

    /// Fill the channel buffers with `n` frames; true on an underrun.
    fn pull(&mut self, n: usize) -> bool {
        let ch = self.channels;
        let rx = &mut self.rx;
        let want = n * ch;
        // Keep latency bounded: drop the oldest input beyond a few blocks.
        let surplus = rx.slots().saturating_sub(want * MAX_QUEUED_BLOCKS) / ch * ch;
        if surplus > 0
            && let Ok(chunk) = rx.read_chunk(surplus)
        {
            chunk.commit_all();
        }
        let avail = rx.slots().min(want) / ch * ch;
        if let Ok(chunk) = rx.read_chunk(avail) {
            let (a, b) = chunk.as_slices();
            deinterleave(a, b, ch, &mut self.bufs);
            chunk.commit_all();
        }
        let got = avail / ch;
        self.started |= got > 0;
        if got < n {
            for buf in &mut self.bufs {
                buf[got..n].fill(0.0);
            }
            return self.started;
        }
        false
    }
}

/// Runs the processor from a device's output callback.
pub struct DeviceRunner {
    processor: Processor,
    shared: Arc<EngineShared>,
    inputs: Vec<InputState>,
    midi: rtrb::Consumer<MidiInEvent>,
    midi_buf: Vec<MidiInEvent>,
    out: [Vec<f32>; 2],
    rate: u32,
    /// Period reported in `EngineShared::buffer_size` (0 = frames per render
    /// call, which varies under WASAPI shared mode).
    period: u32,
}

impl DeviceRunner {
    pub fn new(mut processor: Processor, shared: Arc<EngineShared>, rate: u32, midi: rtrb::Consumer<MidiInEvent>) -> Self {
        processor.set_sample_rate(rate as f64);
        DeviceRunner {
            processor,
            shared,
            inputs: vec![],
            midi,
            midi_buf: Vec::with_capacity(MAX_MIDI_PER_CYCLE),
            out: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            rate,
            period: 0,
        }
    }

    /// Replace the capture streams. Allocates: call it holding the runner's
    /// lock (the output callback then renders silence for a moment).
    pub fn set_inputs(&mut self, inputs: Vec<DeviceInput>) {
        self.inputs = inputs.into_iter().map(|i| InputState::new(i, MAX_BLOCK)).collect();
    }

    /// Input devices currently attached.
    #[cfg(test)]
    pub fn input_devices(&self) -> Vec<usize> {
        self.inputs.iter().map(|i| i.device).collect()
    }

    /// Fix the period shown in the UI (0 = frames per render call).
    pub fn set_period(&mut self, frames: u32) {
        self.period = frames;
    }

    /// Take the processor back (when the driver could not be started).
    #[cfg_attr(not(all(windows, feature = "asio")), allow(dead_code))]
    pub fn into_processor(self) -> Processor {
        self.processor
    }

    pub fn shared(&self) -> &EngineShared {
        &self.shared
    }

    pub fn set_rate(&mut self, rate: u32) {
        self.rate = rate;
        self.processor.set_sample_rate(rate as f64);
    }

    /// Fill an interleaved device output buffer with `channels` channels.
    /// Real-time safe.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        let started = Instant::now();
        let channels = channels.max(1);
        let frames = out.len() / channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            self.pull_input(n);
            self.pull_midi();
            {
                let [ol, or] = &mut self.out;
                let mut io = DeviceIo { frames: n, inputs: &self.inputs, midi: &self.midi_buf, out_l: &mut ol[..n], out_r: &mut or[..n] };
                self.processor.process(&mut io);
            }
            for i in 0..n {
                let f = &mut out[(done + i) * channels..(done + i + 1) * channels];
                f[0] = self.out[0][i];
                if channels > 1 {
                    f[1] = self.out[1][i];
                    f[2..].fill(0.0);
                }
            }
            done += n;
        }
        let period = if self.period > 0 { self.period } else { frames as u32 };
        self.shared.buffer_size.store(period, Ordering::Relaxed);
        let period = frames as f32 / self.rate.max(1) as f32;
        if period > 0.0 {
            self.shared.dsp_load.set(started.elapsed().as_secs_f32() / period);
        }
    }

    /// Deinterleave `n` frames of each input device's capture.
    fn pull_input(&mut self, n: usize) {
        for input in &mut self.inputs {
            if input.pull(n) {
                self.shared.xruns.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn pull_midi(&mut self) {
        self.midi_buf.clear();
        while self.midi_buf.len() < MAX_MIDI_PER_CYCLE {
            let Ok(e) = self.midi.pop() else { break };
            self.midi_buf.push(e);
        }
    }
}
