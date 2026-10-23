use std::sync::atomic::{AtomicU32, Ordering};

/// Lock-free f32 cell, used to share parameters and meter levels between the
/// UI thread and the real-time audio thread.
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    #[inline]
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
    /// Raise the stored value to `v` if larger (peak-hold for meters).
    #[inline]
    pub fn max(&self, v: f32) {
        let mut cur = self.0.load(Ordering::Relaxed);
        while v > f32::from_bits(cur) {
            match self.0.compare_exchange_weak(cur, v.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
    }
    /// Read the value and reset it to zero (meter consumption).
    #[inline]
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    if db <= -60.0 { 0.0 } else { 10f32.powf(db / 20.0) }
}

pub fn gain_to_db(g: f32) -> f32 {
    if g <= 0.000_001 { -120.0 } else { 20.0 * g.log10() }
}

/// Format a sample position as `m:ss.mmm`.
pub fn format_time(samples: i64, rate: u32) -> String {
    let secs = samples.max(0) as f64 / rate.max(1) as f64;
    let m = (secs / 60.0).floor() as i64;
    let s = secs - m as f64 * 60.0;
    format!("{m}:{s:06.3}")
}
