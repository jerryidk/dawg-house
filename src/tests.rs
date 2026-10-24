use crate::device::{DeviceCategory, DeviceRegistry, DeviceShared, MidiEvent, ProcessContext};
use crate::project::{Clip, ClipContent, History, Note, Project, TrackKind};

fn registry() -> DeviceRegistry {
    crate::lv2::setup_lv2_path();
    DeviceRegistry::new(48000.0)
}

fn run(reg: &DeviceRegistry, id: &str, midi: &[MidiEvent], input: f32) -> (Vec<f32>, Vec<f32>) {
    let info = reg.get(id).unwrap();
    let shared = DeviceShared::new(info);
    let mut dev = reg.create(id, 48000.0, shared, None).unwrap().device;
    let mut l = vec![input; 512];
    let mut r = vec![input; 512];
    dev.process(&mut ProcessContext { left: &mut l, right: &mut r, midi });
    (l, r)
}

#[test]
fn hosts_every_supported_lv2_effect() {
    let reg = registry();
    if cfg!(not(target_os = "linux")) && reg.lv2_error().is_some() {
        println!("no lilv on this platform, skipping: {:?}", reg.lv2_error());
        return;
    }
    assert!(reg.lv2_error().is_none(), "{:?}", reg.lv2_error());
    let lv2: Vec<_> = reg.devices.iter().filter(|d| d.id.starts_with("lv2:") && d.unsupported.is_none()).collect();
    println!("{} LV2 devices supported of {}", lv2.len(), reg.devices.iter().filter(|d| d.id.starts_with("lv2:")).count());
    for d in reg.devices.iter().filter(|d| d.unsupported.is_some()) {
        println!("  unsupported: {} ({})", d.name, d.unsupported.as_ref().unwrap());
    }
    for d in lv2 {
        let (l, r) = run(&reg, &d.id, &[], 0.25);
        assert!(l.iter().chain(&r).all(|s| s.is_finite()), "{} produced non-finite output", d.name);
        println!("  ok: {} [{:?}] {} params", d.name, d.category, d.params.len());
    }
}

#[test]
fn builtin_synth_plays_midi() {
    let reg = registry();
    assert_eq!(reg.get("builtin:synth").unwrap().category, DeviceCategory::Instrument);
    let (silent, _) = run(&reg, "builtin:synth", &[], 0.0);
    assert!(silent.iter().all(|s| *s == 0.0));
    let (l, r) = run(&reg, "builtin:synth", &[MidiEvent::new(0, &[0x90, 60, 100])], 0.0);
    let peak = l.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.01, "synth peak {peak}");
    assert_eq!(l, r);
}

#[test]
fn midi_clip_events_respect_trim() {
    let spb = 24000.0; // 120 BPM at 48 kHz
    let notes = vec![
        Note { pitch: 60, velocity: 100, start: 0.0, length: 1.0 },
        Note { pitch: 62, velocity: 90, start: 1.0, length: 1.0 },
        Note { pitch: 64, velocity: 80, start: 3.5, length: 2.0 },
    ];
    // Left edge trimmed by half a beat (12000), clip 4 beats long (96000).
    let clip = Clip {
        id: 1,
        content: ClipContent::Midi { notes: notes.clone() },
        name: String::new(),
        start: 0,
        offset: 12000,
        length: 96000,
        gain: 1.0,
        fade_in: 0,
        fade_out: 0,
        muted: false,
    };
    let ev = crate::song::midi_events(&notes, clip.offset, clip.length, spb);
    assert_eq!(
        ev,
        vec![
            (0, [0x90, 60, 100]),     // started before the trim point -> starts at 0
            (12000, [0x80, 60, 0]),   // note-off sorted before the next note-on
            (12000, [0x90, 62, 90]),
            (36000, [0x80, 62, 0]),
            (72000, [0x90, 64, 80]),  // 3.5 beats - 0.5 beat trim
            (95999, [0x80, 64, 0]),   // cut at the clip end
        ]
    );
}

#[test]
fn split_and_undo() {
    let mut p = Project::default();
    let t = p.new_track(TrackKind::Audio).id;
    let id = p.alloc_id();
    p.track_mut(t).unwrap().clips.push(Clip {
        id,
        content: ClipContent::Audio { source: 99 },
        name: "a".into(),
        start: 1000,
        offset: 50,
        length: 10000,
        gain: 1.0,
        fade_in: 100,
        fade_out: 100,
        muted: false,
    });
    let mut h = History::default();
    h.checkpoint(&p);
    let right = p.split_clip(id, 4000).unwrap();
    let clips = &p.track(t).unwrap().clips;
    assert_eq!(clips.len(), 2);
    assert_eq!((clips[0].start, clips[0].length, clips[0].offset), (1000, 3000, 50));
    let r = clips.iter().find(|c| c.id == right).unwrap();
    assert_eq!((r.start, r.length, r.offset), (4000, 7000, 3050));
    assert!(p.split_clip(id, 500).is_none());
    assert!(h.undo(&mut p));
    assert_eq!(p.track(t).unwrap().clips.len(), 1);
    assert!(h.redo(&mut p));
    assert_eq!(p.track(t).unwrap().clips.len(), 2);
}

#[test]
fn project_roundtrips_through_json() {
    let mut p = Project::default();
    p.new_track(TrackKind::Instrument);
    let s = serde_json::to_string(&p).unwrap();
    let q: Project = serde_json::from_str(&s).unwrap();
    assert_eq!(q.tracks[0].kind, TrackKind::Instrument);
}

/// Needs a running JACK server, e.g.
/// `jackd -n studiotest -d dummy & JACK_DEFAULT_SERVER=studiotest cargo test -- --ignored`
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn engine_end_to_end_with_jack() {
    use crate::engine::{Arrangement, Command, Engine, PlayClip, PlayContent, TrackShared};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let mut engine = Engine::new("studio-test");
    assert!(engine.is_running(), "{:?}", engine.error);
    let reg = registry();

    // Instrument track with the built-in synth, fed by a MIDI clip.
    let inst = Arc::new(TrackShared::default());
    inst.armed.store(true, Ordering::Relaxed);
    engine.add_track(1, TrackKind::Instrument, inst.clone()).unwrap();
    let info = reg.get("builtin:synth").unwrap();
    let device = reg.create(&info.id, engine.sample_rate as f64, DeviceShared::new(info), None).unwrap().device;
    engine.send(Command::InsertDevice { track: 1, index: 0, device });
    let events = vec![(0u64, [0x90, 57, 110]), (40000, [0x80, 57, 0])];
    engine.set_arrangement(Arrangement {
        tracks: vec![(1, vec![PlayClip { start: 0, length: 48000, gain: 1.0, fade_in: 0, fade_out: 0, content: PlayContent::Midi { events: Arc::new(events) } }])],
    });

    // Audio track to record from.
    let audio = Arc::new(TrackShared::default());
    engine.add_track(2, TrackKind::Audio, audio.clone()).unwrap();

    // A second client acting as a MIDI keyboard.
    let (kb, _) = jack::Client::new("studio-test-kb", jack::ClientOptions::NO_START_SERVER).unwrap();
    let mut out = kb.register_port("out", jack::MidiOut::default()).unwrap();
    let mut cycle = 0u32;
    let kb = kb
        .activate_async(
            (),
            jack::contrib::ClosureProcessHandler::new(move |_: &jack::Client, ps: &jack::ProcessScope| {
                let mut w = out.writer(ps);
                cycle += 1;
                if cycle % 20 == 1 {
                    let _ = w.write(&jack::RawMidi { time: 0, bytes: &[0x90, 64, 90] });
                }
                if cycle % 20 == 10 {
                    let _ = w.write(&jack::RawMidi { time: 0, bytes: &[0x80, 64, 0] });
                }
                jack::Control::Continue
            }),
        )
        .unwrap();
    kb.as_client().connect_ports_by_name("studio-test-kb:out", "studio-test:track1_midi_in").unwrap();

    let dir = std::env::temp_dir().join(format!("studio-test-{}", std::process::id()));
    let (rec, producer) = crate::audio::Recorder::start(2, dir.join("take.wav"), 1, engine.sample_rate).unwrap();
    engine.send(Command::Record { takes: vec![(2, producer, 1)], from: 0, start_at: 0 });
    std::thread::sleep(Duration::from_millis(600));

    let master = engine.shared.master_peak[0].take();
    let inst_out = inst.out_peak[0].take();
    assert!(engine.shared.recording.load(Ordering::Relaxed));
    assert!(engine.shared.position.load(Ordering::Relaxed) > 0, "transport did not move");
    engine.send(Command::Stop);
    std::thread::sleep(Duration::from_millis(100));
    assert!(!engine.shared.playing.load(Ordering::Relaxed));

    let midi = engine.take_midi_recording();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !rec.is_finished() && Instant::now() < deadline {
        engine.collect_garbage();
        std::thread::sleep(Duration::from_millis(10));
    }
    let data = rec.join().unwrap();
    println!("master peak {master:.3}, instrument peak {inst_out:.3}, recorded {} frames, {} MIDI events", data.len(), midi.len());
    assert!(master > 0.01 && inst_out > 0.01, "no audio from the synth");
    assert!(data.len() > 48000 / 4, "recording too short: {}", data.len());
    assert!(crate::audio::load_wav(&dir.join("take.wav"), engine.sample_rate).unwrap().len() == data.len());
    assert!(midi.iter().any(|e| e.track == 1 && e.data[0] == 0x90), "no MIDI recorded");
    let _ = std::fs::remove_dir_all(dir);
    drop(kb);
}

// ---------------------------------------------------------------------------
// Snippets, takes and linked placements

fn snippet_project() -> (Project, u64, u64, crate::song::Sources) {
    use std::sync::Arc;
    let mut p = Project::default();
    let track = p.new_track(TrackKind::Audio).id;
    let snippet = p.new_snippet(track).unwrap();
    // 1 bar of 4/4 at 120 BPM and 48 kHz = 96000 samples.
    p.snippet_mut(snippet).unwrap().bars = 1;
    let mut sources = crate::song::Sources::new();
    sources.insert(500, Arc::new(crate::audio::AudioData::new("t.wav".into(), vec![vec![0.5; 96000 * 3]])));
    for (i, off) in [0u64, 96000, 192000].into_iter().enumerate() {
        let id = p.alloc_id();
        let s = p.snippet_mut(snippet).unwrap();
        s.takes.push(crate::project::Take::new(id, format!("Take {}", i + 1), ClipContent::Audio { source: 500 }, off));
        s.active = Some(id);
    }
    (p, track, snippet, sources)
}

#[test]
fn linked_placement_loops_active_take() {
    use crate::engine::PlayContent;
    let (mut p, track, snippet, sources) = snippet_project();
    let clip = p.place_snippet(snippet, 1000, 48000).unwrap();
    assert_eq!(p.track(track).unwrap().clips[0].length, 96000);
    // Stretch the placement to 2.5 loops, starting half a loop in.
    {
        let c = p.clip_mut(clip).unwrap();
        c.offset = 48000;
        c.length = 240000;
    }
    let arr = crate::song::compile_song(&p, &sources, 48000);
    let clips = &arr.tracks[0].1;
    let spans: Vec<_> = clips
        .iter()
        .map(|c| match &c.content {
            PlayContent::Audio { offset, .. } => (c.start, c.length, *offset),
            _ => unreachable!(),
        })
        .collect();
    // Active take is take 3 (source offset 192000).
    assert_eq!(spans, vec![(1000, 48000, 240000), (49000, 96000, 192000), (145000, 96000, 192000)]);

    // Switching the active take updates every placement (linked).
    let take1 = p.snippet(snippet).unwrap().takes[0].id;
    p.snippet_mut(snippet).unwrap().active = Some(take1);
    let arr = crate::song::compile_song(&p, &sources, 48000);
    assert!(arr.tracks[0].1.iter().all(|c| matches!(&c.content, PlayContent::Audio { offset, .. } if *offset < 96000)));
}

#[test]
fn make_unique_forks_a_placement() {
    let (mut p, track, snippet, _) = snippet_project();
    let a = p.place_snippet(snippet, 0, 48000).unwrap();
    let b = p.place_snippet(snippet, 96000, 48000).unwrap();
    let copy = p.make_unique(b).unwrap();
    assert_ne!(copy, snippet);
    // Changing the copy leaves the original placement alone.
    let first_take = p.snippet(copy).unwrap().takes[0].id;
    p.snippet_mut(copy).unwrap().active = Some(first_take);
    let content = |p: &Project, c| p.track(track).unwrap().clips.iter().find(|x| x.id == c).unwrap().content.clone();
    assert!(matches!(content(&p, a), ClipContent::Snippet { snippet: s } if s == snippet));
    assert!(matches!(content(&p, b), ClipContent::Snippet { snippet: s } if s == copy));
    assert_ne!(p.snippet(snippet).unwrap().active, p.snippet(copy).unwrap().active);
    // Take ids stay unique project-wide.
    assert!(p.find_take(first_take).is_some_and(|(_, si, _)| p.track(track).unwrap().snippets[si].id == copy));
    // Deleting a snippet removes its placements.
    p.delete_snippet(snippet);
    assert_eq!(p.track(track).unwrap().clips.len(), 1);
}

#[test]
fn loop_recordings_split_into_takes() {
    assert_eq!(crate::song::audio_passes(250, 100), vec![0, 100]); // trailing half pass dropped
    assert_eq!(crate::song::audio_passes(40, 100), vec![0]); // nothing complete: keep it
    assert!(crate::song::audio_passes(0, 100).is_empty());
    // Notes in passes 0 and 2 (pass 1 silent), one held over a loop boundary.
    let notes = [(10, 30, 60, 100), (250, 330, 64, 90)];
    let passes = crate::song::midi_passes(&notes, 100, 10.0);
    assert_eq!(passes.len(), 2);
    assert_eq!(passes[0][0], Note { pitch: 60, velocity: 100, start: 1.0, length: 2.0 });
    assert_eq!(passes[1][0], Note { pitch: 64, velocity: 90, start: 5.0, length: 5.0 }); // cut at pass end
}

#[test]
fn snippet_loop_uses_cued_backing_and_mutes_focus_when_recording() {
    let (mut p, track, snippet, sources) = snippet_project();
    let drums = p.new_track(TrackKind::Audio).id;
    let beat = p.new_snippet(drums).unwrap();
    p.snippet_mut(beat).unwrap().bars = 1;
    let take = p.alloc_id();
    let s = p.snippet_mut(beat).unwrap();
    s.takes.push(crate::project::Take::new(take, "t".into(), ClipContent::Audio { source: 500 }, 0));
    s.active = Some(take);
    // Loop the 2-bar snippet: the 1-bar drum snippet repeats twice underneath.
    p.snippet_mut(snippet).unwrap().bars = 2;
    let (arr, len) = crate::song::compile_snippet_loop(&p, &sources, 48000, snippet, false);
    assert_eq!(len, 192000);
    let count = |id| arr.tracks.iter().find(|(t, _)| *t == id).unwrap().1.len();
    assert_eq!((count(track), count(drums)), (1, 2)); // focus: one loop; drums: 2 bars of a 1-bar loop
    let (arr, _) = crate::song::compile_snippet_loop(&p, &sources, 48000, snippet, true);
    assert_eq!(arr.tracks.iter().find(|(t, _)| *t == track).unwrap().1.len(), 0);
    assert_eq!(arr.tracks.iter().find(|(t, _)| *t == drums).unwrap().1.len(), 2);
}

/// Loop recording through a real JACK server (see engine_end_to_end_with_jack).
#[test]
#[ignore]
fn engine_loop_records_with_count_in() {
    use crate::engine::{Command, Engine, TrackShared};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let mut engine = Engine::new("studio-loop-test");
    assert!(engine.is_running(), "{:?}", engine.error);
    let rate = engine.sample_rate as i64;
    let audio = Arc::new(TrackShared::default());
    engine.add_track(7, TrackKind::Audio, audio).unwrap();
    // Loop length a multiple of the block size (like whole bars at 120 BPM),
    // the case that used to stop after one pass, with a one-loop count-in.
    let block = engine.buffer_size().max(1) as i64;
    let period = (rate / 4 / block) * block;
    let sh = engine.shared.clone();
    sh.loop_enabled.store(true, Ordering::Relaxed);
    sh.loop_start.store(0, Ordering::Relaxed);
    sh.loop_end.store(period, Ordering::Relaxed);
    sh.metro_samples_per_beat.set((period / 4) as f32);
    sh.metro_beats_per_bar.store(4, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("studio-loop-{}", std::process::id()));
    let (rec, producer) = crate::audio::Recorder::start(7, dir.join("loop.wav"), 1, engine.sample_rate).unwrap();
    engine.send(Command::Record { takes: vec![(7, producer, 1)], from: 0, start_at: -period });
    let mut max_pos = i64::MIN;
    let mut saw_count_in = false;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1100) {
        let p = sh.position.load(Ordering::Relaxed);
        saw_count_in |= p < 0;
        max_pos = max_pos.max(p);
        std::thread::sleep(Duration::from_millis(2));
    }
    engine.send(Command::Stop);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !rec.is_finished() && Instant::now() < deadline {
        engine.collect_garbage();
        std::thread::sleep(Duration::from_millis(10));
    }
    let data = rec.join().unwrap();
    let frames = sh.recorded_frames.load(Ordering::Relaxed);
    let passes = crate::song::audio_passes(data.len() as u64, period as u64);
    println!("count-in seen {saw_count_in}, max position {max_pos} (loop {period}), recorded {} frames = {} passes", data.len(), passes.len());
    assert!(saw_count_in, "count-in never happened");
    assert!(max_pos < period + 8192, "transport did not loop: {max_pos}");
    assert_eq!(data.len() as i64, frames);
    // ~0.85 s recorded after a 0.25 s count-in = 3 full passes of 0.25 s.
    assert!((2..=4).contains(&passes.len()), "{} passes", passes.len());
    let _ = std::fs::remove_dir_all(dir);
}

/// Opens TONE3000's own GUI (needs the plugin in ~/.lv2 and an X display).
/// `STUDIO_GUI_SECS` controls how long the window stays up.
#[test]
#[ignore]
fn tone3000_gui_and_state() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    crate::lv2ui::init_x11();
    let reg = registry();
    let id = "lv2:https://www.tone3000.com/plugin";
    let info = reg.get(id).expect("TONE3000 not installed").clone();
    assert!(info.has_gui, "TONE3000 GUI not discovered");
    let shared = DeviceShared::new(&info);
    let created = reg.create(id, 48000.0, shared.clone(), None).unwrap();
    let link = created.lv2.clone().unwrap();

    // Simulated audio thread running the plugin.
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let mut device = created.device;
    let audio = std::thread::spawn(move || {
        let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
        let mut blocks = 0u64;
        while !stop2.load(Ordering::Relaxed) {
            for (i, (a, b)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
                *a = ((blocks * 256 + i as u64) as f32 * 0.05).sin() * 0.2;
                *b = *a;
            }
            device.process(&mut ProcessContext { left: &mut l, right: &mut r, midi: &[] });
            blocks += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        (device, blocks)
    });

    let mut win = crate::lv2ui::PluginWindow::open(link.clone(), shared.clone(), "TONE3000 test".into(), 48000.0).expect("GUI failed to open");
    let secs: u64 = std::env::var("STUDIO_GUI_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let t0 = Instant::now();
    let mut frames = 0;
    while t0.elapsed() < Duration::from_secs(secs) {
        assert!(win.pump(), "GUI closed itself");
        frames += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    let state = crate::lv2::save_state(&link).expect("no state interface");
    println!("GUI ran {frames} frames; state has {} entries:", state.len());
    for e in &state {
        println!("  {} ({}): {} bytes", e.key, e.type_uri, e.value.len());
    }
    drop(win);
    stop.store(true, Ordering::Relaxed);
    let (_device, blocks) = audio.join().unwrap();
    println!("audio thread processed {blocks} blocks");
    assert!(blocks > 10);

    // Restore the saved state into a fresh instance.
    let again = reg.create(id, 48000.0, DeviceShared::new(&info), Some(&state)).unwrap();
    let state2 = crate::lv2::save_state(again.lv2.as_ref().unwrap()).unwrap();
    assert_eq!(state.len(), state2.len());
    println!("state restored into a new instance ({} entries)", state2.len());
}

/// Buffer size changes and master output routing through a real JACK server.
#[test]
#[ignore]
fn engine_buffer_size_and_master_output() {
    use crate::engine::Engine;
    use std::time::{Duration, Instant};
    let mut engine = Engine::new("studio-io-test");
    assert!(engine.is_running(), "{:?}", engine.error);
    let before = engine.buffer_size();
    let target = if before == 512 { 1024 } else { 512 };
    engine.set_buffer_size(target).unwrap();
    let t0 = Instant::now();
    while engine.buffer_size() != target && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("buffer size {before} -> {}", engine.buffer_size());
    assert_eq!(engine.buffer_size(), target);
    engine.set_buffer_size(before).unwrap();

    let devices = engine.playback_devices();
    println!("playback devices: {devices:?}");
    assert!(!devices.is_empty());
    assert_eq!(engine.master_output().as_deref(), Some(devices[0].0.as_str()), "master autoconnects to the first device");
    engine.set_master_output(&[]);
    assert_eq!(engine.master_output(), None);
    engine.set_master_output(&devices[0].1);
    assert_eq!(engine.master_output().as_deref(), Some(devices[0].0.as_str()));
}

/// Make an EGL (desktop GL) context current on this thread, as egui's renderer
/// does on the app's main thread. Returns false if EGL is unavailable.
#[cfg(test)]
fn make_egl_context_current() -> bool {
    use std::ffi::c_void;
    type P = *mut c_void;
    unsafe {
        let Ok(lib) = libloading::Library::new("libEGL.so.1") else { return false };
        let get_platform_display: libloading::Symbol<unsafe extern "C" fn(u32, P, *const i32) -> P> = lib.get(b"eglGetPlatformDisplay\0").unwrap();
        let initialize: libloading::Symbol<unsafe extern "C" fn(P, *mut i32, *mut i32) -> u32> = lib.get(b"eglInitialize\0").unwrap();
        let bind_api: libloading::Symbol<unsafe extern "C" fn(u32) -> u32> = lib.get(b"eglBindAPI\0").unwrap();
        let choose_config: libloading::Symbol<unsafe extern "C" fn(P, *const i32, *mut P, i32, *mut i32) -> u32> = lib.get(b"eglChooseConfig\0").unwrap();
        let create_context: libloading::Symbol<unsafe extern "C" fn(P, P, P, *const i32) -> P> = lib.get(b"eglCreateContext\0").unwrap();
        let make_current: libloading::Symbol<unsafe extern "C" fn(P, P, P, P) -> u32> = lib.get(b"eglMakeCurrent\0").unwrap();
        let get_error: libloading::Symbol<unsafe extern "C" fn() -> i32> = lib.get(b"eglGetError\0").unwrap();
        const SURFACELESS_MESA: u32 = 0x31DD;
        let dpy = get_platform_display(SURFACELESS_MESA, std::ptr::null_mut(), std::ptr::null());
        if dpy.is_null() || initialize(dpy, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
            eprintln!("EGL display/initialize failed: {:#x}", get_error());
            return false;
        }
        bind_api(0x30A2); // EGL_OPENGL_API
        let attribs = [0x3040, 0x0008, 0x3033, 0x0001, 0x3038]; // RENDERABLE_TYPE = OPENGL_BIT, SURFACE_TYPE = PBUFFER_BIT
        let mut cfg: P = std::ptr::null_mut();
        let mut n = 0;
        if choose_config(dpy, attribs.as_ptr(), &mut cfg, 1, &mut n) == 0 || n == 0 {
            eprintln!("eglChooseConfig failed: {:#x} n={n}", get_error());
            return false;
        }
        let ctx = create_context(dpy, cfg, std::ptr::null_mut(), [0x3038].as_ptr());
        if ctx.is_null() {
            eprintln!("eglCreateContext failed: {:#x}", get_error());
            return false;
        }
        let ok = make_current(dpy, std::ptr::null_mut(), std::ptr::null_mut(), ctx) != 0;
        if !ok {
            eprintln!("eglMakeCurrent failed: {:#x}", get_error());
        }
        std::mem::forget(lib);
        ok
    }
}

/// Regression test for the x42 GUI crash: an OpenGL (GLX) plugin GUI while an
/// EGL context is current on the calling thread (as egui's is on the app's
/// main thread). Through the GUI thread it must work; the old in-thread path
/// must now only log the X error instead of killing the process.
#[test]
#[ignore]
fn x42_tuner_gui_with_egl_context_current() {
    use std::time::{Duration, Instant};
    crate::lv2ui::init_x11();
    let reg = registry();
    let id = "lv2:http://gareus.org/oss/lv2/tuna#one";
    let info = reg.get(id).expect("x42 tuner not installed").clone();
    assert!(info.has_gui);
    let shared = DeviceShared::new(&info);
    let created = reg.create(id, 48000.0, shared.clone(), None).unwrap();
    let link = created.lv2.unwrap();
    assert!(make_egl_context_current(), "could not make an EGL context current");

    // 1. Through the GUI thread: opens and keeps running without X errors.
    let mut gui = crate::lv2ui::GuiHost::new();
    gui.open(link.clone(), shared.clone(), "x42 tuner test".into(), 48000.0);
    let secs: u64 = std::env::var("STUDIO_GUI_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    let t0 = Instant::now();
    let mut msgs = vec![];
    while t0.elapsed() < Duration::from_secs(secs) {
        msgs.extend(gui.poll());
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(msgs.is_empty(), "GUI thread reported: {msgs:?}");
    assert_eq!(gui.open_count(), 1, "tuner GUI should be open");
    gui.close(&shared);
    assert_eq!(gui.open_count(), 0);
    println!("GUI thread: tuner GUI ran {secs} s without X errors");
    drop(gui); // stop the GUI thread so it does not collect the errors below

    // 2. The old path (GUI on the EGL thread) fails, but no longer exits.
    crate::lv2ui::take_x_errors();
    let mut win = crate::lv2ui::PluginWindow::open(link, shared, "x42 tuner on EGL thread".into(), 48000.0).unwrap();
    for _ in 0..10 {
        win.pump();
        std::thread::sleep(Duration::from_millis(16));
    }
    let errors = crate::lv2ui::take_x_errors();
    println!("EGL thread: {errors:?}");
    assert!(errors.iter().any(|e| e.contains("BadAccess")), "expected the GLX BadAccess to be reported, got {errors:?}");
}

#[test]
fn clips_and_takes_edit_through_one_region_interface() {
    use crate::engine::PlayContent;
    let (mut p, track, snippet, sources) = snippet_project();
    let take = p.snippet(snippet).unwrap().active.unwrap();
    let clip = p.place_snippet(snippet, 0, 48000).unwrap();
    // Same code edits a take and a clip.
    for id in [take, clip] {
        let (_, r) = p.region_mut(id, 48000).unwrap();
        let f = r.fields();
        *f.gain = 0.5;
        *f.fade_in = 100;
    }
    let (ctx, _) = p.region_mut(take, 48000).unwrap();
    assert_eq!(ctx.length, 96000);
    assert_eq!(ctx.owner, crate::project::RegionOwner::Take { track, snippet });
    // Take gain and fades reach playback on every loop pass; the placement
    // gain multiplies and its own fade-in wins at the placement start.
    p.clip_mut(clip).unwrap().length = 96000 * 2;
    let arr = crate::song::compile_song(&p, &sources, 48000);
    let clips = &arr.tracks[0].1;
    assert_eq!(clips.len(), 2);
    assert!(clips.iter().all(|c| (c.gain - 0.25).abs() < 1e-6));
    assert_eq!((clips[0].fade_in, clips[1].fade_in), (100, 100));
    assert!(matches!(&clips[1].content, PlayContent::Audio { offset, .. } if *offset == 192000));
}

#[test]
fn loop_wraps_when_a_block_ends_exactly_on_the_loop_end() {
    use crate::engine::loop_block;
    // 4 bars at 120 BPM / 48 kHz = 384000 frames = 1500 blocks of 256.
    let (ls, le) = (0, 384_000);
    let mut pos = 0i64;
    let mut wraps = 0;
    for _ in 0..1500 * 3 {
        let (p, split, next) = loop_block(pos, 256, 256, true, ls, le);
        assert!((0..le).contains(&p), "played outside the loop at {p}");
        assert_eq!(split, 256, "blocks align with the loop, no split needed");
        if next < pos {
            wraps += 1;
        }
        pos = next;
    }
    assert_eq!(wraps, 3, "loop must keep wrapping, not stop after one pass");
    // A block crossing the end splits there.
    assert_eq!(loop_block(383_900, 256, 256, true, ls, le), (383_900, 100, 156));
    // Count-in (before the loop start) plays on until the loop starts.
    assert_eq!(loop_block(-512, 256, 256, true, ls, le), (-512, 256, -256));
    // A position beyond a shortened loop wraps back inside it.
    assert_eq!(loop_block(500_000, 256, 256, true, ls, le).0, 500_000 - 384_000);
    // Not looping: plain advance.
    assert_eq!(loop_block(10, 256, 256, false, ls, le), (10, 256, 266));
}

/// Render `blocks` of 256 frames through an instrument, hitting `note` every
/// 0.25 s, pacing like real time so the plugin's worker thread can run.
fn play_drum(reg: &DeviceRegistry, id: &str, note: u8, blocks: usize) -> (f32, f32, f32) {
    let info = reg.get(id).unwrap();
    let mut dev = reg.create(id, 48000.0, DeviceShared::new(info), None).unwrap().device;
    let (mut peak_l, mut peak_r, mut max_diff) = (0f32, 0f32, 0f32);
    for b in 0..blocks {
        let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
        let midi = if b.is_multiple_of(47) { vec![MidiEvent::new(0, &[0x99, note, 120])] } else { vec![] };
        dev.process(&mut ProcessContext { left: &mut l, right: &mut r, midi: &midi });
        for (a, c) in l.iter().zip(&r) {
            peak_l = peak_l.max(a.abs());
            peak_r = peak_r.max(c.abs());
            max_diff = max_diff.max((a - c).abs());
        }
        std::thread::sleep(std::time::Duration::from_micros(5333));
    }
    (peak_l, peak_r, max_diff)
}

/// AVL "Black Pearl" drum kit: an LV2 instrument that loads its samples
/// through the worker extension. Skipped when avldrums is not installed.
#[test]
fn avl_drumkit_plays_via_worker_and_multi_out_downmix() {
    let reg = registry();
    let stereo = "lv2:http://gareus.org/oss/lv2/avldrums#BlackPearl";
    let multi = "lv2:http://gareus.org/oss/lv2/avldrums#BlackPearlMulti";
    let Some(info) = reg.get(stereo) else {
        println!("avldrums not installed, skipping");
        return;
    };
    assert!(info.unsupported.is_none(), "{:?}", info.unsupported);
    assert_eq!(info.category, DeviceCategory::Instrument);
    // ~2.5 s of playback: the kit loads in the background, then kicks sound.
    let (l, r, _) = play_drum(&reg, stereo, 36, 470);
    println!("Black Pearl stereo: peak L {l:.3} R {r:.3}");
    assert!(l > 0.01 && r > 0.01, "kick should sound once the kit has loaded");
    // Multi-out: the kick is a mono (centre) output, so L and R match.
    let (l, r, diff) = play_drum(&reg, multi, 36, 470);
    println!("Black Pearl multi: peak L {l:.3} R {r:.3}, max L/R difference {diff:.4}");
    assert!(l > 0.01 && r > 0.01, "multi-out kit should sound through the stereo downmix");
    assert!(diff < 1e-6, "centre (mono) drum should be identical on both sides");
}

/// Opens the AVL drum kit's own GUI while the plugin runs and gets hits,
/// so its notify-port events reach the GUI. Needs avldrums and a display.
#[test]
#[ignore]
fn avl_drumkit_gui() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    crate::lv2ui::init_x11();
    let reg = registry();
    let id = "lv2:http://gareus.org/oss/lv2/avldrums#BlackPearl";
    let info = reg.get(id).expect("avldrums not installed").clone();
    assert!(info.has_gui);
    let shared = DeviceShared::new(&info);
    let created = reg.create(id, 48000.0, shared.clone(), None).unwrap();
    let link = created.lv2.unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let mut dev = created.device;
    let audio = std::thread::spawn(move || {
        let mut b = 0u64;
        while !stop2.load(Ordering::Relaxed) {
            let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
            // Kick, snare, hi-hat pattern.
            let note = [36u8, 42, 38, 42][(b / 47 % 4) as usize];
            let midi = if b.is_multiple_of(47) { vec![MidiEvent::new(0, &[0x99, note, 110])] } else { vec![] };
            dev.process(&mut ProcessContext { left: &mut l, right: &mut r, midi: &midi });
            b += 1;
            std::thread::sleep(Duration::from_micros(5333));
        }
        dev
    });
    let mut gui = crate::lv2ui::GuiHost::new();
    gui.open(link, shared.clone(), "AVL drums test".into(), 48000.0);
    let secs: u64 = std::env::var("STUDIO_GUI_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let t0 = Instant::now();
    let mut msgs = vec![];
    while t0.elapsed() < Duration::from_secs(secs) {
        msgs.extend(gui.poll());
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(msgs.is_empty(), "{msgs:?}");
    assert_eq!(gui.open_count(), 1);
    gui.close(&shared);
    stop.store(true, Ordering::Relaxed);
    drop(audio.join().unwrap());
}

/// The device-style I/O used by the Windows backend, driven without a driver:
/// input-channel routing, MIDI fan-in, multi-channel output and the
/// latency bound on queued input.
#[test]
fn device_runner_routes_inputs_midi_and_outputs() {
    use crate::backend::TrackPorts;
    use crate::backend::devio::{DeviceRunner, MidiInEvent};
    use crate::engine::{Arrangement, Command, EngineShared, EngineTrack, Processor, TrackShared};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let shared = Arc::new(EngineShared::default());
    shared.master_gain.set(1.0);
    let arrangement = Arc::new(arc_swap::ArcSwap::from_pointee(Arrangement::default()));
    let (processor, mut q) = Processor::create(shared.clone(), arrangement);

    // Audio track monitoring device input channel 2 (mono).
    let audio = Arc::new(TrackShared::default());
    audio.monitor.store(true, Ordering::Relaxed);
    audio.in_channels[0].store(1, Ordering::Relaxed);
    assert!(q.commands.push(Command::AddTrack(EngineTrack::new(1, TrackKind::Audio, audio.clone(), TrackPorts::Device))).is_ok());
    // Instrument track with the built-in synth, listening to every MIDI port.
    let inst = Arc::new(TrackShared::default());
    inst.monitor.store(true, Ordering::Relaxed);
    inst.midi_source.store(-2, Ordering::Relaxed);
    assert!(q.commands.push(Command::AddTrack(EngineTrack::new(2, TrackKind::Instrument, inst.clone(), TrackPorts::Device))).is_ok());
    let reg = registry();
    let synth = reg.create("builtin:synth", 48000.0, DeviceShared::new(reg.get("builtin:synth").unwrap()), None).unwrap().device;
    assert!(q.commands.push(Command::InsertDevice { track: 2, index: 0, device: synth }).is_ok());

    let (mut in_tx, in_rx) = rtrb::RingBuffer::new(256 * 2 * 32);
    let (mut midi_tx, midi_rx) = rtrb::RingBuffer::new(64);
    let mut runner = DeviceRunner::new(processor, shared.clone(), 48000, midi_rx);
    runner.set_input(Some((in_rx, 2)));

    // 1. Channel 2 (0.5) reaches both master channels; ch 1 (0.1) is ignored;
    //    outputs 3 and 4 of a 4-channel device stay silent.
    for _ in 0..256 {
        in_tx.push(0.1).unwrap();
        in_tx.push(0.5).unwrap();
    }
    let mut out = vec![9.0f32; 256 * 4];
    runner.render(&mut out, 4);
    for f in out.chunks(4) {
        assert_eq!(f, [0.5, 0.5, 0.0, 0.0]);
    }
    assert_eq!(shared.buffer_size.load(Ordering::Relaxed), 256);

    // 2. A note from MIDI port 0 plays the synth (instrument listens to all ports).
    audio.monitor.store(false, Ordering::Relaxed);
    midi_tx.push(MidiInEvent::new(0, &[0x90, 60, 110]).unwrap()).unwrap();
    let mut peak = 0f32;
    for _ in 0..8 {
        let mut out = vec![0.0f32; 256 * 2];
        runner.render(&mut out, 2);
        peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
    }
    assert!(peak > 0.01, "synth should play the MIDI note, peak {peak}");
    assert!(inst.midi_activity.load(Ordering::Relaxed));

    // 3. Queued input beyond four blocks is dropped (bounded latency).
    for _ in 0..256 * 20 {
        let _ = in_tx.push(0.0);
        let _ = in_tx.push(0.0);
    }
    let mut out = vec![0.0f32; 256 * 2];
    runner.render(&mut out, 2);
    assert!(in_tx.slots() >= in_tx.buffer().capacity() - 256 * 2 * 4, "queued input should be capped at 4 blocks");

    // SysEx is not a short message.
    assert!(MidiInEvent::new(0, &[0xF0, 1, 2, 3, 0xF7]).is_none());
}

/// The Windows audio backend (cpal: WASAPI, or ASIO with the `asio` feature)
/// end to end: the built-in synth plays a MIDI clip through the device. Under
/// Wine on Linux, WASAPI goes through winepulse to PipeWire/PulseAudio:
///   WINEDEBUG=-all cargo test --target x86_64-pc-windows-gnu wasapi -- --ignored --nocapture
#[cfg(windows)]
#[test]
#[ignore]
fn windows_backend_plays_the_synth() {
    use crate::engine::{Arrangement, Command, Engine, PlayClip, PlayContent, TrackShared};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let mut engine = Engine::new("studio-windows-test");
    assert!(engine.is_running(), "{:?}", engine.error);
    println!("backend: {} at {} Hz", engine.backend_name(), engine.sample_rate);
    let reg = registry();
    let inst = Arc::new(TrackShared::default());
    engine.add_track(1, TrackKind::Instrument, inst.clone()).unwrap();
    let info = reg.get("builtin:synth").unwrap();
    let device = reg.create(&info.id, engine.sample_rate as f64, DeviceShared::new(info), None).unwrap().device;
    engine.send(Command::InsertDevice { track: 1, index: 0, device });
    let events = vec![(0u64, [0x90, 57, 110]), (engine.sample_rate as u64, [0x80, 57, 0])];
    engine.set_arrangement(Arrangement {
        tracks: vec![(1, vec![PlayClip { start: 0, length: engine.sample_rate as usize * 2, gain: 1.0, fade_in: 0, fade_out: 0, content: PlayContent::Midi { events: Arc::new(events) } }])],
    });
    engine.send(Command::Play);
    let t0 = Instant::now();
    let mut peak = 0f32;
    while t0.elapsed() < Duration::from_millis(800) {
        peak = peak.max(engine.shared.master_peak[0].take());
        std::thread::sleep(Duration::from_millis(20));
    }
    engine.send(Command::Stop);
    let devices = engine.playback_devices();
    println!(
        "peak {peak:.3}, period {} frames, DSP {:.1}%, xruns {}, position {}, devices {:?}, buffer sizes {:?}",
        engine.buffer_size(),
        engine.cpu_load(),
        engine.shared.xruns.load(Ordering::Relaxed),
        engine.shared.position.load(Ordering::Relaxed),
        devices.iter().map(|d| &d.0).collect::<Vec<_>>(),
        engine.buffer_sizes()
    );
    assert!(engine.buffer_size() > 0, "the device callback never ran");
    assert!(engine.shared.position.load(Ordering::Relaxed) > 0, "transport did not advance");
    assert!(peak > 0.01, "the synth should be audible on the master");
    assert!(!devices.is_empty());
}
