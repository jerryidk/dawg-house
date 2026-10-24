//! Real-time audio engine running inside a JACK client. Works with jackd and
//! with PipeWire's JACK emulation (pipewire-jack).
//!
//! The UI talks to the audio thread through a lock-free command queue; objects
//! that must not be freed on the audio thread come back on a "garbage" queue.
//! Clip layout is published as an immutable `Arrangement` snapshot.

use crate::audio::AudioData;
use crate::device::{Device, MidiEvent, ProcessContext};
use crate::lv2::MAX_BLOCK;
use crate::project::TrackKind;
use crate::util::AtomicF32;
use arc_swap::ArcSwap;
use jack::{AudioIn, AudioOut, Client, ClientOptions, Control, MidiIn, Port, PortFlags, ProcessScope};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

pub const MAX_TRACKS: usize = 128;
pub const MAX_DEVICES: usize = 16;
const MAX_EVENTS: usize = 2048;
const FADE_MIN: usize = 32;
pub const AUDIO_PORT: &str = "32 bit float mono audio";
pub const MIDI_PORT: &str = "8 bit raw midi";

/// Per-track values the UI writes and the audio thread reads (and vice versa
/// for meters).
pub struct TrackShared {
    pub gain: AtomicF32,
    pub pan: AtomicF32,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    pub monitor: AtomicBool,
    pub armed: AtomicBool,
    pub stereo: AtomicBool,
    pub in_peak: [AtomicF32; 2],
    pub out_peak: [AtomicF32; 2],
    /// Set by the audio thread when MIDI input arrives (activity LED).
    pub midi_activity: AtomicBool,
}

impl Default for TrackShared {
    fn default() -> Self {
        TrackShared {
            gain: AtomicF32::new(1.0),
            pan: AtomicF32::new(0.0),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            monitor: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            stereo: AtomicBool::new(false),
            in_peak: Default::default(),
            out_peak: Default::default(),
            midi_activity: AtomicBool::new(false),
        }
    }
}

#[derive(Default)]
pub struct EngineShared {
    pub playing: AtomicBool,
    pub recording: AtomicBool,
    pub position: AtomicI64,
    /// Timeline position where the current take started.
    pub record_start: AtomicI64,
    pub master_gain: AtomicF32,
    pub master_peak: [AtomicF32; 2],
    pub xruns: AtomicU32,
    pub server_gone: AtomicBool,
    /// Loop range: playback jumps from `loop_end` back to `loop_start`.
    pub loop_enabled: AtomicBool,
    pub loop_start: AtomicI64,
    pub loop_end: AtomicI64,
    /// Metronome. Also clicks during a recording count-in.
    pub metronome: AtomicBool,
    pub metro_samples_per_beat: AtomicF32,
    pub metro_beats_per_bar: AtomicU32,
    /// Timeline position of a downbeat.
    pub metro_anchor: AtomicI64,
    /// Frames captured in the current/last recording.
    pub recorded_frames: AtomicI64,
    /// Current JACK period (frames per cycle), updated by the server.
    pub buffer_size: AtomicU32,
}

pub enum PlayContent {
    Audio { data: Arc<AudioData>, offset: usize },
    /// Events sorted by frame, relative to the clip start.
    Midi { events: Arc<Vec<(u64, [u8; 3])>> },
}

pub struct PlayClip {
    pub start: i64,
    pub length: usize,
    pub gain: f32,
    pub fade_in: usize,
    pub fade_out: usize,
    pub content: PlayContent,
}

#[derive(Default)]
pub struct Arrangement {
    pub tracks: Vec<(u64, Vec<PlayClip>)>,
}

/// A MIDI message captured while recording.
#[derive(Clone, Copy, Debug)]
pub struct MidiRecEvent {
    pub track: u64,
    /// `record_start` + frames recorded so far. Equals the timeline position
    /// unless the transport looped.
    pub pos: i64,
    pub data: [u8; 3],
}

pub struct EngineTrack {
    id: u64,
    kind: TrackKind,
    shared: Arc<TrackShared>,
    audio_in: Option<[Port<AudioIn>; 2]>,
    midi_in: Option<Port<MidiIn>>,
    devices: Vec<Box<dyn Device>>,
    recorder: Option<(rtrb::Producer<f32>, usize)>,
    events: Vec<MidiEvent>,
    /// Bit mask of MIDI channels holding each note, to release on stop.
    active_notes: [u16; 128],
    panic: bool,
}

pub enum Command {
    AddTrack(Box<EngineTrack>),
    RemoveTrack(u64),
    InsertDevice { track: u64, index: usize, device: Box<dyn Device> },
    RemoveDevice { track: u64, index: usize },
    MoveDevice { track: u64, from: usize, to: usize },
    Play,
    Stop,
    Locate(i64),
    /// Start recording. Audio tracks get (track id, producer, channel count);
    /// armed instrument tracks record MIDI automatically. Capture begins when
    /// the playhead reaches `from` (anything before is count-in).
    /// The playhead jumps to `start_at` (negative for a count-in).
    Record { takes: Vec<(u64, rtrb::Producer<f32>, usize)>, from: i64, start_at: i64 },
}

enum Garbage {
    Track(Box<EngineTrack>),
    Device(#[allow(dead_code)] Box<dyn Device>),
    Producer(#[allow(dead_code)] rtrb::Producer<f32>),
    Takes(#[allow(dead_code)] Vec<(u64, rtrb::Producer<f32>, usize)>),
}

// ---------------------------------------------------------------------------
// Audio thread

pub struct Processor {
    shared: Arc<EngineShared>,
    arrangement: Arc<ArcSwap<Arrangement>>,
    commands: rtrb::Consumer<Command>,
    garbage: rtrb::Producer<Garbage>,
    midi_rec: rtrb::Producer<MidiRecEvent>,
    // Boxed on purpose: tracks arrive and leave as Boxes, and unboxing here
    // would free the allocation on the audio thread.
    #[allow(clippy::vec_box)]
    tracks: Vec<Box<EngineTrack>>,
    out_l: Port<AudioOut>,
    out_r: Port<AudioOut>,
    buf: [Vec<f32>; 2],
    mix: [Vec<f32>; 2],
    record_from: i64,
    /// Frames captured since `record_from`.
    rec_clock: i64,
    sample_rate: f64,
}

impl Processor {
    fn trash(&mut self, g: Garbage) {
        if let Err(rtrb::PushError::Full(g)) = self.garbage.push(g) {
            // Never deallocate on the audio thread; leaking is the lesser evil.
            std::mem::forget(g);
        }
    }

    fn handle_commands(&mut self) {
        let shared = self.shared.clone();
        for _ in 0..64 {
            let Ok(cmd) = self.commands.pop() else { break };
            match cmd {
                Command::AddTrack(t) => {
                    if self.tracks.len() < self.tracks.capacity() {
                        self.tracks.push(t);
                    } else {
                        self.trash(Garbage::Track(t));
                    }
                }
                Command::RemoveTrack(id) => {
                    if let Some(i) = self.tracks.iter().position(|t| t.id == id) {
                        let t = self.tracks.remove(i);
                        self.trash(Garbage::Track(t));
                    }
                }
                Command::InsertDevice { track, index, device } => match self.track_mut(track) {
                    Some(t) if t.devices.len() < MAX_DEVICES => {
                        let i = index.min(t.devices.len());
                        t.devices.insert(i, device);
                    }
                    _ => self.trash(Garbage::Device(device)),
                },
                Command::RemoveDevice { track, index } => {
                    let removed = self.track_mut(track).filter(|t| index < t.devices.len()).map(|t| {
                        t.panic = true;
                        t.devices.remove(index)
                    });
                    if let Some(d) = removed {
                        self.trash(Garbage::Device(d));
                    }
                }
                Command::MoveDevice { track, from, to } => {
                    if let Some(t) = self.track_mut(track)
                        && from < t.devices.len()
                        && to < t.devices.len()
                    {
                        let d = t.devices.remove(from);
                        t.devices.insert(to, d);
                    }
                }
                Command::Play => shared.playing.store(true, Ordering::Relaxed),
                Command::Stop => {
                    shared.playing.store(false, Ordering::Relaxed);
                    self.stop_recording();
                    self.panic_all();
                }
                Command::Locate(pos) => {
                    shared.position.store(pos.max(0), Ordering::Relaxed);
                    self.panic_all();
                }
                Command::Record { mut takes, from, start_at } => {
                    shared.position.store(start_at, Ordering::Relaxed);
                    shared.recorded_frames.store(0, Ordering::Relaxed);
                    self.stop_recording();
                    self.record_from = from;
                    self.rec_clock = 0;
                    for (id, producer, channels) in takes.drain(..) {
                        match self.track_mut(id) {
                            Some(t) => t.recorder = Some((producer, channels)),
                            None => self.trash(Garbage::Producer(producer)),
                        }
                    }
                    self.trash(Garbage::Takes(takes));
                    shared.record_start.store(from, Ordering::Relaxed);
                    shared.recording.store(true, Ordering::Relaxed);
                    shared.playing.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    fn panic_all(&mut self) {
        for t in &mut self.tracks {
            t.panic = true;
        }
    }

    fn stop_recording(&mut self) {
        self.shared.recording.store(false, Ordering::Relaxed);
        for i in 0..self.tracks.len() {
            if let Some((p, _)) = self.tracks[i].recorder.take() {
                self.trash(Garbage::Producer(p));
            }
        }
    }

    fn track_mut(&mut self, id: u64) -> Option<&mut Box<EngineTrack>> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }
}

fn peak(buf: &[f32]) -> f32 {
    buf.iter().fold(0f32, |m, s| m.max(s.abs()))
}

fn push_event(events: &mut Vec<MidiEvent>, e: MidiEvent) {
    if events.len() < events.capacity() {
        events.push(e);
    }
}

/// Loop arithmetic for one block of `n` frames (`total` = full cycle size).
/// Returns (play position, split, next position): frames `[0, split)` play
/// from the returned position and frames `[split, n)` from `ls`.
///
/// A position at or past the loop end wraps first. This covers a block that
/// ended exactly on the loop end (loop length a multiple of the block size,
/// e.g. whole bars at 120 BPM / 48 kHz) and a loop that was shortened.
/// Positions before the loop start (count-in) play on until they reach it.
pub(crate) fn loop_block(pos: i64, n: usize, total: usize, looping: bool, ls: i64, le: i64) -> (i64, usize, i64) {
    if !looping || le <= ls {
        return (pos, n, pos + total as i64);
    }
    let len = le - ls;
    let pos = if pos >= le { ls + (pos - ls).rem_euclid(len) } else { pos };
    let split = if pos + n as i64 > le { (le - pos) as usize } else { n };
    let mut next = if split < n { ls + (total - split) as i64 } else { pos + total as i64 };
    if next >= le {
        next = ls + (next - ls).rem_euclid(len);
    }
    (pos, split, next)
}

/// Run a track's device chain in place. On instrument tracks only the first
/// device (the instrument) receives the notes. Shared with offline export.
pub(crate) fn run_chain(devices: &mut [Box<dyn Device>], kind: TrackKind, l: &mut [f32], r: &mut [f32], midi: &[MidiEvent]) {
    let mut ctx = ProcessContext { left: l, right: r, midi };
    for (i, d) in devices.iter_mut().enumerate() {
        d.process(&mut ctx);
        if i == 0 && kind == TrackKind::Instrument {
            ctx.midi = &[];
        }
    }
}

/// Apply fader gain and balance-style pan and add the track into the mix.
/// Returns the post-fader peaks. Shared with offline export.
pub(crate) fn mix_into(l: &[f32], r: &[f32], ml: &mut [f32], mr: &mut [f32], gain: f32, pan: f32, audible: bool) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    let gl = gain * if pan > 0.0 { 1.0 - pan } else { 1.0 };
    let gr = gain * if pan < 0.0 { 1.0 + pan } else { 1.0 };
    let (mut pl, mut pr) = (0f32, 0f32);
    if audible {
        for i in 0..l.len().min(ml.len()) {
            let (a, b) = (l[i] * gl, r[i] * gr);
            pl = pl.max(a.abs());
            pr = pr.max(b.abs());
            ml[i] += a;
            mr[i] += b;
        }
    }
    (pl, pr)
}

/// Render clips for timeline range `pos..pos + l.len()` into `l`/`r`; MIDI
/// events are stamped `frame_offset` + their position in the range.
pub(crate) fn render_clips(clips: &[PlayClip], pos: i64, l: &mut [f32], r: &mut [f32], events: &mut Vec<MidiEvent>, frame_offset: usize) {
    let n = l.len() as i64;
    for c in clips {
        let (cs, ce) = (c.start, c.start + c.length as i64);
        let (a, b) = (cs.max(pos), ce.min(pos + n));
        if a >= b {
            continue;
        }
        match &c.content {
            PlayContent::Audio { data, offset } => {
                let fi = c.fade_in.max(FADE_MIN) as f32;
                let fo = c.fade_out.max(FADE_MIN) as f32;
                for t in a..b {
                    let rel = (t - cs) as usize;
                    let mut g = c.gain;
                    if (rel as f32) < fi {
                        g *= rel as f32 / fi;
                    }
                    let to_end = (c.length - rel) as f32;
                    if to_end < fo {
                        g *= to_end / fo;
                    }
                    let src = offset + rel;
                    let i = (t - pos) as usize;
                    l[i] += data.sample(0, src) * g;
                    r[i] += data.sample(1, src) * g;
                }
            }
            PlayContent::Midi { events: list } => {
                let (ra, rb) = ((a - cs) as u64, (b - cs) as u64);
                let first = list.partition_point(|e| e.0 < ra);
                for &(rel, data) in list[first..].iter().take_while(|e| e.0 < rb) {
                    let len = if data[0] & 0xF0 == 0xC0 || data[0] & 0xF0 == 0xD0 { 2 } else { 3 };
                    push_event(events, MidiEvent::new((cs + rel as i64 - pos) as u32 + frame_offset as u32, &data[..len]));
                }
            }
        }
    }
}

impl jack::ProcessHandler for Processor {
    fn buffer_size(&mut self, _: &Client, size: jack::Frames) -> Control {
        self.shared.buffer_size.store(size, Ordering::Relaxed);
        Control::Continue
    }

    fn process(&mut self, _: &Client, ps: &ProcessScope) -> Control {
        self.handle_commands();
        let shared = self.shared.clone();
        let arr = self.arrangement.load();
        let total = ps.n_frames() as usize;
        // Blocks larger than MAX_BLOCK (never seen in practice) are truncated.
        let n = total.min(MAX_BLOCK);
        let playing = shared.playing.load(Ordering::Relaxed);
        let recording = shared.recording.load(Ordering::Relaxed);
        let pos0 = shared.position.load(Ordering::Relaxed);
        let any_solo = self.tracks.iter().any(|t| t.shared.solo.load(Ordering::Relaxed));

        // Loop: frames [0, split) play at `pos`, frames [split, n) at `loop_start`.
        let (ls, le) = (shared.loop_start.load(Ordering::Relaxed), shared.loop_end.load(Ordering::Relaxed));
        let looping = playing && shared.loop_enabled.load(Ordering::Relaxed);
        let (pos, split, next_pos) = loop_block(pos0, n, total, looping, ls, le);
        // First frame that is captured (frames before `record_from` are count-in).
        let first_rec = if !recording || pos >= self.record_from || split < n {
            0
        } else {
            ((self.record_from - pos) as usize).min(n)
        };

        let [ml, mr] = &mut self.mix;
        ml[..n].fill(0.0);
        mr[..n].fill(0.0);

        for t in self.tracks.iter_mut() {
            let sh = &*t.shared;
            let armed = sh.armed.load(Ordering::Relaxed);
            let monitor = sh.monitor.load(Ordering::Relaxed);
            let [bl, br] = &mut self.buf;
            let (bl, br) = (&mut bl[..n], &mut br[..n]);
            bl.fill(0.0);
            br.fill(0.0);

            // ---- MIDI: release held notes, clip events, live input
            t.events.clear();
            if t.panic {
                t.panic = false;
                for (note, chans) in t.active_notes.iter_mut().enumerate() {
                    for ch in 0..16u8 {
                        if *chans & (1 << ch) != 0 {
                            push_event(&mut t.events, MidiEvent::new(0, &[0x80 | ch, note as u8, 0]));
                        }
                    }
                    *chans = 0;
                }
            }
            if playing && let Some((_, clips)) = arr.tracks.iter().find(|(id, _)| *id == t.id) {
                let (al, bl2) = bl.split_at_mut(split);
                let (ar, br2) = br.split_at_mut(split);
                render_clips(clips, pos, al, ar, &mut t.events, 0);
                if split < n {
                    render_clips(clips, ls, bl2, br2, &mut t.events, split);
                }
            }
            if let Some(port) = &t.midi_in {
                for raw in port.iter(ps) {
                    if raw.bytes.is_empty() || raw.bytes[0] >= 0xF0 || raw.time as usize >= n {
                        continue; // skip sysex / realtime messages
                    }
                    sh.midi_activity.store(true, Ordering::Relaxed);
                    let e = MidiEvent::new(raw.time, raw.bytes);
                    if armed || monitor {
                        push_event(&mut t.events, e);
                    }
                    if recording && armed && raw.time as usize >= first_rec {
                        let clock = self.rec_clock + (raw.time as usize - first_rec) as i64;
                        let _ = self.midi_rec.push(MidiRecEvent { track: t.id, pos: self.record_from + clock, data: e.data });
                    }
                }
            }
            t.events.sort_unstable_by_key(|e| e.frame);
            for e in &t.events {
                let (status, note) = (e.data[0] & 0xF0, (e.data[1] & 0x7F) as usize);
                let ch = e.data[0] & 0x0F;
                match status {
                    0x90 if e.data[2] > 0 => t.active_notes[note] |= 1 << ch,
                    0x80 | 0x90 => t.active_notes[note] &= !(1 << ch),
                    _ => {}
                }
            }

            // ---- Audio input
            if let Some([pl, pr]) = &t.audio_in {
                let in_l = &pl.as_slice(ps)[..n];
                let in_r = if sh.stereo.load(Ordering::Relaxed) { &pr.as_slice(ps)[..n] } else { in_l };
                sh.in_peak[0].max(peak(in_l));
                sh.in_peak[1].max(peak(in_r));
                if recording && let Some((prod, ch)) = &mut t.recorder {
                    if prod.slots() >= n * *ch {
                        for i in first_rec..n {
                            let _ = prod.push(in_l[i]);
                            if *ch == 2 {
                                let _ = prod.push(in_r[i]);
                            }
                        }
                    } else {
                        shared.xruns.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if monitor {
                    for i in 0..n {
                        bl[i] += in_l[i];
                        br[i] += in_r[i];
                    }
                }
            }

            // ---- Device chain, then fader / pan / solo / mute into the mix
            run_chain(&mut t.devices, t.kind, bl, br, &t.events);
            let audible = !sh.mute.load(Ordering::Relaxed) && (!any_solo || sh.solo.load(Ordering::Relaxed));
            let (pl, pr) = mix_into(bl, br, &mut ml[..n], &mut mr[..n], sh.gain.get(), sh.pan.get(), audible);
            sh.out_peak[0].max(pl);
            sh.out_peak[1].max(pr);
        }

        // Metronome (always during a recording count-in).
        let count_in = recording && pos < self.record_from;
        if playing && (count_in || shared.metronome.load(Ordering::Relaxed)) {
            let spb = shared.metro_samples_per_beat.get() as f64;
            let bpb = shared.metro_beats_per_bar.load(Ordering::Relaxed).max(1) as i64;
            let anchor = shared.metro_anchor.load(Ordering::Relaxed);
            let rate = self.sample_rate;
            if spb > 1.0 {
                for i in 0..n {
                    let p = if i < split { pos + i as i64 } else { ls + (i - split) as i64 };
                    let t = (p - anchor) as f64;
                    let beat = (t / spb).floor();
                    let within = t - beat * spb;
                    let click_len = rate * 0.03;
                    if within < click_len {
                        let downbeat = (beat as i64).rem_euclid(bpb) == 0;
                        let f = if downbeat { 1760.0 } else { 1320.0 };
                        let env = 1.0 - within / click_len;
                        let v = ((std::f64::consts::TAU * f * within / rate).sin() * env * env * 0.35) as f32;
                        ml[i] += v;
                        mr[i] += v;
                    }
                }
            }
        }

        let mg = shared.master_gain.get();
        for (port, mix, meter) in [(&mut self.out_l, &*ml, &shared.master_peak[0]), (&mut self.out_r, &*mr, &shared.master_peak[1])] {
            let out = port.as_mut_slice(ps);
            for (o, s) in out.iter_mut().zip(mix[..n].iter()) {
                *o = s * mg;
            }
            out[n..].fill(0.0);
            meter.max(peak(&out[..n]));
        }

        if recording {
            self.rec_clock += (n - first_rec) as i64;
            shared.recorded_frames.store(self.rec_clock, Ordering::Relaxed);
        }
        if playing {
            // Only advance if nobody relocated us during this cycle.
            let _ = shared.position.compare_exchange(pos0, next_pos, Ordering::Relaxed, Ordering::Relaxed);
        }
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

// ---------------------------------------------------------------------------
// UI-side handle

pub struct Engine {
    client: Option<jack::AsyncClient<Notifications, Processor>>,
    pub shared: Arc<EngineShared>,
    commands: rtrb::Producer<Command>,
    garbage: Option<rtrb::Consumer<Garbage>>,
    midi_rec: Option<rtrb::Consumer<MidiRecEvent>>,
    arrangement: Arc<ArcSwap<Arrangement>>,
    retired: Vec<Arc<Arrangement>>,
    pub sample_rate: u32,
    pub error: Option<String>,
    /// True when libjack is PipeWire's JACK implementation.
    pub pipewire: bool,
    /// Without a JACK server, devices are kept here (per track) so they live
    /// as long as they would in the engine (their GUIs may be open).
    offline_devices: std::collections::HashMap<u64, Vec<Box<dyn Device>>>,
}

impl Engine {
    pub fn new(name: &str) -> Self {
        let shared = Arc::new(EngineShared::default());
        shared.master_gain.set(1.0);
        let arrangement = Arc::new(ArcSwap::from_pointee(Arrangement::default()));
        let (commands, cmd_rx) = rtrb::RingBuffer::new(1024);
        let mut engine = Engine {
            client: None,
            shared: shared.clone(),
            commands,
            garbage: None,
            midi_rec: None,
            arrangement: arrangement.clone(),
            retired: vec![],
            sample_rate: 48000,
            error: None,
            pipewire: false,
            offline_devices: Default::default(),
        };
        match Self::start(name, shared, arrangement, cmd_rx) {
            Ok((client, garbage, midi_rec)) => {
                let c = client.as_client();
                engine.sample_rate = c.sample_rate();
                engine.shared.buffer_size.store(c.buffer_size(), Ordering::Relaxed);
                engine.pipewire = libjack_is_pipewire();
                engine.garbage = Some(garbage);
                engine.midi_rec = Some(midi_rec);
                engine.client = Some(client);
                engine.autoconnect_master();
            }
            Err(e) => engine.error = Some(e),
        }
        engine
    }

    #[allow(clippy::type_complexity)]
    fn start(
        name: &str,
        shared: Arc<EngineShared>,
        arrangement: Arc<ArcSwap<Arrangement>>,
        commands: rtrb::Consumer<Command>,
    ) -> Result<
        (jack::AsyncClient<Notifications, Processor>, rtrb::Consumer<Garbage>, rtrb::Consumer<MidiRecEvent>),
        String,
    > {
        let (client, _status) = Client::new(name, ClientOptions::NO_START_SERVER)
            .map_err(|e| format!("Could not connect to JACK/PipeWire: {e}. Is pipewire-jack installed (or jackd running)?"))?;
        let out_l = client.register_port("master_out_L", AudioOut::default()).map_err(|e| e.to_string())?;
        let out_r = client.register_port("master_out_R", AudioOut::default()).map_err(|e| e.to_string())?;
        let (gtx, grx) = rtrb::RingBuffer::new(1024);
        let (mtx, mrx) = rtrb::RingBuffer::new(16384);
        let processor = Processor {
            shared: shared.clone(),
            arrangement,
            commands,
            garbage: gtx,
            midi_rec: mtx,
            tracks: Vec::with_capacity(MAX_TRACKS),
            out_l,
            out_r,
            buf: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            mix: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            record_from: 0,
            rec_clock: 0,
            sample_rate: client.sample_rate() as f64,
        };
        let client = client.activate_async(Notifications { shared }, processor).map_err(|e| e.to_string())?;
        Ok((client, grx, mrx))
    }

    pub fn is_running(&self) -> bool {
        self.client.is_some() && !self.shared.server_gone.load(Ordering::Relaxed)
    }

    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref().map(|c| c.as_client())
    }

    pub fn cpu_load(&self) -> f32 {
        self.client().map_or(0.0, |c| c.cpu_load())
    }

    pub fn send(&mut self, cmd: Command) {
        if self.client.is_some() {
            // If the queue is full the command is dropped (on this thread).
            let _ = self.commands.push(cmd);
        } else {
            // Offline: emulate the transport so the UI stays usable.
            match cmd {
                Command::Play => self.shared.playing.store(true, Ordering::Relaxed),
                Command::Stop => {
                    self.shared.playing.store(false, Ordering::Relaxed);
                    self.shared.recording.store(false, Ordering::Relaxed);
                }
                Command::Locate(p) => self.shared.position.store(p.max(0), Ordering::Relaxed),
                Command::InsertDevice { track, index, device } => {
                    let list = self.offline_devices.entry(track).or_default();
                    list.insert(index.min(list.len()), device);
                }
                Command::RemoveDevice { track, index } => {
                    if let Some(list) = self.offline_devices.get_mut(&track)
                        && index < list.len()
                    {
                        list.remove(index);
                    }
                }
                Command::MoveDevice { track, from, to } => {
                    if let Some(list) = self.offline_devices.get_mut(&track)
                        && from < list.len()
                        && to < list.len()
                    {
                        let d = list.remove(from);
                        list.insert(to, d);
                    }
                }
                Command::RemoveTrack(id) => {
                    self.offline_devices.remove(&id);
                }
                _ => {}
            }
        }
    }

    /// Register ports for a new track and hand it to the audio thread.
    pub fn add_track(&mut self, id: u64, kind: TrackKind, shared: Arc<TrackShared>) -> Result<(), String> {
        let Some(client) = self.client() else { return Ok(()) };
        let (audio_in, midi_in) = match kind {
            TrackKind::Audio => {
                let l = client.register_port(&format!("track{id}_in_L"), AudioIn::default()).map_err(|e| e.to_string())?;
                let r = client.register_port(&format!("track{id}_in_R"), AudioIn::default()).map_err(|e| e.to_string())?;
                (Some([l, r]), None)
            }
            TrackKind::Instrument => {
                let m = client.register_port(&format!("track{id}_midi_in"), MidiIn::default()).map_err(|e| e.to_string())?;
                (None, Some(m))
            }
        };
        let track = EngineTrack {
            id,
            kind,
            shared,
            audio_in,
            midi_in,
            devices: Vec::with_capacity(MAX_DEVICES),
            recorder: None,
            events: Vec::with_capacity(MAX_EVENTS),
            active_notes: [0; 128],
            panic: false,
        };
        self.send(Command::AddTrack(Box::new(track)));
        Ok(())
    }

    pub fn port_name(&self, short: &str) -> String {
        format!("{}:{short}", self.client().map_or("studio", |c| c.name()))
    }

    /// Output ports of other clients of the given type, physical ones first.
    pub fn source_ports(&self, port_type: &str) -> Vec<String> {
        let Some(c) = self.client() else { return vec![] };
        let own = format!("{}:", c.name());
        let mut ports: Vec<String> =
            c.ports(None, Some(port_type), PortFlags::IS_OUTPUT).into_iter().filter(|p| !p.starts_with(&own)).collect();
        let physical = c.ports(None, Some(port_type), PortFlags::IS_OUTPUT | PortFlags::IS_PHYSICAL);
        ports.sort_by_key(|p| !physical.contains(p));
        ports
    }

    /// Connect one of our input ports exclusively to `source`.
    pub fn connect_input(&self, short: &str, source: Option<&str>) {
        let Some(c) = self.client() else { return };
        let dst = self.port_name(short);
        if let Some(p) = c.port_by_name(&dst) {
            let _ = c.disconnect(&p);
        }
        if let Some(src) = source {
            let _ = c.connect_ports_by_name(src, &dst);
        }
    }

    fn autoconnect_master(&self) {
        if let Some((_, ports)) = self.playback_devices().into_iter().next() {
            self.set_master_output(&ports);
        }
    }

    /// Frames per JACK cycle (0 when offline).
    pub fn buffer_size(&self) -> u32 {
        self.shared.buffer_size.load(Ordering::Relaxed)
    }

    /// Ask the server for a new period size. Under PipeWire this forces the
    /// graph quantum. The actual size arrives via the buffer-size callback.
    pub fn set_buffer_size(&self, frames: u32) -> Result<(), String> {
        let c = self.client().ok_or("audio engine offline")?;
        c.set_buffer_size(frames).map_err(|e| format!("could not set buffer size to {frames}: {e}"))
    }

    /// Output devices: hardware playback ports grouped by client, in order.
    pub fn playback_devices(&self) -> Vec<(String, Vec<String>)> {
        let Some(c) = self.client() else { return vec![] };
        let mut devices: Vec<(String, Vec<String>)> = vec![];
        for port in c.ports(None, Some(AUDIO_PORT), PortFlags::IS_INPUT | PortFlags::IS_PHYSICAL) {
            let client = port.split(':').next().unwrap_or("").to_string();
            match devices.iter_mut().find(|(d, _)| *d == client) {
                Some((_, ports)) => ports.push(port),
                None => devices.push((client, vec![port])),
            }
        }
        devices
    }

    /// Client name of the device the master output currently feeds.
    pub fn master_output(&self) -> Option<String> {
        let c = self.client()?;
        let port = c.port_by_name(&self.port_name("master_out_L"))?;
        port.get_connections().first().map(|p| p.split(':').next().unwrap_or("").to_string())
    }

    /// Route the master output to `ports` (L to the first, R to the second;
    /// a mono device gets both). An empty slice disconnects the master.
    pub fn set_master_output(&self, ports: &[String]) {
        let Some(c) = self.client() else { return };
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

    pub fn set_arrangement(&mut self, arr: Arrangement) {
        let old = self.arrangement.swap(Arc::new(arr));
        self.retired.push(old);
    }

    /// MIDI captured by the audio thread since the last call.
    pub fn take_midi_recording(&mut self) -> Vec<MidiRecEvent> {
        let mut out = vec![];
        if let Some(rx) = &mut self.midi_rec {
            while let Ok(e) = rx.pop() {
                out.push(e);
            }
        }
        out
    }

    /// Free objects the audio thread has handed back. Call once per UI frame.
    pub fn collect_garbage(&mut self) {
        self.retired.retain(|a| Arc::strong_count(a) > 1);
        let Some(rx) = &mut self.garbage else { return };
        let mut tracks = vec![];
        while let Ok(g) = rx.pop() {
            if let Garbage::Track(t) = g {
                tracks.push(t);
            }
        }
        if let Some(c) = self.client.as_ref().map(|c| c.as_client()) {
            for t in tracks {
                let t = *t;
                if let Some([l, r]) = t.audio_in {
                    let _ = c.unregister_port(l);
                    let _ = c.unregister_port(r);
                }
                if let Some(m) = t.midi_in {
                    let _ = c.unregister_port(m);
                }
            }
        }
    }
}

fn libjack_is_pipewire() -> bool {
    std::fs::read_to_string("/proc/self/maps")
        .map(|m| m.lines().any(|l| l.contains("libjack") && l.contains("pipewire")))
        .unwrap_or(false)
}
