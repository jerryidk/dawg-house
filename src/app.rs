//! Application state and top-level UI: transport, menus, foldable panels,
//! device chain panel, device browser, recording and project I/O.

use crate::audio::{self, AudioData, Recorder};
use crate::device::{DeviceCategory, DeviceRegistry, DeviceShared};
use crate::engine::{AUDIO_PORT, Command, Engine, MAX_DEVICES, MAX_TRACKS, MIDI_PORT};
use crate::pianoroll::RollState;
use crate::project::{Clip, ClipContent, DeviceSlot, History, Id, Note, Project, SourceRef, Take, TrackKind};
use crate::song::{self, PlayScope};
use crate::theme::{self, ACCENT, PANEL_HI, REC, TEXT_DIM};
use crate::timeline::{Drag, View};
use crate::util::{db_to_gain, format_time};
use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText};
use egui_phosphor::regular as icon;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// "All MIDI inputs" marker stored in `Track::input_l` of instrument tracks.
pub const ALL_MIDI: &str = "*";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BottomTab {
    Takes,
    Editor,
    Devices,
}

#[derive(Default)]
pub struct Selection {
    pub track: Option<Id>,
    /// Clip on the timeline.
    pub clip: Option<Id>,
    pub snippet: Option<Id>,
    /// Take opened in the editor.
    pub take: Option<Id>,
}

/// Which panels are folded down to their header.
#[derive(Default)]
pub struct Folds {
    pub snippets: bool,
    pub timeline: bool,
    pub detail: bool,
}

/// Where a recording ends up.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RecTarget {
    /// New clips on the timeline at the record position.
    Timeline,
    /// New takes of a snippet, one per loop pass.
    Snippet { snippet: Id, period: u64, spb: f64 },
}

struct AudioTake {
    rec: Recorder,
    start: Option<i64>,
    target: RecTarget,
}

#[derive(Default)]
pub struct MidiTake {
    pub start: i64,
    open: HashMap<(u8, u8), (i64, u8)>,
    /// (start, end, pitch, velocity) in absolute samples.
    pub notes: Vec<(i64, i64, u8, u8)>,
}

impl MidiTake {
    /// Notes still held: (start, pitch).
    pub fn open_notes(&self) -> impl Iterator<Item = (i64, u8)> + '_ {
        self.open.iter().map(|(&(_, pitch), &(s, _))| (s, pitch))
    }
}

/// State of the export window.
pub struct ExportUi {
    pub open: bool,
    pub format: crate::export::Format,
    /// Seconds rendered after the last clip ends (reverb/release tails).
    pub tail: f32,
    pub job: Option<crate::export::ExportJob>,
}

impl Default for ExportUi {
    fn default() -> Self {
        ExportUi { open: false, format: crate::export::Format::Wav24, tail: 2.0, job: None }
    }
}

struct Browser {
    track: Id,
    filter: String,
    category: Option<DeviceCategory>,
}

pub struct StudioApp {
    pub engine: Engine,
    pub registry: DeviceRegistry,
    pub project: Project,
    pub sources: HashMap<Id, Arc<AudioData>>,
    pub history: History,
    pub view: View,
    pub sel: Selection,
    pub scope: PlayScope,
    /// Song position to return to when leaving snippet looping.
    song_pos: i64,
    pub folds: Folds,
    pub metronome: bool,
    pub count_in: bool,
    rec_target: RecTarget,
    /// Snippet currently being recorded into (its old takes are muted).
    pub recording_snippet: Option<Id>,
    pub drag: Option<Drag>,
    pub roll: RollState,
    pub bottom: BottomTab,
    pub lane_rects: Vec<(Id, egui::Rect)>,
    pub snippet_rects: Vec<(Id, egui::Rect)>,
    /// Plugin GUI windows (run on their own thread).
    pub gui: crate::lv2ui::GuiHost,
    pub export: ExportUi,
    pub audio_ports: Vec<String>,
    /// Playback devices (client name, ports) for the master output.
    pub output_devices: Vec<(String, Vec<String>)>,
    /// Device the master output is connected to right now.
    pub master_device: Option<String>,
    pub midi_ports: Vec<String>,
    pub midi_takes: HashMap<Id, MidiTake>,
    takes: Vec<AudioTake>,
    was_recording: bool,
    ports_refreshed: Option<Instant>,
    pub meters: HashMap<Id, [f32; 4]>,
    master_meter: [f32; 2],
    browser: Option<Browser>,
    status: Option<(String, Instant)>,
    project_path: Option<PathBuf>,
    pub arrangement_dirty: bool,
    last_frame: Instant,
}

impl StudioApp {
    pub fn new(cc: &eframe::CreationContext, project_path: Option<PathBuf>) -> Self {
        theme::apply(&cc.egui_ctx);
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let engine = Engine::new("studio");
        let registry = DeviceRegistry::new(engine.sample_rate as f32);
        let mut app = StudioApp {
            engine,
            registry,
            project: Project::default(),
            sources: HashMap::new(),
            history: History::default(),
            view: View::default(),
            sel: Selection::default(),
            scope: PlayScope::Song,
            song_pos: 0,
            folds: Folds::default(),
            metronome: false,
            count_in: true,
            rec_target: RecTarget::Timeline,
            recording_snippet: None,
            drag: None,
            roll: RollState::default(),
            bottom: BottomTab::Takes,
            lane_rects: vec![],
            snippet_rects: vec![],
            gui: crate::lv2ui::GuiHost::new(),
            export: ExportUi::default(),
            audio_ports: vec![],
            output_devices: vec![],
            master_device: None,
            midi_ports: vec![],
            midi_takes: HashMap::new(),
            takes: vec![],
            was_recording: false,
            ports_refreshed: None,
            meters: HashMap::new(),
            master_meter: [0.0; 2],
            browser: None,
            status: None,
            project_path: None,
            arrangement_dirty: true,
            last_frame: Instant::now(),
        };
        app.refresh_ports();
        if let Some(e) = app.engine.error.clone() {
            app.notify(e);
        }
        match project_path {
            Some(p) => app.open_project(p),
            None => {
                app.add_track(TrackKind::Audio);
                app.add_track(TrackKind::Instrument);
            }
        }
        app
    }

    pub fn rate(&self) -> u32 {
        self.engine.sample_rate
    }

    pub fn notify(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn position(&self) -> i64 {
        self.engine.shared.position.load(Ordering::Relaxed)
    }

    /// Playhead on the timeline: the transport position while the song plays,
    /// the remembered song position while a snippet loops.
    pub fn song_position(&self) -> i64 {
        match self.scope {
            PlayScope::Song => self.position(),
            PlayScope::Snippet(_) => self.song_pos,
        }
    }

    pub fn playing(&self) -> bool {
        self.engine.shared.playing.load(Ordering::Relaxed)
    }

    pub fn recording(&self) -> bool {
        self.engine.shared.recording.load(Ordering::Relaxed)
    }

    fn refresh_ports(&mut self) {
        self.audio_ports = self.engine.source_ports(AUDIO_PORT);
        self.midi_ports = self.engine.source_ports(MIDI_PORT);
        self.output_devices = self.engine.playback_devices();
        self.master_device = self.engine.master_output();
        self.ports_refreshed = Some(Instant::now());
    }

    // -----------------------------------------------------------------------
    // Tracks

    pub fn add_track(&mut self, kind: TrackKind) -> Option<Id> {
        if self.project.tracks.len() >= MAX_TRACKS {
            self.notify("Track limit reached");
            return None;
        }
        let audio_count = self.project.tracks.iter().filter(|t| t.kind == TrackKind::Audio).count();
        let default_audio = self.audio_ports.get(audio_count % self.audio_ports.len().max(1)).cloned();
        let t = self.project.new_track(kind);
        match kind {
            TrackKind::Audio => t.input_l = default_audio,
            TrackKind::Instrument => {
                t.input_l = Some(ALL_MIDI.into());
                t.monitor = true;
            }
        }
        let id = t.id;
        self.attach_track(id);
        if kind == TrackKind::Instrument {
            self.add_device(id, "builtin:synth");
        }
        // Every track starts with one empty snippet to record into.
        let snippet = self.project.new_snippet(id);
        self.sel = Selection { track: Some(id), snippet, ..Default::default() };
        Some(id)
    }

    /// Register a project track with the engine and connect its inputs.
    fn attach_track(&mut self, id: Id) {
        let Some(t) = self.project.track(id) else { return };
        if let Err(e) = self.engine.add_track(id, t.kind, t.shared.clone()) {
            self.notify(format!("Could not create ports: {e}"));
        }
        self.connect_track_inputs(id);
    }

    pub fn connect_track_inputs(&mut self, id: Id) {
        let Some(t) = self.project.track(id) else { return };
        match t.kind {
            TrackKind::Audio => {
                self.engine.connect_input(&format!("track{id}_in_L"), t.input_l.as_deref());
                let r = if t.stereo { t.input_r.as_deref() } else { None };
                self.engine.connect_input(&format!("track{id}_in_R"), r);
            }
            TrackKind::Instrument => {
                let port = format!("track{id}_midi_in");
                if t.input_l.as_deref() == Some(ALL_MIDI) {
                    self.engine.connect_input(&port, None);
                    if let Some(c) = self.engine.client() {
                        let dst = self.engine.port_name(&port);
                        for src in &self.midi_ports {
                            let _ = c.connect_ports_by_name(src, &dst);
                        }
                    }
                } else {
                    self.engine.connect_input(&port, t.input_l.as_deref());
                }
            }
        }
    }

    pub fn remove_track(&mut self, id: Id) {
        self.close_plugin_guis(id, None);
        self.engine.send(Command::RemoveTrack(id));
        self.project.tracks.retain(|t| t.id != id);
        if self.sel.track == Some(id) {
            self.sel = Selection::default();
        }
        self.arrangement_dirty = true;
    }

    // -----------------------------------------------------------------------
    // Devices

    pub fn add_device(&mut self, track: Id, device_id: &str) {
        let Some(info) = self.registry.get(device_id) else { return };
        let Some(t) = self.project.track(track) else { return };
        if t.devices.len() >= MAX_DEVICES {
            self.notify("Device chain is full");
            return;
        }
        let index = t.devices.len();
        let shared = DeviceShared::new(info);
        let name = info.name.clone();
        match self.registry.create(device_id, self.rate() as f64, shared.clone(), None) {
            Ok(created) => {
                self.engine.send(Command::InsertDevice { track, index, device: created.device });
                if let Some(t) = self.project.track_mut(track) {
                    t.devices.push(DeviceSlot {
                        device_id: device_id.into(),
                        name,
                        params: vec![],
                        bypass: false,
                        shared: Some(shared),
                        state: None,
                        lv2: created.lv2,
                    });
                }
            }
            Err(e) => self.notify(e),
        }
    }

    fn restore_device(&mut self, track: Id, index: usize) {
        let Some(slot) = self.project.track(track).and_then(|t| t.devices.get(index)).cloned() else { return };
        let Some(info) = self.registry.get(&slot.device_id) else {
            self.notify(format!("Missing device: {}", slot.name));
            return;
        };
        let shared = DeviceShared::new(info);
        for (sym, v) in &slot.params {
            if let Some(i) = info.params.iter().position(|p| &p.symbol == sym) {
                shared.params[i].set(*v);
            }
        }
        shared.bypass.store(slot.bypass, Ordering::Relaxed);
        match self.registry.create(&slot.device_id, self.rate() as f64, shared.clone(), slot.state.as_deref()) {
            Ok(created) => {
                self.engine.send(Command::InsertDevice { track, index, device: created.device });
                if let Some(s) = self.project.track_mut(track).and_then(|t| t.devices.get_mut(index)) {
                    s.shared = Some(shared);
                    s.lv2 = created.lv2;
                }
            }
            Err(e) => self.notify(e),
        }
    }

    // -----------------------------------------------------------------------
    // Plugin GUIs

    /// Open a plugin's own GUI in a separate window.
    pub fn open_plugin_gui(&mut self, track: Id, index: usize) {
        let Some(t) = self.project.track(track) else { return };
        let Some(slot) = t.devices.get(index) else { return };
        let (Some(link), Some(shared)) = (slot.lv2.clone(), slot.shared.clone()) else { return };
        if self.gui.is_open(&shared) {
            self.notify(format!("{} GUI is already open", slot.name));
            return;
        }
        // XStoreName takes Latin-1, so keep the title ASCII-safe.
        let title = format!("{} - {}", slot.name, t.name);
        let rate = self.rate() as f32;
        self.gui.open(link, shared, title, rate);
    }

    /// Close the GUIs of a track's devices (all of them, or only `index`).
    /// Must happen before the plugin instances can be freed.
    fn close_plugin_guis(&mut self, track: Id, index: Option<usize>) {
        let Some(t) = self.project.track(track) else { return };
        let doomed: Vec<Arc<DeviceShared>> = t
            .devices
            .iter()
            .enumerate()
            .filter(|(i, _)| index.is_none_or(|x| x == *i))
            .filter_map(|(_, d)| d.shared.clone())
            .collect();
        for s in doomed {
            self.gui.close(&s);
        }
    }

    fn remove_device(&mut self, track: Id, index: usize) {
        self.close_plugin_guis(track, Some(index));
        self.engine.send(Command::RemoveDevice { track, index });
        if let Some(t) = self.project.track_mut(track) {
            t.devices.remove(index);
        }
    }

    fn move_device(&mut self, track: Id, from: usize, to: usize) {
        self.engine.send(Command::MoveDevice { track, from, to });
        if let Some(t) = self.project.track_mut(track) {
            let d = t.devices.remove(from);
            t.devices.insert(to, d);
        }
    }

    /// Copy live parameter values into the slots so they get saved.
    fn sync_device_params(&mut self) {
        for t in &mut self.project.tracks {
            for slot in &mut t.devices {
                let (Some(shared), Some(info)) = (&slot.shared, self.registry.get(&slot.device_id)) else { continue };
                slot.params = info.params.iter().zip(&shared.params).map(|(p, v)| (p.symbol.clone(), v.get())).collect();
                slot.bypass = shared.bypass.load(Ordering::Relaxed);
                if let Some(link) = &slot.lv2 {
                    slot.state = crate::lv2::save_state(link);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Transport and recording

    pub fn toggle_play(&mut self) {
        if self.playing() {
            self.engine.send(Command::Stop);
        } else {
            self.engine.send(Command::Play);
        }
    }

    pub fn stop(&mut self) {
        if self.playing() {
            self.engine.send(Command::Stop);
        } else {
            self.locate(0);
        }
    }

    pub fn locate(&mut self, pos: i64) {
        self.engine.send(Command::Locate(pos));
    }

    /// Switch between playing the song and looping a snippet.
    pub fn set_scope(&mut self, scope: PlayScope) {
        if self.scope == scope || self.recording() {
            return;
        }
        match (self.scope, scope) {
            (PlayScope::Song, PlayScope::Snippet(_)) => {
                if self.playing() {
                    self.engine.send(Command::Stop);
                }
                self.song_pos = self.position();
                self.locate(0);
            }
            (PlayScope::Snippet(_), PlayScope::Song) => {
                if self.playing() {
                    self.engine.send(Command::Stop);
                }
                self.locate(self.song_pos);
            }
            _ => self.locate(0),
        }
        self.scope = scope;
        self.arrangement_dirty = true;
    }

    /// Select a snippet: it becomes its track's cued snippet and starts looping
    /// scope.
    pub fn select_snippet(&mut self, id: Id) {
        let Some(track) = self.project.snippet_track(id) else { return };
        if let Some(t) = self.project.track_mut(track) {
            t.cued = Some(id);
        }
        self.sel.snippet = Some(id);
        self.sel.track = Some(track);
        self.sel.clip = None;
        self.set_scope(PlayScope::Snippet(id));
        self.arrangement_dirty = true;
    }

    pub fn toggle_record(&mut self) {
        if self.recording() {
            self.engine.send(Command::Stop);
            return;
        }
        if !self.engine.is_running() {
            self.notify("Recording needs a running JACK / PipeWire engine");
            return;
        }
        match self.scope {
            PlayScope::Snippet(id) => self.record_snippet(id),
            PlayScope::Song => self.record_timeline(),
        }
    }

    fn take_path(&self, name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let safe: String = name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        self.project.audio_dir.join(format!("{safe}-{stamp}.wav"))
    }

    fn record_timeline(&mut self) {
        let armed: Vec<_> = self.project.tracks.iter().filter(|t| t.armed).map(|t| (t.id, t.kind, t.stereo, t.name.clone())).collect();
        if armed.is_empty() {
            self.notify("Arm a track first (the ● button in the track header)");
            return;
        }
        let mut producers = vec![];
        for (id, kind, stereo, name) in armed {
            if kind != TrackKind::Audio {
                continue; // MIDI is captured by the engine directly
            }
            let ch = if stereo { 2 } else { 1 };
            match Recorder::start(id, self.take_path(&name), ch, self.rate()) {
                Ok((rec, producer)) => {
                    producers.push((id, producer, ch));
                    self.takes.push(AudioTake { rec, start: None, target: RecTarget::Timeline });
                }
                Err(e) => self.notify(format!("Cannot record: {e}")),
            }
        }
        self.rec_target = RecTarget::Timeline;
        let pos = self.position().max(0);
        self.engine.send(Command::Record { takes: producers, from: pos, start_at: pos });
    }

    /// Loop-record into a snippet: count-in, then every pass becomes a take.
    pub fn record_snippet(&mut self, id: Id) {
        let rate = self.rate();
        let Some(track) = self.project.snippet_track(id) else { return };
        let Some(s) = self.project.snippet(id) else { return };
        let (period, spb, bar) = (s.length(rate), s.samples_per_beat(rate), (s.samples_per_beat(rate) * s.beats_per_bar as f64) as i64);
        let label = s.name.clone();
        let Some(t) = self.project.track_mut(track) else { return };
        // Recording into a snippet arms its track.
        t.armed = true;
        t.shared.armed.store(true, Ordering::Relaxed);
        let (kind, stereo, name) = (t.kind, t.stereo, format!("{}-{label}", t.name));
        let target = RecTarget::Snippet { snippet: id, period, spb };
        let mut producers = vec![];
        if kind == TrackKind::Audio {
            let ch = if stereo { 2 } else { 1 };
            match Recorder::start(track, self.take_path(&name), ch, rate) {
                Ok((rec, producer)) => {
                    producers.push((track, producer, ch));
                    self.takes.push(AudioTake { rec, start: None, target });
                }
                Err(e) => {
                    self.notify(format!("Cannot record: {e}"));
                    return;
                }
            }
        }
        self.scope = PlayScope::Snippet(id);
        self.sel.snippet = Some(id);
        self.rec_target = target;
        self.recording_snippet = Some(id);
        self.rebuild_arrangement();
        let start_at = if self.count_in { -bar } else { 0 };
        self.engine.send(Command::Record { takes: producers, from: 0, start_at });
    }

    fn add_source(&mut self, data: AudioData) -> Id {
        let source = self.project.alloc_id();
        self.project.sources.push(SourceRef { id: source, path: data.path.clone() });
        self.sources.insert(source, Arc::new(data));
        source
    }

    /// Append takes to a snippet and make the last one active.
    fn add_takes(&mut self, snippet: Id, contents: Vec<(ClipContent, u64)>) {
        if contents.is_empty() {
            return;
        }
        self.history.checkpoint(&self.project);
        let ids: Vec<Id> = contents.iter().map(|_| self.project.alloc_id()).collect();
        let Some(s) = self.project.snippet_mut(snippet) else { return };
        for (id, (content, offset)) in ids.into_iter().zip(contents) {
            let name = format!("Take {}", s.takes.len() + 1);
            s.takes.push(Take { id, name, content, offset });
            s.active = Some(id);
        }
        let msg = format!("{} take(s) in {}", s.takes.len(), s.name);
        self.notify(msg);
        self.arrangement_dirty = true;
    }

    fn poll_recording(&mut self) {
        let recording = self.recording();
        let record_start = self.engine.shared.record_start.load(Ordering::Relaxed);
        if recording {
            for t in self.takes.iter_mut().filter(|t| t.start.is_none()) {
                t.start = Some(record_start);
            }
        }

        // MIDI capture
        let only_track = match self.rec_target {
            RecTarget::Snippet { snippet, .. } => self.project.snippet_track(snippet),
            RecTarget::Timeline => None,
        };
        for e in self.engine.take_midi_recording() {
            if only_track.is_some_and(|t| t != e.track) {
                continue;
            }
            let take = self.midi_takes.entry(e.track).or_insert_with(|| MidiTake { start: record_start, ..Default::default() });
            let (status, ch, pitch, vel) = (e.data[0] & 0xF0, e.data[0] & 0x0F, e.data[1] & 0x7F, e.data[2]);
            match status {
                0x90 if vel > 0 => {
                    take.open.insert((ch, pitch), (e.pos, vel));
                }
                0x80 | 0x90 => {
                    if let Some((s, v)) = take.open.remove(&(ch, pitch)) {
                        take.notes.push((s, e.pos.max(s + 1), pitch, v));
                    }
                }
                _ => {}
            }
        }
        if self.was_recording && !recording {
            let stop = record_start + self.engine.shared.recorded_frames.load(Ordering::Relaxed);
            let takes: Vec<_> = self.midi_takes.drain().collect();
            for (track, mut take) in takes {
                for ((_, pitch), (s, v)) in take.open.drain() {
                    take.notes.push((s, stop.max(s + 1), pitch, v));
                }
                if take.notes.is_empty() {
                    continue;
                }
                match self.rec_target {
                    RecTarget::Snippet { snippet, period, spb } => {
                        let rel: Vec<_> = take.notes.iter().map(|&(s, e, p, v)| (s - take.start, e - take.start, p, v)).collect();
                        let passes = song::midi_passes(&rel, period, spb);
                        self.add_takes(snippet, passes.into_iter().map(|notes| (ClipContent::Midi { notes }, 0)).collect());
                    }
                    RecTarget::Timeline => self.add_midi_clip(track, &take, stop),
                }
            }
            self.recording_snippet = None;
            self.arrangement_dirty = true;
        }
        self.was_recording = recording;

        // Audio takes finish once the engine has released the ring buffer.
        let mut i = 0;
        while i < self.takes.len() {
            if !self.takes[i].rec.is_finished() {
                i += 1;
                continue;
            }
            let take = self.takes.remove(i);
            let start = take.start.unwrap_or(0);
            let track = take.rec.track_id;
            match take.rec.join() {
                Ok(data) if data.len() > 0 => match take.target {
                    RecTarget::Timeline => {
                        let name = self.project.track(track).map(|t| format!("{} take", t.name)).unwrap_or_default();
                        self.history.checkpoint(&self.project);
                        self.add_audio_clip(track, data, start, name);
                    }
                    RecTarget::Snippet { snippet, period, .. } => {
                        let total = data.len() as u64;
                        let source = self.add_source(data);
                        let passes = song::audio_passes(total, period);
                        self.add_takes(snippet, passes.into_iter().map(|off| (ClipContent::Audio { source }, off)).collect());
                    }
                },
                Ok(_) => {}
                Err(e) => self.notify(format!("Recording failed: {e}")),
            }
        }
    }

    fn add_midi_clip(&mut self, track: Id, take: &MidiTake, stop: i64) {
        let spb = self.project.samples_per_beat(self.rate());
        let end = take.notes.iter().map(|n| n.1).max().unwrap_or(stop).max(stop);
        let notes = take
            .notes
            .iter()
            .map(|&(s, e, pitch, velocity)| Note { pitch, velocity, start: (s - take.start) as f64 / spb, length: (e - s) as f64 / spb })
            .collect();
        self.history.checkpoint(&self.project);
        let id = self.project.alloc_id();
        if let Some(t) = self.project.track_mut(track) {
            t.clips.push(Clip {
                id,
                content: ClipContent::Midi { notes },
                name: format!("{} take", t.name),
                start: take.start,
                offset: 0,
                length: (end - take.start).max(1) as u64,
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
                muted: false,
            });
            self.arrangement_dirty = true;
        }
    }

    /// Live recording overlay for the timeline (timeline recordings only).
    pub fn audio_take_live(&self, track: Id) -> Option<&Recorder> {
        self.takes.iter().find(|t| t.rec.track_id == track && t.target == RecTarget::Timeline).map(|t| &t.rec)
    }

    pub fn add_audio_clip(&mut self, track: Id, data: AudioData, start: i64, name: String) {
        let len = data.len() as u64;
        let source = self.add_source(data);
        let id = self.project.alloc_id();
        if let Some(t) = self.project.track_mut(track) {
            t.clips.push(Clip {
                id,
                content: ClipContent::Audio { source },
                name,
                start,
                offset: 0,
                length: len,
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
                muted: false,
            });
        }
        self.arrangement_dirty = true;
    }

    pub fn import_files(&mut self, files: Vec<PathBuf>, track: Option<Id>) {
        let track = track
            .filter(|id| self.project.track(*id).is_some_and(|t| t.kind == TrackKind::Audio))
            .or(self.sel.track.filter(|id| self.project.track(*id).is_some_and(|t| t.kind == TrackKind::Audio)))
            .or_else(|| self.add_track(TrackKind::Audio));
        let Some(track) = track else { return };
        self.history.checkpoint(&self.project);
        let mut at = self.song_position();
        for f in files {
            match audio::load_wav(&f, self.rate()) {
                Ok(data) => {
                    let len = data.len() as i64;
                    let name = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    self.add_audio_clip(track, data, at, name);
                    at += len;
                }
                Err(e) => self.notify(e),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Clip editing helpers (shared by keyboard shortcuts and menus)

    pub fn split_selected(&mut self) {
        let pos = self.song_position();
        let targets: Vec<Id> = match self.sel.clip {
            Some(c) => vec![c],
            None => self
                .sel
                .track
                .and_then(|t| self.project.track(t))
                .map(|t| t.clips.iter().filter(|c| c.start < pos && c.end() > pos).map(|c| c.id).collect())
                .unwrap_or_default(),
        };
        if targets.is_empty() {
            return;
        }
        self.history.checkpoint(&self.project);
        for c in targets {
            self.project.split_clip(c, pos);
        }
        self.arrangement_dirty = true;
    }

    pub fn delete_selected_clip(&mut self) {
        if let Some(c) = self.sel.clip.take() {
            self.history.checkpoint(&self.project);
            self.project.delete_clip(c);
            self.arrangement_dirty = true;
        }
    }

    pub fn duplicate_selected_clip(&mut self) {
        if let Some(c) = self.sel.clip {
            self.history.checkpoint(&self.project);
            self.sel.clip = self.project.duplicate_clip(c);
            self.arrangement_dirty = true;
        }
    }

    pub fn undo(&mut self) {
        if self.history.undo(&mut self.project) {
            self.arrangement_dirty = true;
        }
    }

    pub fn redo(&mut self) {
        if self.history.redo(&mut self.project) {
            self.arrangement_dirty = true;
        }
    }

    /// Publish what should play to the audio thread, with loop and metronome
    /// settings for the current scope.
    pub fn rebuild_arrangement(&mut self) {
        let rate = self.rate();
        let sh = self.engine.shared.clone();
        let (arr, loop_len, spb, beats) = match self.scope {
            PlayScope::Snippet(id) if self.project.snippet(id).is_some() => {
                let mute = self.recording_snippet == Some(id);
                let (arr, len) = song::compile_snippet_loop(&self.project, &self.sources, rate, id, mute);
                let s = self.project.snippet(id).unwrap();
                (arr, len, s.samples_per_beat(rate), s.beats_per_bar)
            }
            _ => {
                self.scope = PlayScope::Song;
                (song::compile_song(&self.project, &self.sources, rate), 0, self.project.samples_per_beat(rate), 4)
            }
        };
        sh.loop_enabled.store(loop_len > 0, Ordering::Relaxed);
        sh.loop_start.store(0, Ordering::Relaxed);
        sh.loop_end.store(loop_len as i64, Ordering::Relaxed);
        sh.metro_samples_per_beat.set(spb as f32);
        sh.metro_beats_per_bar.store(beats, Ordering::Relaxed);
        sh.metro_anchor.store(0, Ordering::Relaxed);
        sh.metronome.store(self.metronome, Ordering::Relaxed);
        self.engine.set_arrangement(arr);
    }

    // -----------------------------------------------------------------------
    // Project files

    fn teardown(&mut self) {
        if self.recording() || self.playing() {
            self.engine.send(Command::Stop);
        }
        self.gui.close_all();
        for t in &self.project.tracks {
            self.engine.send(Command::RemoveTrack(t.id));
        }
        self.sources.clear();
        self.history.clear();
        self.sel = Selection::default();
        self.scope = PlayScope::Song;
        self.recording_snippet = None;
        self.drag = None;
        self.arrangement_dirty = true;
    }

    pub fn new_project(&mut self) {
        self.teardown();
        self.project = Project::default();
        self.project_path = None;
        self.add_track(TrackKind::Audio);
        self.add_track(TrackKind::Instrument);
        self.locate(0);
    }

    pub fn open_project(&mut self, path: PathBuf) {
        let project: Project = match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
            Ok(p) => p,
            Err(e) => {
                self.notify(format!("Could not open {}: {e}", path.display()));
                return;
            }
        };
        self.teardown();
        self.project = project;
        let mut missing = 0;
        for s in self.project.sources.clone() {
            match audio::load_wav(&s.path, self.rate()) {
                Ok(d) => {
                    self.sources.insert(s.id, Arc::new(d));
                }
                Err(_) => missing += 1,
            }
        }
        let ids: Vec<Id> = self.project.tracks.iter().map(|t| t.id).collect();
        for id in ids {
            self.attach_track(id);
            let n = self.project.track(id).map_or(0, |t| t.devices.len());
            for i in 0..n {
                self.restore_device(id, i);
            }
        }
        if let Some(name) = self.project.master_output.clone()
            && let Some((_, ports)) = self.engine.playback_devices().into_iter().find(|(d, _)| *d == name)
        {
            self.engine.set_master_output(&ports);
        }
        self.refresh_ports();
        self.project_path = Some(path.clone());
        self.notify(if missing > 0 {
            format!("Opened {} ({missing} audio files missing)", path.display())
        } else {
            format!("Opened {}", path.display())
        });
        self.locate(0);
    }

    pub fn save_project(&mut self, save_as: bool) {
        let path = match (&self.project_path, save_as) {
            (Some(p), false) => p.clone(),
            _ => {
                let dir = self.project.audio_dir.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                let _ = std::fs::create_dir_all(&dir);
                match rfd::FileDialog::new().add_filter("Studio project", &["studio"]).set_directory(&dir).set_file_name("project.studio").save_file() {
                    Some(p) => p.with_extension("studio"),
                    None => return,
                }
            }
        };
        self.save_project_to(path);
    }

    /// Write the project to `path` (plugin parameters and state included).
    pub fn save_project_to(&mut self, path: PathBuf) {
        self.sync_device_params();
        // Only keep sources still referenced by clips or takes.
        let used = self.project.used_sources();
        self.project.sources.retain(|s| used.contains(&s.id));
        match serde_json::to_string_pretty(&self.project).map_err(|e| e.to_string()).and_then(|s| std::fs::write(&path, s).map_err(|e| e.to_string())) {
            Ok(()) => {
                self.notify(format!("Saved {}", path.display()));
                self.project_path = Some(path);
            }
            Err(e) => self.notify(format!("Save failed: {e}")),
        }
    }

    // -----------------------------------------------------------------------
    // Per-frame housekeeping

    fn sync_shared(&mut self, dt: f32) {
        let decay = (-dt * 6.0).exp();
        for t in &self.project.tracks {
            let s = &t.shared;
            s.gain.set(db_to_gain(t.volume_db));
            s.pan.set(t.pan);
            s.mute.store(t.mute, Ordering::Relaxed);
            s.solo.store(t.solo, Ordering::Relaxed);
            s.monitor.store(t.monitor, Ordering::Relaxed);
            s.armed.store(t.armed, Ordering::Relaxed);
            s.stereo.store(t.stereo, Ordering::Relaxed);
            let m = self.meters.entry(t.id).or_default();
            let new = [s.in_peak[0].take(), s.in_peak[1].take(), s.out_peak[0].take(), s.out_peak[1].take()];
            for (v, n) in m.iter_mut().zip(new) {
                *v = n.max(*v * decay);
            }
        }
        self.engine.shared.master_gain.set(db_to_gain(self.project.master_db));
        for (i, v) in self.master_meter.iter_mut().enumerate() {
            *v = self.engine.shared.master_peak[i].take().max(*v * decay);
        }
        if !self.engine.is_running() && self.playing() {
            // Offline: advance the playhead from the UI clock.
            let p = self.position() + (dt as f64 * self.rate() as f64) as i64;
            self.engine.shared.position.store(p, Ordering::Relaxed);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let ctrl = |k| KeyboardShortcut::new(Modifiers::COMMAND, k);
        let ctrl_shift = |k| KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, k);
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl_shift(Key::Z))) {
            self.redo();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl(Key::Z))) {
            self.undo();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl(Key::Y))) {
            self.redo();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl_shift(Key::S))) {
            self.save_project(true);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl(Key::S))) {
            self.save_project(false);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl(Key::O))) {
            self.open_dialog();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl_shift(Key::T))) {
            self.add_track(TrackKind::Instrument);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl(Key::T))) {
            self.add_track(TrackKind::Audio);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl(Key::D))) {
            self.duplicate_selected_clip();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl(Key::I))) {
            self.import_dialog();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&ctrl(Key::E))) {
            self.export.open = true;
        }
        if ctx.egui_wants_keyboard_input() {
            return; // typing in a text field
        }
        let pressed = |k| ctx.input(|i| i.key_pressed(k) && i.modifiers.is_none());
        if pressed(Key::Space) {
            self.toggle_play();
        }
        if pressed(Key::R) {
            self.toggle_record();
        }
        if pressed(Key::Home) || pressed(Key::Enter) {
            self.locate(0);
        }
        if pressed(Key::S) {
            self.split_selected();
        }
        if pressed(Key::Q) && self.bottom == BottomTab::Editor {
            self.quantize_selected_clip();
        }
        if pressed(Key::Delete) || pressed(Key::Backspace) {
            if self.bottom == BottomTab::Editor && self.roll.selected.is_some() {
                self.delete_selected_note();
            } else {
                self.delete_selected_clip();
            }
        }
        if pressed(Key::Plus) || pressed(Key::Equals) {
            self.view.zoom(1.25);
        }
        if pressed(Key::Minus) {
            self.view.zoom(0.8);
        }
    }

    fn open_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new().add_filter("Studio project", &["studio"]).pick_file() {
            self.open_project(p);
        }
    }

    fn import_dialog(&mut self) {
        if let Some(files) = rfd::FileDialog::new().add_filter("WAV audio", &["wav", "WAV"]).pick_files() {
            self.import_files(files, None);
        }
    }

    // -----------------------------------------------------------------------
    // UI sections

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New project").clicked() {
                    self.new_project();
                }
                if ui.add(egui::Button::new("Open…").shortcut_text("Ctrl+O")).clicked() {
                    self.open_dialog();
                }
                if ui.add(egui::Button::new("Save").shortcut_text("Ctrl+S")).clicked() {
                    self.save_project(false);
                }
                if ui.add(egui::Button::new("Save as…").shortcut_text("Ctrl+Shift+S")).clicked() {
                    self.save_project(true);
                }
                ui.separator();
                if ui.add(egui::Button::new("Import audio…").shortcut_text("Ctrl+I")).clicked() {
                    self.import_dialog();
                }
                if ui.add(egui::Button::new("Export audio…").shortcut_text("Ctrl+E")).clicked() {
                    self.export.open = true;
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.add_enabled(self.history.can_undo(), egui::Button::new("Undo").shortcut_text("Ctrl+Z")).clicked() {
                    self.undo();
                }
                if ui.add_enabled(self.history.can_redo(), egui::Button::new("Redo").shortcut_text("Ctrl+Shift+Z")).clicked() {
                    self.redo();
                }
                ui.separator();
                if ui.add(egui::Button::new("Split at playhead").shortcut_text("S")).clicked() {
                    self.split_selected();
                }
                if ui.add_enabled(self.sel.clip.is_some(), egui::Button::new("Duplicate clip").shortcut_text("Ctrl+D")).clicked() {
                    self.duplicate_selected_clip();
                }
                if ui.add_enabled(self.sel.clip.is_some(), egui::Button::new("Delete clip").shortcut_text("Del")).clicked() {
                    self.delete_selected_clip();
                }
            });
            ui.menu_button("Track", |ui| {
                if ui.add(egui::Button::new("Add audio track").shortcut_text("Ctrl+T")).clicked() {
                    self.add_track(TrackKind::Audio);
                }
                if ui.add(egui::Button::new("Add instrument track").shortcut_text("Ctrl+Shift+T")).clicked() {
                    self.add_track(TrackKind::Instrument);
                }
                ui.separator();
                if ui.add_enabled(self.sel.track.is_some(), egui::Button::new("Delete selected track")).clicked()
                    && let Some(id) = self.sel.track
                {
                    self.remove_track(id);
                }
                if ui.button("Refresh input ports").clicked() {
                    self.refresh_ports();
                }
            });
        });

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let playing = self.playing();
            let recording = self.recording();
            let big = |s: &str| RichText::new(s).size(18.0);
            if ui.add(egui::Button::new(big(icon::SKIP_BACK)).min_size(egui::vec2(34.0, 28.0))).on_hover_text("Return to start (Home)").clicked() {
                self.locate(0);
            }
            if ui.add(egui::Button::new(big(icon::STOP)).min_size(egui::vec2(34.0, 28.0))).on_hover_text("Stop").clicked() {
                self.stop();
            }
            let play = egui::Button::new(big(if playing { icon::PAUSE } else { icon::PLAY }).color(if playing { ACCENT } else { theme::TEXT }))
                .min_size(egui::vec2(34.0, 28.0));
            if ui.add(play).on_hover_text("Play / pause (Space)").clicked() {
                self.toggle_play();
            }
            let rec = egui::Button::new(big(icon::RECORD).color(if recording { Color32::WHITE } else { REC }))
                .fill(if recording { REC } else { PANEL_HI })
                .min_size(egui::vec2(34.0, 28.0));
            if ui.add(rec).on_hover_text("Record armed tracks (R)").clicked() {
                self.toggle_record();
            }

            ui.add_space(10.0);
            let pos = self.position();
            // Bars/beats follow the looping snippet's tempo and meter, else the song's.
            let snippet = match self.scope {
                PlayScope::Snippet(id) => self.project.snippet(id).cloned(),
                PlayScope::Song => None,
            };
            let (spb, bpb) = snippet.as_ref().map_or((self.project.samples_per_beat(self.rate()), 4.0), |s| (s.samples_per_beat(self.rate()), s.beats_per_bar as f64));
            let bar_beat = if pos < 0 {
                format!("count-in {}", ((-pos) as f64 / spb).ceil() as i64)
            } else {
                let beats = pos as f64 / spb;
                format!("{:>3}.{}.{:02}", (beats / bpb) as i64 + 1, (beats % bpb) as i64 + 1, ((beats.fract()) * 100.0) as i64)
            };
            egui::Frame::new().fill(theme::BG).corner_radius(6).inner_margin(egui::Margin::symmetric(10, 3)).show(ui, |ui| {
                ui.label(RichText::new(bar_beat).monospace().size(17.0).color(if pos < 0 { REC } else { ACCENT }));
                ui.label(RichText::new(format_time(pos, self.rate())).monospace().size(13.0).color(TEXT_DIM));
            });

            ui.add_space(8.0);
            // Scope: song or looping snippet.
            let song = ui.selectable_label(self.scope == PlayScope::Song, format!("{} Song", icon::LIST)).on_hover_text("Play the assembled timeline");
            if song.clicked() {
                self.set_scope(PlayScope::Song);
            }
            if let Some(s) = &snippet {
                ui.label(
                    RichText::new(format!("{} {} · {} bars · {:.0} BPM · {}", icon::REPEAT, s.name, s.bars, s.bpm, s.meter()))
                        .color(ACCENT),
                )
                .on_hover_text("Looping this snippet with the other tracks' cued snippets");
            }

            ui.add_space(8.0);
            let metro = egui::Button::new(RichText::new(icon::METRONOME).size(15.0).color(if self.metronome { ACCENT } else { TEXT_DIM }));
            if ui.add(metro).on_hover_text("Metronome").clicked() {
                self.metronome = !self.metronome;
                self.engine.shared.metronome.store(self.metronome, Ordering::Relaxed);
            }
            let ci = egui::Button::new(RichText::new(format!("{} 1 bar", icon::TIMER)).color(if self.count_in { ACCENT } else { TEXT_DIM }));
            if ui.add(ci).on_hover_text("Count-in before recording into a snippet").clicked() {
                self.count_in = !self.count_in;
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Song BPM").small().color(TEXT_DIM));
            if ui.add(egui::DragValue::new(&mut self.project.bpm).range(30.0..=300.0).speed(0.2).fixed_decimals(1)).on_hover_text("Timeline grid and default tempo for new snippets").changed() {
                self.arrangement_dirty = true;
            }
            ui.add_space(6.0);
            let snap = egui::Button::new(RichText::new(format!("{} Snap", icon::MAGNET)).color(if self.project.snap { ACCENT } else { TEXT_DIM }));
            if ui.add(snap).on_hover_text("Snap to grid (hold Alt while dragging to bypass)").clicked() {
                self.project.snap = !self.project.snap;
            }
            if theme::icon_button(ui, icon::MAGNIFYING_GLASS_MINUS, "Zoom out (-)").clicked() {
                self.view.zoom(0.8);
            }
            if theme::icon_button(ui, icon::MAGNIFYING_GLASS_PLUS, "Zoom in (+)").clicked() {
                self.view.zoom(1.25);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                theme::meter(ui, 140.0, self.master_meter, "Master output");
                ui.add(egui::Slider::new(&mut self.project.master_db, -60.0..=6.0).show_value(false).trailing_fill(true))
                    .on_hover_text(format!("Master {:.1} dB", self.project.master_db));
                self.master_output_picker(ui);
                ui.label(RichText::new(format!("{} Master", icon::SPEAKER_HIGH)).color(TEXT_DIM));
            });
        });
        ui.add_space(4.0);
    }

    /// Master output device selector (next to the master volume).
    fn master_output_picker(&mut self, ui: &mut egui::Ui) {
        if !self.engine.is_running() {
            return;
        }
        let current = self.master_device.clone().unwrap_or_else(|| "Not connected".into());
        let mut choice: Option<Option<usize>> = None;
        egui::ComboBox::from_id_salt("master_output")
            .selected_text(RichText::new(short_name(&current, 26)).small())
            .width(170.0)
            .show_ui(ui, |ui| {
                theme::section_label(ui, "Master output device");
                for (i, (name, ports)) in self.output_devices.iter().enumerate() {
                    let label = format!("{name}  ({} ch)", ports.len());
                    if ui.selectable_label(self.master_device.as_deref() == Some(name.as_str()), label).clicked() {
                        choice = Some(Some(i));
                    }
                }
                if self.output_devices.is_empty() {
                    ui.label(RichText::new("No playback devices found").color(TEXT_DIM));
                }
                ui.separator();
                if ui.selectable_label(self.master_device.is_none(), "Not connected").clicked() {
                    choice = Some(None);
                }
            })
            .response
            .on_hover_text("Where the master output plays: the master L/R go to the device's first two channels");
        match choice {
            Some(Some(i)) => {
                let (name, ports) = self.output_devices[i].clone();
                self.engine.set_master_output(&ports);
                self.project.master_output = Some(name.clone());
                self.master_device = Some(name);
            }
            Some(None) => {
                self.engine.set_master_output(&[]);
                self.project.master_output = None;
                self.master_device = None;
            }
            None => {}
        }
    }

    /// Period size selector with the latency each choice gives.
    fn buffer_size_picker(&mut self, ui: &mut egui::Ui) {
        let rate = self.rate().max(1) as f32;
        let current = self.engine.buffer_size();
        let ms = |n: u32| n as f32 / rate * 1000.0;
        let mut pick = None;
        egui::ComboBox::from_id_salt("buffer_size")
            .selected_text(RichText::new(format!("{current} frames · {:.1} ms", ms(current))).color(TEXT_DIM))
            .width(150.0)
            .show_ui(ui, |ui| {
                theme::section_label(ui, "Frames per period — latency");
                for n in [32u32, 64, 128, 256, 512, 1024, 2048, 4096] {
                    let text = format!("{n:>5} frames   {:>6.1} ms   (round trip ≈ {:.1} ms)", ms(n), 2.0 * ms(n));
                    if ui.selectable_label(n == current, RichText::new(text).monospace()).clicked() {
                        pick = Some(n);
                    }
                }
            })
            .response
            .on_hover_text(if self.engine.pipewire {
                "Smaller = lower latency but more CPU and risk of xruns.\nUnder PipeWire this forces the quantum of the whole audio graph."
            } else {
                "Smaller = lower latency but more CPU and risk of xruns."
            });
        if let Some(n) = pick {
            match self.engine.set_buffer_size(n) {
                Ok(()) => self.notify(format!("Requested {n} frames ({:.1} ms)", ms(n))),
                Err(e) => self.notify(e),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Export

    /// End of the last clip on the timeline (samples).
    pub fn song_end(&self) -> i64 {
        self.project.tracks.iter().flat_map(|t| t.clips.iter().filter(|c| !c.muted).map(|c| c.end())).max().unwrap_or(0)
    }

    /// Prepare an offline render of the song: the playback list plus fresh
    /// copies of every device with current parameters and plugin state.
    pub fn build_export_plan(&self, tail_secs: f32) -> Result<crate::export::RenderPlan, String> {
        let rate = self.rate();
        let end = self.song_end();
        if end <= 0 {
            return Err("The timeline is empty: place snippets or clips on it first".into());
        }
        let any_solo = self.project.tracks.iter().any(|t| t.solo);
        let mut tracks = vec![];
        for t in &self.project.tracks {
            let mut devices = vec![];
            for slot in &t.devices {
                let Some(info) = self.registry.get(&slot.device_id) else { continue };
                let shared = DeviceShared::new(info);
                if let Some(live) = &slot.shared {
                    for (dst, src) in shared.params.iter().zip(&live.params) {
                        dst.set(src.get());
                    }
                    shared.bypass.store(live.bypass.load(Ordering::Relaxed), Ordering::Relaxed);
                }
                let state = slot.lv2.as_ref().and_then(|l| crate::lv2::save_state(l)).or_else(|| slot.state.clone());
                let created = self.registry.create(&slot.device_id, rate as f64, shared, state.as_deref()).map_err(|e| format!("{}: {e}", slot.name))?;
                devices.push(created.device);
            }
            tracks.push(crate::export::OfflineTrack {
                id: t.id,
                kind: t.kind,
                gain: db_to_gain(t.volume_db),
                pan: t.pan,
                audible: !t.mute && (!any_solo || t.solo),
                devices,
            });
        }
        Ok(crate::export::RenderPlan {
            rate,
            length: end as u64 + (tail_secs.max(0.0) * rate as f32) as u64,
            arrangement: song::compile_song(&self.project, &self.sources, rate),
            tracks,
            master_gain: db_to_gain(self.project.master_db),
        })
    }

    /// Start exporting to `path`.
    pub fn start_export(&mut self, path: PathBuf) {
        let plan = match self.build_export_plan(self.export.tail) {
            Ok(p) => p,
            Err(e) => return self.notify(e),
        };
        match crate::export::ExportJob::start(plan, path, self.export.format) {
            Ok(job) => self.export.job = Some(job),
            Err(e) => self.notify(format!("Export failed: {e}")),
        }
    }

    fn poll_export(&mut self) {
        if !self.export.job.as_ref().is_some_and(|j| j.is_finished()) {
            return;
        }
        let job = self.export.job.take().unwrap();
        let path = job.path.clone();
        match job.join() {
            Ok(stats) => {
                let db = crate::util::gain_to_db(stats.peak);
                let warn = if stats.peak > 1.0 { " — clipped! lower the master volume" } else { "" };
                self.notify(format!("Exported {} ({}, peak {db:.1} dBFS){warn}", path.display(), format_time(stats.frames as i64, self.rate())));
            }
            Err(e) => self.notify(format!("Export: {e}")),
        }
    }

    fn export_window(&mut self, ctx: &egui::Context) {
        self.poll_export();
        if !self.export.open {
            return;
        }
        let mut open = true;
        let mut choose_file = false;
        let rate = self.rate();
        let end = self.song_end();
        let mp3 = crate::export::mp3_available();
        egui::Window::new(format!("{} Export audio", icon::FILE_AUDIO)).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            let running = self.export.job.is_some();
            ui.add_enabled_ui(!running, |ui| {
                theme::section_label(ui, "Format");
                for f in crate::export::Format::ALL {
                    let is_mp3 = matches!(f, crate::export::Format::Mp3 { .. });
                    let r = ui.add_enabled(!is_mp3 || mp3.is_ok(), egui::RadioButton::new(self.export.format == f, f.label()));
                    let r = match (&mp3, is_mp3) {
                        (Err(why), true) => r.on_disabled_hover_text(why),
                        _ => r,
                    };
                    if r.clicked() {
                        self.export.format = f;
                    }
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Tail");
                    ui.add(egui::DragValue::new(&mut self.export.tail).range(0.0..=30.0).speed(0.1).suffix(" s"))
                        .on_hover_text("Extra time after the last clip so reverbs and note releases can ring out");
                });
                ui.label(
                    RichText::new(format!(
                        "Song: {} + {:.1} s tail · {:.1} kHz stereo · rendered offline with all devices",
                        format_time(end, rate),
                        self.export.tail,
                        rate as f32 / 1000.0
                    ))
                    .small()
                    .color(TEXT_DIM),
                );
            });
            ui.add_space(6.0);
            match &self.export.job {
                Some(job) => {
                    ui.add(egui::ProgressBar::new(job.progress.get()).show_percentage());
                    ui.label(RichText::new(job.path.display().to_string()).small().color(TEXT_DIM));
                    if ui.button("Cancel").clicked() {
                        job.cancel();
                    }
                }
                None => {
                    let ok = end > 0;
                    if ui.add_enabled(ok, egui::Button::new(format!("{} Export…", icon::EXPORT))).on_disabled_hover_text("The timeline is empty").clicked() {
                        choose_file = true;
                    }
                }
            }
        });
        if !open && self.export.job.is_none() {
            self.export.open = false;
        }
        if choose_file {
            let ext = self.export.format.extension();
            let stem = self.project_path.as_ref().and_then(|p| p.file_stem()).map_or("song".to_string(), |s| s.to_string_lossy().into_owned());
            let dir = self.project.audio_dir.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let _ = std::fs::create_dir_all(&dir);
            if let Some(path) = rfd::FileDialog::new().add_filter(ext.to_uppercase(), &[ext]).set_directory(&dir).set_file_name(format!("{stem}.{ext}")).save_file() {
                self.start_export(path.with_extension(ext));
            }
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let e = &self.engine;
            if e.is_running() {
                let backend = if e.pipewire { "PipeWire (JACK)" } else { "JACK" };
                let rate = e.sample_rate;
                ui.label(RichText::new(format!("{} {backend}", icon::PLUGS)).color(theme::SOLO));
                ui.label(RichText::new(format!("{:.1} kHz", rate as f32 / 1000.0)).color(TEXT_DIM));
                self.buffer_size_picker(ui);
                let e = &self.engine;
                ui.label(RichText::new(format!("DSP {:.0}% · xruns {}", e.cpu_load(), e.shared.xruns.load(Ordering::Relaxed))).color(TEXT_DIM));
            } else {
                ui.label(RichText::new(format!("{} Audio engine offline", icon::PLUGS)).color(REC))
                    .on_hover_text(e.error.clone().unwrap_or_else(|| "The JACK server shut down".into()));
            }
            ui.separator();
            let lv2 = self.registry.devices.iter().filter(|d| d.id.starts_with("lv2:"));
            let (found, usable) = lv2.fold((0, 0), |(f, u), d| (f + 1, u + d.unsupported.is_none() as usize));
            ui.label(RichText::new(format!("{usable} LV2 plugins usable ({found} found)")).color(TEXT_DIM))
                .on_hover_text(self.registry.lv2_error().unwrap_or("Plugins found via LV2_PATH").to_string());
            if let Some((msg, at)) = &self.status
                && at.elapsed() < Duration::from_secs(8)
            {
                ui.separator();
                ui.label(RichText::new(msg).color(theme::TEXT));
            }
        });
    }

    fn device_panel(&mut self, ui: &mut egui::Ui) {
        let Some(tid) = self.sel.track else {
            ui.centered_and_justified(|ui| ui.label(RichText::new("Select a track to see its device chain").color(TEXT_DIM)));
            return;
        };
        let gui = &self.gui;
        let Some(track) = self.project.track(tid) else { return };
        let kind = track.kind;
        let mut action: Option<(usize, i32)> = None; // (index, -1 left / +1 right / 0 remove)
        egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let n = track.devices.len();
                for (i, slot) in track.devices.iter().enumerate() {
                    let info = self.registry.get(&slot.device_id);
                    let Some(shared) = &slot.shared else { continue };
                    egui::Frame::new().fill(theme::BG).stroke(egui::Stroke::new(1.0, theme::BORDER)).corner_radius(8).inner_margin(8).show(ui, |ui| {
                        ui.set_width(250.0);
                        ui.set_min_height(ui.available_height());
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.horizontal(|ui| {
                            let mut on = !shared.bypass.load(Ordering::Relaxed);
                            let pw = RichText::new(icon::POWER).color(if on { ACCENT } else { TEXT_DIM });
                            if ui.add(egui::Button::new(pw).frame(false)).on_hover_text("Enable / bypass").clicked() {
                                on = !on;
                                shared.bypass.store(!on, Ordering::Relaxed);
                            }
                            let cat = info.map(|d| d.category);
                            let badge = if cat == Some(DeviceCategory::Instrument) { icon::WAVEFORM } else { icon::SLIDERS_HORIZONTAL };
                            ui.label(RichText::new(format!("{badge} {}", slot.name)).strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if theme::icon_button(ui, icon::X, "Remove device").clicked() {
                                    action = Some((i, 0));
                                }
                                if i + 1 < n && theme::icon_button(ui, icon::ARROW_RIGHT, "Move right").clicked() {
                                    action = Some((i, 1));
                                }
                                if i > 0 && theme::icon_button(ui, icon::ARROW_LEFT, "Move left").clicked() {
                                    action = Some((i, -1));
                                }
                            });
                        });
                        ui.separator();
                        let Some(info) = info else { return };
                        if info.has_gui {
                            let open = gui.is_open(shared);
                            let label = if open { format!("{} GUI open", icon::APP_WINDOW) } else { format!("{} Open GUI", icon::APP_WINDOW) };
                            let b = egui::Button::new(RichText::new(label).color(if open { ACCENT } else { theme::TEXT })).min_size(egui::vec2(234.0, 26.0));
                            if ui.add_enabled(!open, b).on_hover_text("Open the plugin's own interface in a separate window").clicked() {
                                action = Some((i, 2));
                            }
                        }
                        egui::ScrollArea::vertical().id_salt(("dev", tid, i)).max_height(ui.available_height().max(120.0)).show(ui, |ui| {
                            for (pi, p) in info.params.iter().enumerate() {
                                param_widget(ui, p, &shared.params[pi]);
                            }
                            if !info.outputs.is_empty() {
                                theme::section_label(ui, "Outputs");
                                for (pi, p) in info.outputs.iter().enumerate() {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(&p.name).small().color(TEXT_DIM));
                                        ui.label(RichText::new(format!("{:.2}", shared.outputs[pi].get())).small().monospace());
                                    });
                                }
                            }
                            if info.params.is_empty() && info.outputs.is_empty() {
                                ui.label(RichText::new("No parameters").small().color(TEXT_DIM));
                            }
                        });
                        });
                    });
                }
                let label = if kind == TrackKind::Instrument && n == 0 { "Add instrument" } else { "Add device" };
                let add = egui::Button::new(RichText::new(format!("{} {label}", icon::PLUS)).size(14.0)).min_size(egui::vec2(140.0, 60.0));
                if ui.add(add).clicked() {
                    let category = if kind == TrackKind::Instrument && n == 0 { Some(DeviceCategory::Instrument) } else { None };
                    self.browser = Some(Browser { track: tid, filter: String::new(), category });
                }
            });
        });
        match action {
            Some((i, 0)) => self.remove_device(tid, i),
            Some((i, 2)) => self.open_plugin_gui(tid, i),
            Some((i, d)) => self.move_device(tid, i, (i as i32 + d) as usize),
            None => {}
        }
    }

    fn browser_window(&mut self, ctx: &egui::Context) {
        let Some(b) = &mut self.browser else { return };
        let mut open = true;
        let mut chosen = None;
        egui::Window::new(format!("{} Device browser", icon::PLUGS))
            .open(&mut open)
            .default_size([460.0, 520.0])
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(icon::MAGNIFYING_GLASS);
                    ui.add(egui::TextEdit::singleline(&mut b.filter).hint_text("Search devices…").desired_width(f32::INFINITY)).request_focus();
                });
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut b.category, None, "All");
                    ui.selectable_value(&mut b.category, Some(DeviceCategory::Instrument), "Instruments");
                    ui.selectable_value(&mut b.category, Some(DeviceCategory::Effect), "Effects");
                });
                ui.separator();
                let filter = b.filter.to_lowercase();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for d in &self.registry.devices {
                        if b.category.is_some_and(|c| c != d.category) {
                            continue;
                        }
                        if !filter.is_empty() && !d.name.to_lowercase().contains(&filter) && !d.class.to_lowercase().contains(&filter) {
                            continue;
                        }
                        let kind = if d.id.starts_with("lv2:") { "LV2" } else { "Built-in" };
                        let text = RichText::new(format!("{}   ", d.name)).color(if d.unsupported.is_some() { TEXT_DIM } else { theme::TEXT });
                        ui.horizontal(|ui| {
                            let r = ui.add_enabled(d.unsupported.is_none(), egui::Button::new(text).frame(false));
                            ui.label(RichText::new(format!("{kind} · {}", d.class)).small().color(TEXT_DIM));
                            let r = match &d.unsupported {
                                Some(why) => r.on_disabled_hover_text(why),
                                None => r.on_hover_text(&d.id),
                            };
                            if r.clicked() {
                                chosen = Some(d.id.clone());
                            }
                        });
                    }
                });
            });
        let track = b.track;
        if let Some(id) = chosen {
            self.add_device(track, &id);
            self.browser = None;
        } else if !open {
            self.browser = None;
        }
    }

    fn clip_inspector(&mut self, ui: &mut egui::Ui, clip: Id) {
        let rate = self.rate();
        let Some(c) = self.project.clip_mut(clip) else { return };
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", icon::WAVEFORM, c.name)).strong());
        });
        egui::Grid::new("clip_inspector").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut c.name);
            ui.end_row();
            ui.label("Gain");
            let mut db = crate::util::gain_to_db(c.gain);
            if ui.add(egui::Slider::new(&mut db, -40.0..=12.0).suffix(" dB")).changed() {
                c.gain = db_to_gain(db);
                changed = true;
            }
            ui.end_row();
            let ms = |s: u64| s as f32 / rate as f32 * 1000.0;
            let max_fade = c.length as f32 / rate as f32 * 1000.0;
            for (label, v) in [("Fade in", &mut c.fade_in), ("Fade out", &mut c.fade_out)] {
                ui.label(label);
                let mut m = ms(*v);
                if ui.add(egui::Slider::new(&mut m, 0.0..=max_fade.min(10000.0)).suffix(" ms")).changed() {
                    *v = (m / 1000.0 * rate as f32) as u64;
                    changed = true;
                }
                ui.end_row();
            }
            ui.label("Muted");
            changed |= ui.checkbox(&mut c.muted, "").changed();
            ui.end_row();
            ui.label("Position");
            ui.label(format_time(c.start, rate));
            ui.end_row();
            ui.label("Length");
            ui.label(format_time(c.length as i64, rate));
            ui.end_row();
        });
        if changed {
            self.arrangement_dirty = true;
        }
    }

    fn bottom_tabs(&mut self, ui: &mut egui::Ui) {
        ui.selectable_value(&mut self.bottom, BottomTab::Takes, format!("{} Takes", icon::STACK));
        ui.selectable_value(&mut self.bottom, BottomTab::Editor, format!("{} Editor", icon::PIANO_KEYS));
        ui.selectable_value(&mut self.bottom, BottomTab::Devices, format!("{} Devices", icon::FADERS));
        if let Some(t) = self.sel.track.and_then(|id| self.project.track(id)) {
            ui.separator();
            ui.label(RichText::new(&t.name).color(theme::rgb(t.color)).strong());
        }
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui) {
        match self.bottom {
            BottomTab::Takes => match self.sel.snippet.filter(|id| self.project.snippet(*id).is_some()) {
                Some(id) => self.snippet_detail(ui, id),
                None => {
                    ui.centered_and_justified(|ui| ui.label(RichText::new("Select a snippet to see its takes").color(TEXT_DIM)));
                }
            },
            BottomTab::Devices => self.device_panel(ui),
            BottomTab::Editor => {
                // A MIDI take opened from the Takes tab wins over the timeline selection.
                if let Some(take) = self.sel.take.filter(|t| self.project.find_take(*t).is_some()) {
                    self.piano_roll(ui, take);
                    return;
                }
                let clip = self.sel.clip.and_then(|c| self.project.find_clip(c).map(|(t, i)| &self.project.tracks[t].clips[i]));
                match clip.map(|c| (c.id, c.content.clone())) {
                    Some((id, ClipContent::Midi { .. })) => self.piano_roll(ui, id),
                    Some((id, ClipContent::Audio { .. })) => self.clip_inspector(ui, id),
                    Some((_, ClipContent::Snippet { snippet })) => {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} Linked placement — edit the snippet itself", icon::LINK)).color(TEXT_DIM));
                            if ui.button("Open snippet").clicked() {
                                self.sel.snippet = Some(snippet);
                                self.bottom = BottomTab::Takes;
                            }
                        });
                    }
                    None => {
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new("Select a clip or a MIDI take to edit it").color(TEXT_DIM))
                        });
                    }
                }
            }
        }
    }

    /// Header strip of a foldable panel. Returns true if folded.
    pub fn fold_header(ui: &mut egui::Ui, folded: &mut bool, title: &str, extra: impl FnOnce(&mut egui::Ui)) -> bool {
        ui.horizontal(|ui| {
            let caret = if *folded { icon::CARET_RIGHT } else { icon::CARET_DOWN };
            let tip = if *folded { "Expand panel" } else { "Fold panel" };
            if ui.add(egui::Button::new(RichText::new(format!("{caret} {title}")).strong()).frame(false)).on_hover_text(tip).clicked() {
                *folded = !*folded;
            }
            extra(ui);
        });
        *folded
    }
}

/// Generic parameter editor used for every device.
fn param_widget(ui: &mut egui::Ui, p: &crate::device::ParamInfo, value: &crate::util::AtomicF32) {
    let mut v = value.get();
    ui.label(RichText::new(&p.name).small().color(TEXT_DIM));
    let changed = if p.toggled {
        let mut on = v > 0.5;
        let r = ui.checkbox(&mut on, "").changed();
        v = if on { 1.0 } else { 0.0 };
        r
    } else if !p.scale_points.is_empty() {
        let current = p.scale_points.iter().find(|(_, sv)| (*sv - v).abs() < 1e-3).map(|(l, _)| l.clone()).unwrap_or_else(|| format!("{v}"));
        let mut changed = false;
        egui::ComboBox::from_id_salt(ui.next_auto_id()).selected_text(current).width(220.0).show_ui(ui, |ui| {
            for (label, sv) in &p.scale_points {
                if ui.selectable_label((*sv - v).abs() < 1e-3, label).clicked() {
                    v = *sv;
                    changed = true;
                }
            }
        });
        changed
    } else {
        let mut s = egui::Slider::new(&mut v, p.min..=p.max).logarithmic(p.logarithmic);
        if p.integer {
            s = s.integer();
        }
        let r = ui.add(s);
        if r.double_clicked() {
            v = p.default;
        }
        r.changed() || r.double_clicked()
    };
    if changed {
        value.set(v);
    }
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;

        self.engine.collect_garbage();
        for msg in self.gui.poll() {
            self.notify(msg);
        }
        self.poll_recording();
        self.sync_shared(dt);
        if self.ports_refreshed.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            self.refresh_ports();
        }
        self.shortcuts(&ctx);

        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            let hover = ctx.input(|i| i.pointer.hover_pos());
            let target = hover.and_then(|p| self.lane_rects.iter().find(|(_, r)| r.contains(p)).map(|(id, _)| *id));
            self.import_files(dropped, target);
        }

        egui::Panel::top("top").frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(8, 4))).show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(10, 3))).show(ui, |ui| self.status_bar(ui));
        // Detail panel (takes / editor / devices).
        let frame = egui::Frame::new().fill(theme::PANEL).inner_margin(8);
        if self.folds.detail {
            egui::Panel::bottom("detail_folded").frame(frame).show(ui, |ui| {
                let mut f = self.folds.detail;
                Self::fold_header(ui, &mut f, "Detail", |_| {});
                self.folds.detail = f;
            });
        } else {
            egui::Panel::bottom("detail").resizable(true).default_size(280.0).frame(frame).show(ui, |ui| {
                let mut f = self.folds.detail;
                Self::fold_header(ui, &mut f, "Detail", |ui| {
                    ui.separator();
                    self.bottom_tabs(ui);
                });
                self.folds.detail = f;
                ui.separator();
                self.bottom_panel(ui);
            });
        }

        // Snippet columns and the linear timeline. Whichever is unfolded fills
        // the middle; folded ones shrink to their header.
        let snippet_frame = egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(8, 6));
        let timeline_frame = egui::Frame::new().fill(theme::LANE);
        if !self.folds.timeline {
            if self.folds.snippets {
                egui::Panel::top("snippets_folded").frame(snippet_frame).show(ui, |ui| self.snippet_panel(ui));
            } else {
                egui::Panel::top("snippets").resizable(true).default_size(250.0).frame(snippet_frame).show(ui, |ui| self.snippet_panel(ui));
            }
            egui::CentralPanel::default().frame(timeline_frame).show(ui, |ui| self.timeline_panel(ui));
        } else {
            egui::Panel::bottom("timeline_folded").frame(timeline_frame).show(ui, |ui| self.timeline_panel(ui));
            egui::CentralPanel::default().frame(snippet_frame).show(ui, |ui| self.snippet_panel(ui));
        }

        self.browser_window(&ctx);
        self.export_window(&ctx);

        if self.arrangement_dirty {
            self.arrangement_dirty = false;
            self.rebuild_arrangement();
        }
        // Meters and the playhead need continuous updates.
        // Meters, the playhead and plugin GUIs need continuous updates.
        let busy = self.playing() || self.gui.has_pending();
        ctx.request_repaint_after(Duration::from_millis(if busy { 16 } else { 40 }));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.gui.close_all();
        // Finish recordings so WAV headers get written.
        if self.recording() {
            self.engine.send(Command::Stop);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.takes.is_empty() && Instant::now() < deadline {
            self.engine.collect_garbage();
            self.takes.retain(|t| !t.rec.is_finished());
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Shorten a long device name for compact display.
fn short_name(s: &str, max: usize) -> String {
    if s.chars().count() > max { format!("{}…", s.chars().take(max - 1).collect::<String>()) } else { s.to_string() }
}
