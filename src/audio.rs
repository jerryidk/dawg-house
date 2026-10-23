//! Audio sources: in-memory sample data with waveform overviews, WAV import
//! and the disk writer used while recording.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Number of frames summarised by one waveform peak entry.
pub const PEAK_BLOCK: usize = 256;

pub struct AudioData {
    pub path: PathBuf,
    /// One Vec per channel (1 or 2), all at the engine sample rate.
    pub channels: Vec<Vec<f32>>,
    /// (min, max) across channels for every `PEAK_BLOCK` frames.
    pub peaks: Vec<(f32, f32)>,
}

impl AudioData {
    pub fn new(path: PathBuf, channels: Vec<Vec<f32>>) -> Self {
        let mut peaks = Vec::new();
        append_peaks(&mut peaks, &channels, 0);
        AudioData { path, channels, peaks }
    }

    pub fn len(&self) -> usize {
        self.channels.first().map_or(0, |c| c.len())
    }

    /// Sample of channel `ch` (mono sources feed both channels).
    #[inline]
    pub fn sample(&self, ch: usize, i: usize) -> f32 {
        let c = &self.channels[ch.min(self.channels.len() - 1)];
        c.get(i).copied().unwrap_or(0.0)
    }

    /// Min/max of the waveform between frames `a` and `b`.
    pub fn range_peak(&self, a: usize, b: usize) -> (f32, f32) {
        range_peak(&self.channels, &self.peaks, a, b)
    }
}

pub fn range_peak(channels: &[Vec<f32>], peaks: &[(f32, f32)], a: usize, b: usize) -> (f32, f32) {
    let (mut lo, mut hi) = (0f32, 0f32);
    if b <= a {
        return (lo, hi);
    }
    if b - a >= PEAK_BLOCK * 2 {
        let (pa, pb) = (a / PEAK_BLOCK, b.div_ceil(PEAK_BLOCK).min(peaks.len()));
        for &(l, h) in peaks.get(pa..pb).unwrap_or(&[]) {
            lo = lo.min(l);
            hi = hi.max(h);
        }
    } else {
        for c in channels {
            for &s in c.get(a..b.min(c.len())).unwrap_or(&[]) {
                lo = lo.min(s);
                hi = hi.max(s);
            }
        }
    }
    (lo, hi)
}

/// Extend `peaks` to cover `channels`, starting at frame `from` (must be a
/// multiple of PEAK_BLOCK; the last partial block is recomputed).
fn append_peaks(peaks: &mut Vec<(f32, f32)>, channels: &[Vec<f32>], from: usize) {
    let len = channels.first().map_or(0, |c| c.len());
    peaks.truncate(from / PEAK_BLOCK);
    let mut start = peaks.len() * PEAK_BLOCK;
    while start < len {
        let end = (start + PEAK_BLOCK).min(len);
        let (mut lo, mut hi) = (0f32, 0f32);
        for c in channels {
            for &s in &c[start..end] {
                lo = lo.min(s);
                hi = hi.max(s);
            }
        }
        peaks.push((lo, hi));
        start = end;
    }
}

/// Load a WAV file, converting to f32 and resampling to `target_rate`.
pub fn load_wav(path: &Path, target_rate: u32) -> Result<AudioData, String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let spec = reader.spec();
    let nch = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.unwrap_or(0) as f32 * scale).collect()
        }
    };
    // Keep at most two channels (stereo tracks).
    let keep = nch.min(2);
    let mut channels = vec![Vec::with_capacity(interleaved.len() / nch); keep];
    for frame in interleaved.chunks_exact(nch) {
        for (c, ch) in channels.iter_mut().enumerate() {
            ch.push(frame[c]);
        }
    }
    if spec.sample_rate != target_rate && target_rate > 0 {
        channels = channels.iter().map(|c| resample_linear(c, spec.sample_rate, target_rate)).collect();
    }
    Ok(AudioData::new(path.to_path_buf(), channels))
}

fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let ratio = from as f64 / to as f64;
    let out_len = (input.len() as f64 / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input.get(idx).copied().unwrap_or(0.0);
            let b = input.get(idx + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Recording

/// Audio captured so far, shared with the UI for the live waveform.
#[derive(Default)]
pub struct LiveTake {
    pub channels: Vec<Vec<f32>>,
    pub peaks: Vec<(f32, f32)>,
}

pub struct Recorder {
    pub track_id: u64,
    pub live: Arc<Mutex<LiveTake>>,
    handle: Option<JoinHandle<Result<AudioData, String>>>,
}

impl Recorder {
    /// Spawn the disk-writer thread. The returned producer is handed to the
    /// audio thread, which pushes interleaved frames into it; dropping the
    /// producer ends the take.
    pub fn start(track_id: u64, path: PathBuf, channels: usize, rate: u32) -> Result<(Self, rtrb::Producer<f32>), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let spec = hound::WavSpec {
            channels: channels as u16,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).map_err(|e| format!("{}: {e}", path.display()))?;
        // ~4 seconds of headroom between the audio thread and the disk thread.
        let (producer, mut consumer) = rtrb::RingBuffer::<f32>::new(rate as usize * channels * 4);
        let live = Arc::new(Mutex::new(LiveTake { channels: vec![Vec::new(); channels], peaks: vec![] }));
        let live2 = live.clone();
        let path2 = path.clone();
        let handle = std::thread::Builder::new()
            .name("studio-recorder".into())
            .spawn(move || {
                let mut scratch: Vec<Vec<f32>> = vec![Vec::new(); channels];
                loop {
                    let finished = consumer.is_abandoned();
                    let avail = consumer.slots() / channels * channels;
                    if avail > 0 {
                        let chunk = consumer.read_chunk(avail).map_err(|e| e.to_string())?;
                        for c in &mut scratch {
                            c.clear();
                        }
                        let (a, b) = chunk.as_slices();
                        for (i, &s) in a.iter().chain(b.iter()).enumerate() {
                            writer.write_sample(s).map_err(|e| e.to_string())?;
                            scratch[i % channels].push(s);
                        }
                        chunk.commit_all();
                        let mut take = live2.lock().unwrap();
                        let from = take.channels[0].len() / PEAK_BLOCK * PEAK_BLOCK;
                        for (dst, src) in take.channels.iter_mut().zip(&scratch) {
                            dst.extend_from_slice(src);
                        }
                        let take = &mut *take;
                        append_peaks(&mut take.peaks, &take.channels, from);
                    } else if finished {
                        break;
                    } else {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                writer.finalize().map_err(|e| e.to_string())?;
                let take = std::mem::take(&mut *live2.lock().unwrap());
                Ok(AudioData { path: path2, channels: take.channels, peaks: take.peaks })
            })
            .map_err(|e| e.to_string())?;
        Ok((Recorder { track_id, live, handle: Some(handle) }, producer))
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    pub fn join(mut self) -> Result<AudioData, String> {
        match self.handle.take() {
            Some(h) => h.join().map_err(|_| "recorder thread panicked".to_string())?,
            None => Err("recorder already joined".into()),
        }
    }
}
