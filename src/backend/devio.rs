//! Device-style audio I/O for backends that drive a sound card directly
//! (WASAPI / ASIO through cpal on Windows). Platform independent, so it is
//! unit-tested on every OS.
//!
//! Model: the device has N input and M output channels. A track picks its
//! input channels (`TrackShared::in_channels`) and MIDI source
//! (`TrackShared::midi_source`); the master goes to the first two output
//! channels. Input and output arrive in separate driver callbacks, so captured
//! input reaches the output callback, which runs the processor, through a
//! ring buffer.

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

/// One cycle of device buffers.
pub struct DeviceIo<'a> {
    pub frames: usize,
    /// Captured input, one buffer per device channel.
    pub inputs: &'a [Vec<f32>],
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
        let ch = |side: usize| shared.in_channels[side].load(Ordering::Relaxed);
        let l = usize::try_from(ch(0)).ok().filter(|&c| c < self.inputs.len())?;
        let r = usize::try_from(ch(1)).ok().filter(|&c| c < self.inputs.len()).unwrap_or(l);
        Some((&self.inputs[l][..self.frames], &self.inputs[r][..self.frames]))
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

/// Runs the processor from a device's output callback.
pub struct DeviceRunner {
    processor: Processor,
    shared: Arc<EngineShared>,
    /// Interleaved captured input and its channel count.
    input: Option<(rtrb::Consumer<f32>, usize)>,
    midi: rtrb::Consumer<MidiInEvent>,
    in_bufs: Vec<Vec<f32>>,
    midi_buf: Vec<MidiInEvent>,
    out: [Vec<f32>; 2],
    rate: u32,
    /// Input has arrived at least once (underruns before that are start-up).
    input_started: bool,
}

impl DeviceRunner {
    pub fn new(mut processor: Processor, shared: Arc<EngineShared>, rate: u32, midi: rtrb::Consumer<MidiInEvent>) -> Self {
        processor.set_sample_rate(rate as f64);
        DeviceRunner {
            processor,
            shared,
            input: None,
            midi,
            in_bufs: vec![],
            midi_buf: Vec::with_capacity(MAX_MIDI_PER_CYCLE),
            out: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            rate,
            input_started: false,
        }
    }

    /// Attach a capture stream (`None` = no input). Allocates; call while the
    /// output stream is stopped.
    pub fn set_input(&mut self, input: Option<(rtrb::Consumer<f32>, usize)>) {
        let channels = input.as_ref().map_or(0, |(_, c)| *c);
        self.in_bufs = vec![vec![0.0; MAX_BLOCK]; channels];
        self.input = input;
        self.input_started = false;
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
                let mut io = DeviceIo { frames: n, inputs: &self.in_bufs, midi: &self.midi_buf, out_l: &mut ol[..n], out_r: &mut or[..n] };
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
        self.shared.buffer_size.store(frames as u32, Ordering::Relaxed);
        let period = frames as f32 / self.rate.max(1) as f32;
        if period > 0.0 {
            self.shared.dsp_load.set(started.elapsed().as_secs_f32() / period);
        }
    }

    /// Deinterleave `n` frames of captured input into the channel buffers.
    fn pull_input(&mut self, n: usize) {
        let Some((rx, ch)) = &mut self.input else { return };
        let ch = (*ch).max(1);
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
            for (i, s) in a.iter().chain(b).enumerate() {
                self.in_bufs[i % ch][i / ch] = *s;
            }
            chunk.commit_all();
        }
        let got = avail / ch;
        self.input_started |= got > 0;
        if got < n {
            for buf in &mut self.in_bufs {
                buf[got..n].fill(0.0);
            }
            if self.input_started {
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
