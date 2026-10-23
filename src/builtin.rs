//! Built-in devices. They implement the same [`Device`] trait as LV2 plugins,
//! so the engine treats them identically.

use crate::device::{Device, DeviceCategory, DeviceInfo, DeviceShared, ParamInfo, ProcessContext};
use crate::util::db_to_gain;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub fn catalogue() -> Vec<DeviceInfo> {
    vec![
        DeviceInfo {
            id: "builtin:synth".into(),
            name: "Basic Synth".into(),
            category: DeviceCategory::Instrument,
            class: "Built-in".into(),
            params: vec![
                ParamInfo::new("wave", "Waveform", 0.0, 3.0, 1.0).choices(&["Sine", "Saw", "Square", "Triangle"]),
                ParamInfo::new("attack", "Attack (ms)", 1.0, 2000.0, 5.0).log(),
                ParamInfo::new("decay", "Decay (ms)", 1.0, 4000.0, 250.0).log(),
                ParamInfo::new("sustain", "Sustain", 0.0, 1.0, 0.6),
                ParamInfo::new("release", "Release (ms)", 1.0, 5000.0, 300.0).log(),
                ParamInfo::new("cutoff", "Cutoff (Hz)", 60.0, 18000.0, 4000.0).log(),
                ParamInfo::new("volume", "Volume (dB)", -40.0, 6.0, -10.0),
            ],
            outputs: vec![],
            unsupported: None,
            has_gui: false,
        },
        DeviceInfo {
            id: "builtin:utility".into(),
            name: "Utility".into(),
            category: DeviceCategory::Effect,
            class: "Built-in".into(),
            params: vec![
                ParamInfo::new("gain", "Gain (dB)", -60.0, 24.0, 0.0),
                ParamInfo::new("width", "Stereo width", 0.0, 2.0, 1.0),
                ParamInfo { toggled: true, ..ParamInfo::new("invert_l", "Invert L", 0.0, 1.0, 0.0) },
                ParamInfo { toggled: true, ..ParamInfo::new("invert_r", "Invert R", 0.0, 1.0, 0.0) },
            ],
            outputs: vec![],
            unsupported: None,
            has_gui: false,
        },
    ]
}

pub fn create(name: &str, rate: f32, shared: Arc<DeviceShared>) -> Option<Box<dyn Device>> {
    match name {
        "synth" => Some(Box::new(BasicSynth::new(rate, shared))),
        "utility" => Some(Box::new(Utility { shared })),
        _ => None,
    }
}

// ---------------------------------------------------------------------------

struct Utility {
    shared: Arc<DeviceShared>,
}

impl Device for Utility {
    fn process(&mut self, ctx: &mut ProcessContext) {
        let p = &self.shared.params;
        if self.shared.bypass.load(Ordering::Relaxed) {
            return;
        }
        let g = db_to_gain(p[0].get());
        let width = p[1].get();
        let gl = if p[2].get() > 0.5 { -g } else { g };
        let gr = if p[3].get() > 0.5 { -g } else { g };
        for (l, r) in ctx.left.iter_mut().zip(ctx.right.iter_mut()) {
            let mid = (*l + *r) * 0.5;
            let side = (*l - *r) * 0.5 * width;
            *l = (mid + side) * gl;
            *r = (mid - side) * gr;
        }
    }
}

// ---------------------------------------------------------------------------

const VOICES: usize = 16;

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy)]
struct Voice {
    stage: Stage,
    note: u8,
    velocity: f32,
    phase: f32,
    inc: f32,
    env: f32,
    lp: [f32; 2],
    age: u32,
}

impl Default for Voice {
    fn default() -> Self {
        Voice { stage: Stage::Off, note: 0, velocity: 0.0, phase: 0.0, inc: 0.0, env: 0.0, lp: [0.0; 2], age: 0 }
    }
}

struct BasicSynth {
    rate: f32,
    shared: Arc<DeviceShared>,
    voices: [Voice; VOICES],
    clock: u32,
}

impl BasicSynth {
    fn new(rate: f32, shared: Arc<DeviceShared>) -> Self {
        BasicSynth { rate, shared, voices: [Voice::default(); VOICES], clock: 0 }
    }

    fn note_on(&mut self, note: u8, vel: u8) {
        self.clock += 1;
        // Reuse a voice playing the same note, else a free one, else the oldest.
        let idx = self
            .voices
            .iter()
            .position(|v| v.stage != Stage::Off && v.note == note)
            .or_else(|| self.voices.iter().position(|v| v.stage == Stage::Off))
            .unwrap_or_else(|| (0..VOICES).min_by_key(|&i| self.voices[i].age).unwrap_or(0));
        let v = &mut self.voices[idx];
        let freq = 440.0 * 2f32.powf((note as f32 - 69.0) / 12.0);
        *v = Voice {
            stage: Stage::Attack,
            note,
            velocity: vel as f32 / 127.0,
            phase: 0.0,
            inc: freq / self.rate,
            env: v.env,
            lp: v.lp,
            age: self.clock,
        };
    }

    fn note_off(&mut self, note: u8) {
        for v in self.voices.iter_mut().filter(|v| v.note == note && v.stage != Stage::Off) {
            v.stage = Stage::Release;
        }
    }

    fn handle(&mut self, e: &[u8]) {
        if e.is_empty() {
            return;
        }
        match (e[0] & 0xF0, e.get(1).copied(), e.get(2).copied()) {
            (0x90, Some(n), Some(v)) if v > 0 => self.note_on(n, v),
            (0x80, Some(n), _) | (0x90, Some(n), Some(0)) => self.note_off(n),
            (0xB0, Some(120..=123), _) => {
                for v in &mut self.voices {
                    v.stage = Stage::Release;
                }
            }
            _ => {}
        }
    }
}

fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let t = t / dt;
        t + t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + t + t + 1.0
    } else {
        0.0
    }
}

impl Device for BasicSynth {
    fn process(&mut self, ctx: &mut ProcessContext) {
        let p = &self.shared.params;
        let wave = p[0].get().round() as u32;
        let ms = |x: f32| (x * 0.001 * self.rate).max(1.0);
        let attack = 1.0 / ms(p[1].get());
        let decay = 1.0 / ms(p[2].get());
        let sustain = p[3].get();
        let release = 1.0 / ms(p[4].get());
        let a = 1.0 - (-std::f32::consts::TAU * p[5].get() / self.rate).exp();
        let volume = db_to_gain(p[6].get());
        let bypass = self.shared.bypass.load(Ordering::Relaxed);

        let mut ev = 0;
        let n = ctx.left.len().min(ctx.right.len());
        for i in 0..n {
            while ev < ctx.midi.len() && ctx.midi[ev].frame as usize <= i {
                let e = ctx.midi[ev];
                self.handle(e.bytes());
                ev += 1;
            }
            if bypass {
                continue;
            }
            let mut out = 0.0;
            for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Off) {
                match v.stage {
                    Stage::Attack => {
                        v.env += attack;
                        if v.env >= 1.0 {
                            v.env = 1.0;
                            v.stage = Stage::Decay;
                        }
                    }
                    Stage::Decay => {
                        v.env -= decay * (1.0 - sustain).max(0.001);
                        if v.env <= sustain {
                            v.env = sustain;
                            v.stage = Stage::Sustain;
                        }
                    }
                    Stage::Release => {
                        v.env -= release;
                        if v.env <= 0.0 {
                            v.env = 0.0;
                            v.stage = Stage::Off;
                        }
                    }
                    _ => {}
                }
                let t = v.phase;
                let s = match wave {
                    0 => (t * std::f32::consts::TAU).sin(),
                    1 => 2.0 * t - 1.0 - poly_blep(t, v.inc),
                    2 => {
                        let sq = if t < 0.5 { 1.0 } else { -1.0 };
                        sq + poly_blep(t, v.inc) - poly_blep((t + 0.5).fract(), v.inc)
                    }
                    _ => 1.0 - 4.0 * (t - 0.5).abs(),
                };
                v.phase = (v.phase + v.inc).fract();
                // Two cascaded one-pole low-pass filters.
                v.lp[0] += a * (s - v.lp[0]);
                v.lp[1] += a * (v.lp[0] - v.lp[1]);
                out += v.lp[1] * v.env * v.velocity;
            }
            ctx.left[i] += out * volume;
            ctx.right[i] += out * volume;
        }
        // Handle events that fall past the end of the block.
        while ev < ctx.midi.len() {
            let e = ctx.midi[ev];
            self.handle(e.bytes());
            ev += 1;
        }
    }
}
