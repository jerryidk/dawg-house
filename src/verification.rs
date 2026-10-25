//! Kani proof harnesses for the code at Studio's unsafe / external boundaries.
//! Run with `cargo kani` (see VERIFICATION.md).

/// `loop_block` never plays outside the loop once inside it, never splits
/// past the block, and never overflows for realistic positions.
#[kani::proof]
fn loop_block_stays_in_loop() {
    let pos: i64 = kani::any();
    let n: usize = kani::any();
    let total: usize = kani::any();
    let (ls, le): (i64, i64) = (kani::any(), kani::any());
    kani::assume(n <= 8192 && total >= n && total <= 8192);
    // Positions and loop points within ±2^40 samples (~700 years at 48 kHz).
    let r = 1i64 << 40;
    kani::assume(pos > -r && pos < r && ls > -r && ls < r && le > -r && le < r);
    let looping: bool = kani::any();
    let (p, split, next) = crate::engine::loop_block(pos, n, total, looping, ls, le);
    assert!(split <= n);
    if looping && le > ls && pos >= ls {
        assert!(p >= ls && p < le, "plays inside the loop");
        assert!(next >= ls && next < le, "next block starts inside the loop");
    }
}

// ---------------------------------------------------------------------------
// LV2 atom buffers (shared with plugin code)

/// The byte views of plugin port buffers cover exactly the words, no more.
#[kani::proof]
fn atom_byte_views_are_exact() {
    let mut words: [u64; 4] = kani::any();
    let expect = words[1].to_ne_bytes();
    let b = crate::atom::as_bytes(&words);
    assert_eq!(b.len(), 32);
    let i: usize = kani::any();
    kani::assume(i < 32);
    let _ = b[i];
    assert_eq!(b[8..16], expect);
    let m = crate::atom::as_bytes_mut(&mut words);
    m[31] = 7;
    assert_eq!(words[3].to_ne_bytes()[7], 7);
}

/// Parsing whatever a plugin wrote into an output port (any sizes, any
/// garbage) stays in bounds and only yields whole atom headers.
#[kani::proof]
#[kani::unwind(5)]
fn atom_parse_any_plugin_output() {
    let bytes: [u8; 48] = kani::any();
    let seq: u32 = kani::any();
    let mut events = 0;
    crate::atom::for_each_event(&bytes, seq, |ev| {
        assert!(ev.len() >= 8, "an event always has its atom header");
        events += 1;
    });
    assert!(events <= 2);
}

/// Appending to an input sequence never writes out of bounds, whatever the
/// header says (a plugin may have scribbled over it), and grows the
/// sequence by exactly one padded event when it fits.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(70)]
fn atom_append_any_header() {
    let mut bytes: [u8; 64] = kani::any();
    let before = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let body_all: [u8; 12] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= 12);
    let ok = crate::atom::seq_append(&mut bytes, kani::any(), kani::any(), &body_all[..len]);
    let after = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if ok {
        assert_eq!(after, before + 16 + len.div_ceil(8) * 8);
    } else {
        assert_eq!(after, before);
    }
}

/// What Studio writes into an input sequence is what a reader gets back.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(66)]
fn atom_roundtrip() {
    // Header (16) + event 1 (16 + 8) + event 2 (16 + 8) = 64 bytes.
    let mut bytes = [0u8; 64];
    let seq: u32 = kani::any();
    crate::atom::seq_reset(&mut bytes, seq);
    let (body1, body2): ([u8; 2], [u8; 5]) = (kani::any(), kani::any());
    let (t1, t2): (u32, u32) = (kani::any(), kani::any());
    assert!(crate::atom::seq_append(&mut bytes, 0, t1, &body1));
    assert!(crate::atom::seq_append(&mut bytes, 5, t2, &body2));
    let mut n = 0;
    crate::atom::for_each_event(&bytes, seq, |ev| {
        let (size, ty) = (u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]), u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]));
        if n == 0 {
            assert!(size == 2 && ty == t1 && ev[8..] == body1);
        } else {
            assert!(size == 5 && ty == t2 && ev[8..] == body2);
        }
        n += 1;
    });
    assert_eq!(n, 2);
}

// ---------------------------------------------------------------------------
// Lock-free message queues (GUI <-> audio thread, LV2 worker)

/// A message bigger than the free space is rejected whole.
#[kani::proof]
#[kani::unwind(9)]
fn frames_reject_oversized() {
    let (mut tx, rx) = rtrb::RingBuffer::<u8>::new(8);
    let len: usize = kani::any();
    kani::assume(len > 4 && len <= 8);
    assert!(!crate::frames::push(&mut tx, &[0; 4], &[0u8; 8][..len]));
    assert_eq!(rx.slots(), 0);
}

/// Arbitrary bytes in a queue (e.g. a corrupt length) never make a reader
/// panic or read past the queued data.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(10)]
fn frames_pop_garbage() {
    let (mut tx, mut rx) = rtrb::RingBuffer::<u8>::new(8);
    let n: usize = kani::any();
    kani::assume(n <= 8);
    for _ in 0..n {
        let _ = tx.push(kani::any());
    }
    let mut out = [0u8; 2];
    if let Some(copied) = crate::frames::pop_with::<4, _>(&mut rx, |_, a, b| crate::frames::copy_body(a, b, &mut out)) {
        assert!(copied <= 2);
    }
}

// ---------------------------------------------------------------------------
// Callbacks into Studio from plugin / GUI code

/// A GUI's control write: `buffer` may point anywhere into its memory.
#[kani::proof]
fn gui_control_value_any_pointer() {
    let buf: [u8; 12] = kani::any();
    let off: usize = kani::any();
    kani::assume(off <= 8);
    let size: u32 = kani::any();
    kani::assume(size as usize <= 12 - off);
    let v = unsafe { crate::lv2ui::decode_control(size, buf.as_ptr().add(off) as *const std::ffi::c_void) };
    assert_eq!(v.is_some(), size >= 4);
}

/// `retrieve` (plugin restoring its state) hands back exactly the stored
/// value and tolerates null out-pointers.
#[kani::proof]
#[kani::unwind(4)]
fn state_retrieve_any_key() {
    let (v1, v2): ([u8; 3], [u8; 5]) = (kani::any(), kani::any());
    let table: Vec<(u32, u32, u32, &[u8])> = vec![(1, 10, 0, &v1[..]), (2, 20, 3, &v2[..])];
    let key: u32 = kani::any();
    let (mut size, mut ty, mut flags) = (0usize, 0u32, 0u32);
    let want_out: bool = kani::any();
    let (sp, tp, fp) = if want_out {
        (&mut size as *mut usize, &mut ty as *mut u32, &mut flags as *mut u32)
    } else {
        (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    let p = unsafe { crate::lv2::retrieve_cb(&table as *const _ as *mut std::ffi::c_void, key, sp, tp, fp) };
    match key {
        1 => assert!(p == v1.as_ptr() as *const std::ffi::c_void && (!want_out || (size == 3 && ty == 10))),
        2 => assert!(p == v2.as_ptr() as *const std::ffi::c_void && (!want_out || (size == 5 && ty == 20 && flags == 3))),
        _ => assert!(p.is_null()),
    }
}

// ---------------------------------------------------------------------------
// Device I/O (values from drivers and atomics)

/// Any channel selection (any i32 in the atomics) routes to valid input
/// buffers of the cycle's length, or to nothing.
#[kani::proof]
#[kani::unwind(5)]
fn device_input_routing_any_channel() {
    use crate::backend::devio::{DeviceInput, DeviceIo, InputState};
    use crate::backend::{CycleIo, TrackPorts};
    use std::sync::atomic::Ordering;
    let shared = crate::engine::TrackShared::default();
    shared.in_channels[0].store(kani::any(), Ordering::Relaxed);
    shared.in_channels[1].store(kani::any(), Ordering::Relaxed);
    // Two open input devices (0: stereo, 3: mono); any id may name others.
    let input = |device, channels| InputState::new(DeviceInput { device, channels, rx: rtrb::RingBuffer::new(1).1 }, 4);
    let inputs = [input(0, 2), input(3, 1)];
    let frames: usize = kani::any();
    kani::assume(frames <= 4);
    let (mut l, mut r) = ([0f32; 4], [0f32; 4]);
    let io = DeviceIo { frames, inputs: &inputs, midi: &[], out_l: &mut l, out_r: &mut r };
    if let Some((a, b)) = io.track_audio(&TrackPorts::Device, &shared) {
        assert!(a.len() == frames && b.len() == frames);
    }
}

/// Deinterleaving captured input with the channel count the driver
/// reported, as `DeviceRunner::set_input` + `pull_input` do (0..=2
/// channels, built concretely to keep the proof small).
#[kani::proof]
#[kani::unwind(6)]
fn device_deinterleave_any_channel_count() {
    let channels = kani::any::<u8>() % 3;
    let mut bufs = match channels {
        0 => vec![],
        1 => vec![vec![0f32; 2]],
        _ => vec![vec![0f32; 2], vec![0f32; 2]],
    }; // set_input: one buffer per reported channel
    let ch = (channels as usize).max(1); // pull_input
    let frames: usize = kani::any();
    kani::assume(frames <= 2);
    let samples = [0.5f32; 4];
    let total = if channels == 0 { 0 } else { frames * ch }; // no data arrives without channels
    let split: usize = kani::any();
    kani::assume(split <= total);
    let (a, b) = samples[..total].split_at(split);
    crate::backend::devio::deinterleave(a, b, ch, &mut bufs);
}

/// Reading a sample from audio data of any shape (0, 1 or 2 channels).
/// The shapes are built concretely: a symbolic-length `Vec` makes CBMC's
/// memory use explode.
#[kani::proof]
#[kani::unwind(3)]
fn audio_sample_any_shape() {
    let channels = match kani::any::<u8>() % 3 {
        0 => vec![],
        1 => vec![vec![0.25f32; 2]],
        _ => vec![vec![0.25f32; 2], vec![0.5f32; 2]],
    };
    let data = crate::audio::AudioData { path: Default::default(), channels, peaks: vec![] };
    let ch: usize = kani::any();
    let i: usize = kani::any();
    kani::assume(ch < 4 && i < 4);
    let _ = data.sample(ch, i);
}

/// MIDI from drivers/ports: any length, no panic; only short messages pass.
#[kani::proof]
fn midi_messages_any_length() {
    let bytes: [u8; 6] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= 6);
    let e = crate::backend::devio::MidiInEvent::new(kani::any(), &bytes[..len]);
    assert_eq!(e.is_some(), (1..=3).contains(&len));
    let m = crate::device::MidiEvent::new(kani::any(), &bytes[..len]);
    assert!(m.bytes().len() == len.min(3));
}

/// Waveform peak scan at any indices.
#[kani::proof]
#[kani::unwind(6)]
fn peak_scan_any_range() {
    let channels = vec![vec![0.5f32; 4]];
    let peaks = vec![(-0.5f32, 0.5f32); 2];
    let (lo, hi) = crate::audio::range_peak(&channels, &peaks, kani::any(), kani::any());
    assert!(lo <= 0.0 && hi >= 0.0);
}

/// Clip rendering index math for any clip placement, offset, length and
/// fades against short audio (the source may be shorter than the clip).
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(4)]
fn render_clip_any_placement() {
    use crate::engine::{PlayClip, PlayContent};
    let data = std::sync::Arc::new(crate::audio::AudioData { path: Default::default(), channels: vec![vec![0.5f32; 2]], peaks: vec![] });
    let r = 1i64 << 12;
    let start: i64 = kani::any();
    let pos: i64 = kani::any();
    let (offset, length, fade_in, fade_out): (usize, usize, usize, usize) = (kani::any(), kani::any(), kani::any(), kani::any());
    kani::assume(start > -r && start < r && pos > -r && pos < r && offset < (1 << 12) && length < (1 << 12) && fade_in < (1 << 12) && fade_out < (1 << 12));
    let clip = PlayClip { start, length, gain: 1.0, fade_in, fade_out, content: PlayContent::Audio { data, offset } };
    let (mut l, mut r2) = ([0f32; 2], [0f32; 2]);
    let n: usize = kani::any();
    kani::assume(n <= 2);
    let mut events = Vec::new();
    crate::engine::render_clips(std::slice::from_ref(&clip), pos, &mut l[..n], &mut r2[..n], &mut events, 0);
}

/// A VST3 plugin's state stream: any seek mode, a seek near the data and
/// any read or write size stay in bounds. (Seeks far past the end, which
/// must not make Studio allocate, are covered by a unit test: the growth
/// loop cannot be unrolled for them.)
#[kani::proof]
#[kani::unwind(17)]
fn vst3_stream_any_seek_read_write() {
    use crate::vst3::com::MemStream;
    use vst3::Steinberg::IBStreamTrait;
    let s = MemStream::new(vec![1, 2, 3]);
    let (pos, mode): (i64, i32) = (kani::any(), kani::any());
    kani::assume((-8..=8).contains(&pos));
    let mut result = 0i64;
    unsafe { s.seek(pos, mode, &mut result) };
    let mut buf = [0u8; 4];
    let n: i32 = kani::any();
    kani::assume(n <= 4);
    let mut done = -1;
    if kani::any() {
        unsafe { s.read(buf.as_mut_ptr().cast(), n, &mut done) };
        assert!(done <= n.max(0));
    } else {
        unsafe { s.write(buf.as_mut_ptr().cast(), n, &mut done) };
        // Seek ≤ end (3) + 8, then at most 4 bytes.
        assert!(s.bytes().len() <= 3 + 8 + 4, "grows only to the write's end");
    }
}

/// Per-block parameter changes as plugins use them: any ids, indices and
/// point counts stay inside the preallocated pool.
#[kani::proof]
#[kani::unwind(17)] // COM interface lookup compares 16-byte GUIDs
fn vst3_param_changes_any_use() {
    use crate::vst3::com::ParamChanges;
    use vst3::Steinberg::Vst::{IParamValueQueueTrait, IParameterChangesTrait};
    let pc = ParamChanges::new(1);
    for _ in 0..2 {
        let id: u32 = kani::any();
        let mut index = -1;
        let q = unsafe { pc.addParameterData(&id, &mut index) };
        assert!(q.is_null() || index == 0);
        if let Some(q) = unsafe { vst3::ComRef::from_raw(q) } {
            let mut at = -1;
            unsafe { q.addPoint(kani::any(), kani::any(), &mut at) };
        }
    }
    let i: i32 = kani::any();
    let q = unsafe { pc.getParameterData(i) };
    assert!(q.is_null() || (0..unsafe { pc.getParameterCount() }).contains(&i));
    if let Some(q) = unsafe { vst3::ComRef::from_raw(q) } {
        let (mut offset, mut value) = (0, 0.0);
        unsafe { q.getPoint(kani::any(), &mut offset, &mut value) };
    }
}
