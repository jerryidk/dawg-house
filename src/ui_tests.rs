//! Headless UI tests: drive the real app with synthetic pointer/keyboard input.

use crate::app::{BottomTab, StudioApp};
use crate::project::{ClipContent, TrackKind};
use eframe::egui::{Event, Key, Modifiers, PointerButton, Pos2, pos2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

fn harness() -> Harness<'static, StudioApp> {
    // Make sure we never attach to a real server from tests.
    unsafe { std::env::set_var("JACK_DEFAULT_SERVER", "studio-ui-test-no-server") };
    let mut h = Harness::builder().with_size([1400.0, 860.0]).with_step_dt(1.0 / 60.0).build_eframe(|cc| StudioApp::new(cc, None));
    // Advance the clock past egui's multi-click window (the harness starts at t=0).
    h.run_steps(60);
    h
}

fn button(h: &Harness<StudioApp>, p: Pos2, pressed: bool) {
    h.event(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
}

fn click(h: &mut Harness<StudioApp>, p: Pos2) {
    h.event(Event::PointerMoved(p));
    h.run_steps(1);
    button(h, p, true);
    h.run_steps(1);
    button(h, p, false);
    h.run_steps(1);
}

fn double_click(h: &mut Harness<StudioApp>, p: Pos2) {
    h.event(Event::PointerMoved(p));
    h.run_steps(1);
    for _ in 0..2 {
        button(h, p, true);
        h.run_steps(1);
        button(h, p, false);
        h.run_steps(1);
    }
    h.run_steps(1);
}

fn drag(h: &mut Harness<StudioApp>, a: Pos2, b: Pos2) {
    h.event(Event::PointerMoved(a));
    h.run_steps(1);
    button(h, a, true);
    h.run_steps(1);
    for i in 1..=8 {
        h.event(Event::PointerMoved(a + (b - a) * (i as f32 / 8.0)));
        h.run_steps(1);
    }
    button(h, b, false);
    h.run_steps(2);
}

fn lane(h: &Harness<StudioApp>, index: usize) -> eframe::egui::Rect {
    h.state().lane_rects[index].1
}

#[test]
fn starts_with_audio_and_instrument_track() {
    let h = harness();
    let app = h.state();
    let kinds: Vec<_> = app.project.tracks.iter().map(|t| t.kind).collect();
    assert_eq!(kinds, vec![TrackKind::Audio, TrackKind::Instrument]);
    assert_eq!(app.project.tracks[1].devices[0].device_id, "builtin:synth");
    assert_eq!(app.lane_rects.len(), 2);
    h.get_by_label(&format!("{} Devices", egui_phosphor::regular::FADERS));
}

#[test]
fn midi_clip_workflow() {
    let mut h = harness();
    // Double-click the instrument lane: creates a one-bar MIDI clip and opens the editor.
    let inst = lane(&h, 1);
    double_click(&mut h, pos2(inst.left() + 150.0, inst.center().y));
    {
        let app = h.state();
        let clips = &app.project.tracks[1].clips;
        assert_eq!(clips.len(), 1, "double-click should create a MIDI clip");
        assert!(matches!(clips[0].content, ClipContent::Midi { .. }));
        assert!(app.bottom == BottomTab::Editor);
    }
    h.run_steps(2);
    let grid = h.state().roll.grid_rect;
    assert!(grid.width() > 100.0, "piano roll not shown: {grid:?}");

    // Click adds a note, drag on empty space draws a longer one.
    click(&mut h, pos2(grid.left() + 40.0, grid.center().y));
    drag(&mut h, pos2(grid.left() + 200.0, grid.center().y - 30.0), pos2(grid.left() + 400.0, grid.center().y - 30.0));
    let notes = |h: &Harness<StudioApp>| match &h.state().project.tracks[1].clips[0].content {
        ClipContent::Midi { notes } => notes.clone(),
        _ => unreachable!(),
    };
    let n = notes(&h);
    assert_eq!(n.len(), 2, "{n:?}");
    assert!(n[1].length > n[0].length, "dragged note should be longer: {n:?}");
    assert!(n[1].pitch > n[0].pitch);

    // Right-click deletes a note; undo brings it back.
    let first = n[0];
    let x = grid.left() + 40.0 + 2.0;
    h.event(Event::PointerMoved(pos2(x, grid.center().y)));
    h.event(Event::PointerButton { pos: pos2(x, grid.center().y), button: PointerButton::Secondary, pressed: true, modifiers: Modifiers::NONE });
    h.event(Event::PointerButton { pos: pos2(x, grid.center().y), button: PointerButton::Secondary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(2);
    assert_eq!(notes(&h).len(), 1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(2);
    assert!(notes(&h).contains(&first));
}

#[test]
fn clip_move_split_delete_undo() {
    let mut h = harness();
    // Put an audio clip on track 1 directly (import path is covered by load_wav).
    {
        let app = h.state_mut();
        let data = crate::audio::AudioData::new("test.wav".into(), vec![(0..96000).map(|i| (i as f32 * 0.01).sin() * 0.5).collect()]);
        let track = app.project.tracks[0].id;
        app.add_audio_clip(track, data, 0, "sine".into());
    }
    h.run_steps(2);
    let l = lane(&h, 0);
    let rate = h.state().rate() as i64;

    // Drag the clip body right.
    drag(&mut h, pos2(l.left() + 40.0, l.center().y + 10.0), pos2(l.left() + 140.0, l.center().y + 10.0));
    let start = h.state().project.tracks[0].clips[0].start;
    assert!(start > rate, "clip should have moved right, start={start}");
    assert!(h.state().sel.clip.is_some());

    // Drag onto the instrument lane: incompatible kind, so it stays on the audio track.
    let inst = lane(&h, 1);
    drag(&mut h, pos2(l.left() + 160.0, l.center().y + 10.0), pos2(l.left() + 160.0, inst.center().y));
    assert_eq!(h.state().project.tracks[0].clips.len(), 1);

    // Split at the playhead with S.
    let clip = h.state().project.tracks[0].clips[0].clone();
    h.state_mut().engine.shared.position.store(clip.start + clip.length as i64 / 2, std::sync::atomic::Ordering::Relaxed);
    h.key_press(Key::S);
    h.run_steps(2);
    assert_eq!(h.state().project.tracks[0].clips.len(), 2);

    // Delete the selected clip, then undo twice.
    h.key_press(Key::Delete);
    h.run_steps(2);
    assert_eq!(h.state().project.tracks[0].clips.len(), 1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(2);
    assert_eq!(h.state().project.tracks[0].clips.len(), 1);
    assert_eq!(h.state().project.tracks[0].clips[0].length, clip.length);
}

#[test]
fn transport_and_track_buttons() {
    let mut h = harness();
    h.key_press(Key::Space);
    h.run_steps(2);
    assert!(h.state().playing());
    h.key_press(Key::Space);
    h.run_steps(2);
    assert!(!h.state().playing());

    h.get_by_label(&format!("{} Instrument", egui_phosphor::regular::PLUS)).click();
    h.run_steps(2);
    assert_eq!(h.state().project.tracks.len(), 3);
    assert_eq!(h.state().project.tracks[2].kind, TrackKind::Instrument);

    // Device browser opens from the devices panel and adds a built-in effect.
    h.get_by_label(&format!("{} Devices", egui_phosphor::regular::FADERS)).click();
    h.run_steps(2);
    h.get_by_label(&format!("{} Add device", egui_phosphor::regular::PLUS)).click();
    h.run_steps(2);
    h.get_by_label("Utility   ").click();
    h.run_steps(2);
    let devs: Vec<_> = h.state().project.tracks[2].devices.iter().map(|d| d.device_id.clone()).collect();
    assert_eq!(devs, vec!["builtin:synth", "builtin:utility"]);
}

#[test]
fn snippet_columns_place_record_and_edit() {
    let mut h = harness();
    {
        let app = h.state();
        assert!(app.project.tracks.iter().all(|t| t.snippets.len() == 1), "every track starts with a snippet");
        assert_eq!(app.snippet_rects.len(), 2);
    }
    let inst_track = h.state().project.tracks[1].id;
    let audio_snippet = h.state().project.tracks[0].snippets[0].id;
    let inst_snippet = h.state().project.tracks[1].snippets[0].id;
    let card = |h: &Harness<StudioApp>, id| h.state().snippet_rects.iter().find(|(s, _)| *s == id).unwrap().1;

    // "New snippet" adds a second snippet to the first column.
    let label = format!("{} New snippet", egui_phosphor::regular::PLUS);
    h.get_all_by_label(&label).next().unwrap().click();
    h.run_steps(2);
    assert_eq!(h.state().project.tracks[0].snippets.len(), 2);

    // Dragging the instrument snippet onto its lane creates a linked placement.
    let lane1 = lane(&h, 1);
    let from = card(&h, inst_snippet).left_center() + eframe::egui::vec2(20.0, 0.0);
    drag(&mut h, from, pos2(lane1.left() + 120.0, lane1.center().y));
    let clips = &h.state().project.track(inst_track).unwrap().clips;
    assert_eq!(clips.len(), 1, "drop should place the snippet");
    assert!(matches!(clips[0].content, ClipContent::Snippet { snippet } if snippet == inst_snippet));

    // A snippet cannot be dropped on another track's lane.
    let from = card(&h, audio_snippet).left_center() + eframe::egui::vec2(20.0, 0.0);
    drag(&mut h, from, pos2(lane1.left() + 400.0, lane1.center().y));
    assert_eq!(h.state().project.track(inst_track).unwrap().clips.len(), 1);

    // Clicking a card loops that snippet.
    let at = card(&h, inst_snippet).left_center() + eframe::egui::vec2(20.0, 0.0);
    click(&mut h, at);
    assert_eq!(h.state().scope, crate::song::PlayScope::Snippet(inst_snippet));
    assert_eq!(h.state().sel.snippet, Some(inst_snippet));

    // Takes tab: create an empty MIDI take and draw a note into it.
    h.get_by_label(&format!("{} Takes", egui_phosphor::regular::STACK)).click();
    h.run_steps(2);
    h.get_by_label(&format!("{} Empty MIDI take", egui_phosphor::regular::PLUS)).click();
    h.run_steps(3);
    assert!(h.state().bottom == BottomTab::Editor);
    let grid = h.state().roll.grid_rect;
    click(&mut h, pos2(grid.left() + 30.0, grid.center().y));
    let s = h.state().project.snippet(inst_snippet).unwrap().clone();
    let take = s.active_take().unwrap();
    assert!(matches!(&take.content, ClipContent::Midi { notes } if notes.len() == 1), "{:?}", take.content);

    // The linked placement now plays that note.
    let arr = crate::song::compile_song(&h.state().project, &h.state().sources, h.state().rate());
    let events = arr.tracks.iter().find(|(t, _)| *t == inst_track).unwrap().1.iter().any(|c| matches!(&c.content, crate::engine::PlayContent::Midi { events } if !events.is_empty()));
    assert!(events, "placement should carry the take's notes");

    // Space loops the snippet (offline transport) with the loop range set.
    h.key_press(Key::Space);
    h.run_steps(2);
    assert!(h.state().playing());
    assert!(h.state().engine.shared.loop_enabled.load(std::sync::atomic::Ordering::Relaxed));
}

/// Full app path for a plugin GUI: add TONE3000, click "Open GUI", save and
/// reopen the project with the plugin state. Needs TONE3000 and an X display.
#[test]
#[ignore]
fn tone3000_gui_from_app_and_state_roundtrip() {
    crate::lv2ui::init_x11();
    let mut h = harness();
    let track = h.state().project.tracks[0].id;
    h.state_mut().add_device(track, "lv2:https://www.tone3000.com/plugin");
    h.state_mut().sel.track = Some(track);
    h.state_mut().bottom = BottomTab::Devices;
    h.run_steps(3);
    h.get_by_label(&format!("{} Open GUI", egui_phosphor::regular::APP_WINDOW)).click();
    // Opening happens on the GUI thread; wait for it.
    for _ in 0..300 {
        h.run_steps(1);
        if h.state().gui.open_count() == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    assert_eq!(h.state().gui.open_count(), 1, "GUI window should be open");
    let secs: u64 = std::env::var("STUDIO_GUI_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    for _ in 0..secs * 60 {
        h.run_steps(1);
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    assert_eq!(h.state().gui.open_count(), 1, "GUI closed unexpectedly");

    // Save (plugin state goes into the file), then reopen.
    let dir = std::env::temp_dir().join(format!("studio-gui-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.studio");
    h.state_mut().save_project_to(path.clone());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("StateString"), "plugin state missing from project file");
    h.state_mut().open_project(path);
    h.run_steps(2);
    assert_eq!(h.state().gui.open_count(), 0, "GUIs close when the project is replaced");
    let slot = &h.state().project.track(track).unwrap().devices[0];
    assert!(slot.lv2.is_some() && slot.state.as_ref().is_some_and(|s| !s.is_empty()));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn export_song_to_wav_and_mp3() {
    use crate::export::{ExportJob, Format};
    use crate::project::{Clip, Note};
    let mut h = harness();
    // Empty timeline: nothing to export.
    assert!(h.state().build_export_plan(0.0).is_err());
    {
        let app = h.state_mut();
        let rate = app.rate() as usize;
        // One second of a 0.5 amplitude tone on the audio track at 0.5 s.
        let audio = app.project.tracks[0].id;
        let data = crate::audio::AudioData::new("tone.wav".into(), vec![(0..rate).map(|i| (i as f32 * 0.05).sin() * 0.5).collect()]);
        app.add_audio_clip(audio, data, rate as i64 / 2, "tone".into());
        // A chord on the instrument track (built-in synth) for the first beat.
        let inst = app.project.tracks[1].id;
        let id = app.project.alloc_id();
        let notes = [60u8, 64, 67].map(|pitch| Note { pitch, velocity: 100, start: 0.0, length: 1.0 }).to_vec();
        app.project.track_mut(inst).unwrap().clips.push(Clip {
            id,
            content: ClipContent::Midi { notes },
            name: "chord".into(),
            start: 0,
            offset: 0,
            length: rate as u64,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
            muted: false,
        });
    }
    let rate = h.state().rate();
    let dir = std::env::temp_dir().join(format!("studio-export-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // WAV 24-bit: length = song end (1.5 s) + 0.5 s tail.
    let wav = dir.join("song.wav");
    let job = ExportJob::start(h.state().build_export_plan(0.5).unwrap(), wav.clone(), Format::Wav24).unwrap();
    let stats = job.join().unwrap();
    let mut r = hound::WavReader::open(&wav).unwrap();
    let spec = r.spec();
    assert_eq!((spec.channels, spec.bits_per_sample, spec.sample_rate), (2, 24, rate));
    assert_eq!(r.duration() as u64, stats.frames);
    assert_eq!(stats.frames, (rate as u64 * 2));
    let samples: Vec<i32> = r.samples::<i32>().map(|s| s.unwrap()).collect();
    let peak_at = |from: f32, to: f32| {
        let (a, b) = ((from * rate as f32) as usize * 2, (to * rate as f32) as usize * 2);
        samples[a..b].iter().map(|s| s.unsigned_abs()).max().unwrap() as f32 / 8_388_607.0
    };
    assert!(peak_at(0.0, 0.4) > 0.01, "synth chord should sound in the first 0.4 s");
    assert!(peak_at(0.6, 1.4) > 0.3, "audio clip should sound from 0.5 s");
    assert!(peak_at(1.8, 2.0) < 0.001, "tail after everything ended should be silent");
    println!("WAV: {} frames, peak {:.2}", stats.frames, stats.peak);

    // MP3 320 kbps (when libmp3lame is available).
    if crate::export::mp3_available().is_ok() {
        let mp3 = dir.join("song.mp3");
        ExportJob::start(h.state().build_export_plan(0.5).unwrap(), mp3.clone(), Format::Mp3 { kbps: 320 }).unwrap().join().unwrap();
        let bytes = std::fs::read(&mp3).unwrap();
        // MPEG audio frame sync, and roughly 2 s at 320 kbps (= 80 kB).
        assert!(bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0, "not an MPEG audio stream");
        assert!((60_000..100_000).contains(&bytes.len()), "unexpected MP3 size {}", bytes.len());
        println!("MP3: {} bytes", bytes.len());
    }

    // The window flow: open with Ctrl+E, and the format radio buttons exist.
    h.key_press_modifiers(Modifiers::COMMAND, Key::E);
    h.run_steps(2);
    assert!(h.state().export.open);
    h.get_by_label("MP3 192 kbps");
    let _ = std::fs::remove_dir_all(dir);
}
