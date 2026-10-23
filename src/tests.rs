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
        s.takes.push(crate::project::Take { id, name: format!("Take {}", i + 1), content: ClipContent::Audio { source: 500 }, offset: off });
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
    s.takes.push(crate::project::Take { id: take, name: "t".into(), content: ClipContent::Audio { source: 500 }, offset: 0 });
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
    // Loop of 0.25 s with a 0.25 s count-in.
    let period = rate / 4;
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
    crate::lv2ui::init_x11_threads();
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
