//! Offline export of the song to WAV or MP3.
//!
//! The song is rendered faster than real time on a worker thread with fresh
//! copies of every device, using the same clip rendering, device chain and
//! fader code as the live engine, and streamed straight into the file.
//! MP3 encoding uses the system's libmp3lame, loaded at runtime.

use crate::device::{Device, MidiEvent};
use crate::engine::{Arrangement, mix_into, render_clips, run_chain};
use crate::project::TrackKind;
use crate::util::AtomicF32;
use libloading::Library;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

const BLOCK: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Wav16,
    Wav24,
    WavFloat,
    Mp3 { kbps: u32 },
}

impl Format {
    pub const ALL: [Format; 5] = [Format::Wav16, Format::Wav24, Format::WavFloat, Format::Mp3 { kbps: 320 }, Format::Mp3 { kbps: 192 }];

    pub fn extension(&self) -> &'static str {
        match self {
            Format::Mp3 { .. } => "mp3",
            _ => "wav",
        }
    }

    pub fn label(&self) -> String {
        match self {
            Format::Wav16 => "WAV 16-bit".into(),
            Format::Wav24 => "WAV 24-bit".into(),
            Format::WavFloat => "WAV 32-bit float".into(),
            Format::Mp3 { kbps } => format!("MP3 {kbps} kbps"),
        }
    }
}

/// One track prepared for offline rendering.
pub struct OfflineTrack {
    pub id: u64,
    pub kind: TrackKind,
    pub gain: f32,
    pub pan: f32,
    /// False when muted or silenced by another track's solo.
    pub audible: bool,
    pub devices: Vec<Box<dyn Device>>,
}

/// Everything needed to render the song away from the app.
pub struct RenderPlan {
    pub rate: u32,
    /// Frames to render (song end plus tail).
    pub length: u64,
    pub arrangement: Arrangement,
    pub tracks: Vec<OfflineTrack>,
    pub master_gain: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ExportStats {
    pub frames: u64,
    /// Highest absolute sample before clamping (> 1.0 means it clipped).
    pub peak: f32,
}

/// Render the plan block by block into `sink`.
pub fn render(plan: &mut RenderPlan, sink: &mut dyn Sink, progress: &AtomicF32, cancel: &AtomicBool) -> Result<ExportStats, String> {
    let (mut ml, mut mr) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
    let (mut bl, mut br) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
    let mut events: Vec<MidiEvent> = Vec::with_capacity(4096);
    let mut peak = 0f32;
    let mut pos: u64 = 0;
    while pos < plan.length {
        if cancel.load(Ordering::Relaxed) {
            return Err("export cancelled".into());
        }
        let n = BLOCK.min((plan.length - pos) as usize);
        ml[..n].fill(0.0);
        mr[..n].fill(0.0);
        for t in &mut plan.tracks {
            bl[..n].fill(0.0);
            br[..n].fill(0.0);
            events.clear();
            if let Some((_, clips)) = plan.arrangement.tracks.iter().find(|(id, _)| *id == t.id) {
                render_clips(clips, pos as i64, &mut bl[..n], &mut br[..n], &mut events, 0);
            }
            events.sort_unstable_by_key(|e| e.frame);
            run_chain(&mut t.devices, t.kind, &mut bl[..n], &mut br[..n], &events);
            mix_into(&bl[..n], &br[..n], &mut ml[..n], &mut mr[..n], t.gain, t.pan, t.audible);
        }
        for i in 0..n {
            ml[i] *= plan.master_gain;
            mr[i] *= plan.master_gain;
            peak = peak.max(ml[i].abs()).max(mr[i].abs());
        }
        sink.write(&ml[..n], &mr[..n])?;
        pos += n as u64;
        progress.set(pos as f32 / plan.length.max(1) as f32);
    }
    Ok(ExportStats { frames: pos, peak })
}

// ---------------------------------------------------------------------------
// Output files

pub trait Sink: Send {
    fn write(&mut self, l: &[f32], r: &[f32]) -> Result<(), String>;
    fn finish(self: Box<Self>) -> Result<(), String>;
}

pub fn open_sink(path: &Path, format: Format, rate: u32) -> Result<Box<dyn Sink>, String> {
    match format {
        Format::Mp3 { kbps } => Ok(Box::new(Mp3Sink::create(path, rate, kbps)?)),
        _ => Ok(Box::new(WavSink::create(path, format, rate)?)),
    }
}

struct WavSink {
    writer: hound::WavWriter<BufWriter<File>>,
    format: Format,
}

impl WavSink {
    fn create(path: &Path, format: Format, rate: u32) -> Result<Self, String> {
        let (bits, sample_format) = match format {
            Format::Wav16 => (16, hound::SampleFormat::Int),
            Format::Wav24 => (24, hound::SampleFormat::Int),
            _ => (32, hound::SampleFormat::Float),
        };
        let spec = hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: bits, sample_format };
        let writer = hound::WavWriter::create(path, spec).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(WavSink { writer, format })
    }
}

impl Sink for WavSink {
    fn write(&mut self, l: &[f32], r: &[f32]) -> Result<(), String> {
        for (&a, &b) in l.iter().zip(r) {
            for s in [a, b] {
                let res = match self.format {
                    Format::Wav16 => self.writer.write_sample((s.clamp(-1.0, 1.0) * 32767.0).round() as i16),
                    Format::Wav24 => self.writer.write_sample((s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32),
                    _ => self.writer.write_sample(s),
                };
                res.map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        self.writer.finalize().map_err(|e| e.to_string())
    }
}

// ---- MP3 via libmp3lame (loaded at runtime)

struct Lame {
    _lib: Library,
    init: unsafe extern "C" fn() -> *mut c_void,
    set_num_channels: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    set_in_samplerate: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    set_brate: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    set_quality: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    init_params: unsafe extern "C" fn(*mut c_void) -> c_int,
    encode_float: unsafe extern "C" fn(*mut c_void, *const f32, *const f32, c_int, *mut u8, c_int) -> c_int,
    encode_flush: unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int,
    close: unsafe extern "C" fn(*mut c_void) -> c_int,
}

impl Lame {
    fn load() -> Result<Self, String> {
        unsafe {
            #[cfg(windows)]
            let (name, hint) = ("libmp3lame.dll", "place libmp3lame.dll next to studio.exe");
            #[cfg(not(windows))]
            let (name, hint) = ("libmp3lame.so.0", "sudo apt install libmp3lame0");
            let lib = Library::new(name).map_err(|_| format!("MP3 export needs {name} ({hint})"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes()).map_err(|e| format!("libmp3lame {}: {e}", $name))?
                };
            }
            Ok(Lame {
                init: sym!("lame_init"),
                set_num_channels: sym!("lame_set_num_channels"),
                set_in_samplerate: sym!("lame_set_in_samplerate"),
                set_brate: sym!("lame_set_brate"),
                set_quality: sym!("lame_set_quality"),
                init_params: sym!("lame_init_params"),
                encode_float: sym!("lame_encode_buffer_ieee_float"),
                encode_flush: sym!("lame_encode_flush"),
                close: sym!("lame_close"),
                _lib: lib,
            })
        }
    }
}

static LAME: OnceLock<Result<Lame, String>> = OnceLock::new();

fn lame() -> Result<&'static Lame, String> {
    LAME.get_or_init(Lame::load).as_ref().map_err(|e| e.clone())
}

/// `Ok` when MP3 export is possible, otherwise the reason it is not.
pub fn mp3_available() -> Result<(), String> {
    lame().map(|_| ())
}

struct Mp3Sink {
    lame: &'static Lame,
    gfp: *mut c_void,
    file: BufWriter<File>,
    buf: Vec<u8>,
    scratch: [Vec<f32>; 2],
}

// The encoder handle is only used by the thread that owns the sink.
unsafe impl Send for Mp3Sink {}

impl Mp3Sink {
    fn create(path: &Path, rate: u32, kbps: u32) -> Result<Self, String> {
        let lame = lame()?;
        let file = BufWriter::new(File::create(path).map_err(|e| format!("{}: {e}", path.display()))?);
        unsafe {
            let gfp = (lame.init)();
            if gfp.is_null() {
                return Err("lame_init failed".into());
            }
            (lame.set_num_channels)(gfp, 2);
            (lame.set_in_samplerate)(gfp, rate as c_int);
            (lame.set_brate)(gfp, kbps as c_int);
            (lame.set_quality)(gfp, 2); // high quality
            if (lame.init_params)(gfp) < 0 {
                (lame.close)(gfp);
                return Err(format!("LAME rejected {rate} Hz / {kbps} kbps"));
            }
            Ok(Mp3Sink { lame, gfp, file, buf: vec![0; BLOCK * 5 / 4 + 7200], scratch: [vec![0.0; BLOCK], vec![0.0; BLOCK]] })
        }
    }
}

impl Sink for Mp3Sink {
    fn write(&mut self, l: &[f32], r: &[f32]) -> Result<(), String> {
        let n = l.len().min(BLOCK);
        for (dst, src) in self.scratch.iter_mut().zip([l, r]) {
            for (d, s) in dst[..n].iter_mut().zip(src) {
                *d = s.clamp(-1.0, 1.0);
            }
        }
        let written = unsafe {
            (self.lame.encode_float)(self.gfp, self.scratch[0].as_ptr(), self.scratch[1].as_ptr(), n as c_int, self.buf.as_mut_ptr(), self.buf.len() as c_int)
        };
        if written < 0 {
            return Err(format!("MP3 encoding failed ({written})"));
        }
        self.file.write_all(&self.buf[..written as usize]).map_err(|e| e.to_string())
    }

    fn finish(mut self: Box<Self>) -> Result<(), String> {
        let written = unsafe { (self.lame.encode_flush)(self.gfp, self.buf.as_mut_ptr(), self.buf.len() as c_int) };
        if written > 0 {
            self.file.write_all(&self.buf[..written as usize]).map_err(|e| e.to_string())?;
        }
        self.file.flush().map_err(|e| e.to_string())
    }
}

impl Drop for Mp3Sink {
    fn drop(&mut self) {
        unsafe { (self.lame.close)(self.gfp) };
    }
}

// ---------------------------------------------------------------------------
// Background job

pub struct ExportJob {
    pub path: PathBuf,
    pub progress: Arc<AtomicF32>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<ExportStats, String>>>,
}

impl ExportJob {
    /// Open the output file and start rendering on a worker thread.
    pub fn start(mut plan: RenderPlan, path: PathBuf, format: Format) -> Result<Self, String> {
        let mut sink = open_sink(&path, format, plan.rate)?;
        let progress = Arc::new(AtomicF32::new(0.0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p, c) = (progress.clone(), cancel.clone());
        let partial = path.clone();
        let handle = std::thread::Builder::new()
            .name("studio-export".into())
            .spawn(move || {
                let result = render(&mut plan, sink.as_mut(), &p, &c);
                let result = result.and_then(|stats| sink.finish().map(|_| stats));
                if result.is_err() {
                    let _ = std::fs::remove_file(&partial);
                }
                result
            })
            .map_err(|e| e.to_string())?;
        Ok(ExportJob { path, progress, cancel, handle: Some(handle) })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    pub fn join(mut self) -> Result<ExportStats, String> {
        match self.handle.take() {
            Some(h) => h.join().map_err(|_| "export thread panicked".to_string())?,
            None => Err("export already finished".into()),
        }
    }
}
