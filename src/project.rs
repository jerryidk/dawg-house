//! Session data model.
//!
//! Each track owns **snippets**: short musical pieces with their own tempo,
//! meter and length in bars. A snippet collects **takes** (one is active).
//! The song is assembled on the linear timeline from **clips**; a clip either
//! holds content directly (imported audio, free MIDI) or is a *linked
//! placement* of a snippet, so fixing a snippet updates every placement.
//! Editing is non-destructive: clips and takes are windows onto content.

use crate::engine::TrackShared;
use crate::device::DeviceShared;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub type Id = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    /// Records and plays audio; inputs are JACK audio ports.
    Audio,
    /// Records and plays MIDI into an instrument device; input is a JACK MIDI port.
    Instrument,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub pitch: u8,
    pub velocity: u8,
    /// Start relative to the clip's content origin, in beats.
    pub start: f64,
    /// Length in beats.
    pub length: f64,
}

impl Note {
    pub fn end(&self) -> f64 {
        self.start + self.length
    }
}

/// What a clip plays. New content kinds (automation, video, ...) slot in here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClipContent {
    Audio { source: Id },
    Midi { notes: Vec<Note> },
    /// Linked placement of a snippet on the same track. The snippet's active
    /// take loops to fill the clip.
    Snippet { snippet: Id },
}

/// One recording of a snippet. Its length is always the snippet's length.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Take {
    pub id: Id,
    pub name: String,
    /// `Audio` or `Midi` (never `Snippet`).
    pub content: ClipContent,
    /// First source frame used (audio takes cut from a loop recording).
    pub offset: u64,
    #[serde(default = "unity")]
    pub gain: f32,
    /// Fades at the take's own start/end (applied on every loop pass).
    #[serde(default)]
    pub fade_in: u64,
    #[serde(default)]
    pub fade_out: u64,
}

fn unity() -> f32 {
    1.0
}

impl Take {
    pub fn new(id: Id, name: String, content: ClipContent, offset: u64) -> Self {
        Take { id, name, content, offset, gain: 1.0, fade_in: 0, fade_out: 0 }
    }
}

/// Mutable view of the fields every region has.
pub struct RegionFields<'a> {
    pub name: &'a mut String,
    pub content: &'a mut ClipContent,
    pub offset: &'a mut u64,
    pub gain: &'a mut f32,
    pub fade_in: &'a mut u64,
    pub fade_out: &'a mut u64,
}

/// What timeline clips and snippet takes have in common: a window onto audio
/// or MIDI content with its own offset, gain and fades. Editors work on
/// `dyn Region`, so clips and takes get the same tools (Rust's shared-interface
/// take on inheritance).
pub trait Region {
    fn fields(&mut self) -> RegionFields<'_>;
}

impl Region for Clip {
    fn fields(&mut self) -> RegionFields<'_> {
        RegionFields {
            name: &mut self.name,
            content: &mut self.content,
            offset: &mut self.offset,
            gain: &mut self.gain,
            fade_in: &mut self.fade_in,
            fade_out: &mut self.fade_out,
        }
    }
}

impl Region for Take {
    fn fields(&mut self) -> RegionFields<'_> {
        RegionFields {
            name: &mut self.name,
            content: &mut self.content,
            offset: &mut self.offset,
            gain: &mut self.gain,
            fade_in: &mut self.fade_in,
            fade_out: &mut self.fade_out,
        }
    }
}

/// Where a region lives and the musical grid it is edited against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegionContext {
    /// Length in samples (a take is as long as its snippet).
    pub length: u64,
    pub samples_per_beat: f64,
    pub beats_per_bar: u32,
    pub owner: RegionOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionOwner {
    /// A clip on the timeline (can be trimmed).
    Clip { track: Id, start: i64 },
    /// A take of a snippet (length fixed by the snippet).
    Take { track: Id, snippet: Id },
}

/// A short piece of one track's part, recorded against its own tempo/meter.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snippet {
    pub id: Id,
    pub name: String,
    /// Beats per minute, where a beat is one `beat_unit` note.
    pub bpm: f32,
    pub beats_per_bar: u32,
    pub beat_unit: u32,
    pub bars: u32,
    pub takes: Vec<Take>,
    pub active: Option<Id>,
}

impl Snippet {
    pub fn samples_per_beat(&self, rate: u32) -> f64 {
        rate as f64 * 60.0 / self.bpm.max(1.0) as f64
    }

    /// Loop length in samples.
    pub fn length(&self, rate: u32) -> u64 {
        (self.samples_per_beat(rate) * (self.bars.max(1) * self.beats_per_bar.max(1)) as f64).round() as u64
    }

    pub fn active_take(&self) -> Option<&Take> {
        self.active.and_then(|id| self.takes.iter().find(|t| t.id == id))
    }

    pub fn meter(&self) -> String {
        format!("{}/{}", self.beats_per_bar, self.beat_unit)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Clip {
    pub id: Id,
    pub content: ClipContent,
    pub name: String,
    /// Timeline position of the first frame, in samples.
    pub start: i64,
    /// Offset into the content (source frames for audio, content time in
    /// samples for MIDI). Trimming the left edge increases it.
    pub offset: u64,
    pub length: u64,
    pub gain: f32,
    pub fade_in: u64,
    pub fade_out: u64,
    #[serde(default)]
    pub muted: bool,
}

impl Clip {
    pub fn end(&self) -> i64 {
        self.start + self.length as i64
    }
}

/// A device in a track's chain. `device_id` identifies the device type in
/// the [`crate::device::DeviceRegistry`] (`lv2:<uri>` or `builtin:<name>`).
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceSlot {
    pub device_id: String,
    pub name: String,
    /// Saved parameter values by port symbol (refreshed before saving).
    pub params: Vec<(String, f32)>,
    pub bypass: bool,
    #[serde(skip)]
    pub shared: Option<Arc<DeviceShared>>,
    /// Plugin-internal state (e.g. a loaded amp model), refreshed before saving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Vec<crate::lv2::StateEntry>>,
    /// Link to the running LV2 instance (GUI hosting, state saving).
    #[serde(skip)]
    pub lv2: Option<Arc<crate::lv2::Lv2Link>>,
}

#[derive(Serialize, Deserialize)]
pub struct Track {
    pub id: Id,
    pub kind: TrackKind,
    pub name: String,
    pub color: [u8; 3],
    pub volume_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub armed: bool,
    pub monitor: bool,
    pub stereo: bool,
    /// Audio: left/mono source port. Instrument: MIDI source port.
    pub input_l: Option<String>,
    pub input_r: Option<String>,
    pub clips: Vec<Clip>,
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    /// Snippet that plays as this track's backing while snippets loop.
    #[serde(default)]
    pub cued: Option<Id>,
    pub devices: Vec<DeviceSlot>,
    #[serde(skip)]
    pub shared: Arc<TrackShared>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SourceRef {
    pub id: Id,
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub tracks: Vec<Track>,
    pub sources: Vec<SourceRef>,
    pub next_id: Id,
    pub bpm: f32,
    pub snap: bool,
    pub master_db: f32,
    /// Directory where new recordings are written.
    pub audio_dir: PathBuf,
    /// Playback device (JACK client name) the master output goes to.
    #[serde(default)]
    pub master_output: Option<String>,
}

pub const TRACK_COLORS: [[u8; 3]; 8] = [
    [94, 156, 255],
    [255, 138, 101],
    [102, 214, 160],
    [196, 132, 252],
    [250, 204, 88],
    [86, 206, 220],
    [244, 114, 182],
    [163, 190, 110],
];

impl Default for Project {
    fn default() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Project {
            version: 1,
            tracks: vec![],
            sources: vec![],
            next_id: 1,
            bpm: 120.0,
            snap: true,
            master_db: 0.0,
            audio_dir: PathBuf::from(home).join("Music/Studio").join(format!("session-{stamp}")).join("audio"),
            master_output: None,
        }
    }
}

impl Project {
    pub fn alloc_id(&mut self) -> Id {
        self.next_id += 1;
        self.next_id - 1
    }

    pub fn new_track(&mut self, kind: TrackKind) -> &mut Track {
        let id = self.alloc_id();
        let n = self.tracks.len();
        let label = match kind {
            TrackKind::Audio => "Audio",
            TrackKind::Instrument => "Inst",
        };
        self.tracks.push(Track {
            id,
            kind,
            name: format!("{label} {}", n + 1),
            color: TRACK_COLORS[n % TRACK_COLORS.len()],
            volume_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            armed: false,
            monitor: false,
            stereo: false,
            input_l: None,
            input_r: None,
            clips: vec![],
            snippets: vec![],
            cued: None,
            devices: vec![],
            shared: Default::default(),
        });
        self.tracks.last_mut().unwrap()
    }

    pub fn track(&self, id: Id) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn track_mut(&mut self, id: Id) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Locate a clip: (track index, clip index).
    pub fn find_clip(&self, clip: Id) -> Option<(usize, usize)> {
        self.tracks
            .iter()
            .enumerate()
            .find_map(|(ti, t)| t.clips.iter().position(|c| c.id == clip).map(|ci| (ti, ci)))
    }

    pub fn clip_mut(&mut self, clip: Id) -> Option<&mut Clip> {
        let (t, c) = self.find_clip(clip)?;
        Some(&mut self.tracks[t].clips[c])
    }

    /// Split `clip` at timeline position `at`. Returns the id of the new
    /// right-hand clip.
    pub fn split_clip(&mut self, clip: Id, at: i64) -> Option<Id> {
        let (ti, ci) = self.find_clip(clip)?;
        let c = &self.tracks[ti].clips[ci];
        if at <= c.start || at >= c.end() {
            return None;
        }
        let new_id = self.alloc_id();
        let c = &mut self.tracks[ti].clips[ci];
        let left_len = (at - c.start) as u64;
        let mut right = c.clone();
        right.id = new_id;
        right.start = at;
        right.offset += left_len;
        right.length -= left_len;
        right.fade_in = 0;
        c.length = left_len;
        c.fade_out = 0;
        self.tracks[ti].clips.insert(ci + 1, right);
        Some(new_id)
    }

    pub fn delete_clip(&mut self, clip: Id) {
        if let Some((t, c)) = self.find_clip(clip) {
            self.tracks[t].clips.remove(c);
        }
    }

    /// Copy a clip and place the copy right after the original.
    pub fn duplicate_clip(&mut self, clip: Id) -> Option<Id> {
        let (ti, ci) = self.find_clip(clip)?;
        let id = self.alloc_id();
        let mut c = self.tracks[ti].clips[ci].clone();
        c.id = id;
        c.start = c.end();
        self.tracks[ti].clips.push(c);
        Some(id)
    }

    pub fn samples_per_beat(&self, rate: u32) -> f64 {
        rate as f64 * 60.0 / self.bpm.max(1.0) as f64
    }

    // -----------------------------------------------------------------------
    // Snippets

    /// Locate a snippet: (track index, snippet index).
    pub fn find_snippet(&self, snippet: Id) -> Option<(usize, usize)> {
        self.tracks
            .iter()
            .enumerate()
            .find_map(|(ti, t)| t.snippets.iter().position(|s| s.id == snippet).map(|si| (ti, si)))
    }

    pub fn snippet(&self, snippet: Id) -> Option<&Snippet> {
        let (t, s) = self.find_snippet(snippet)?;
        Some(&self.tracks[t].snippets[s])
    }

    pub fn snippet_mut(&mut self, snippet: Id) -> Option<&mut Snippet> {
        let (t, s) = self.find_snippet(snippet)?;
        Some(&mut self.tracks[t].snippets[s])
    }

    /// Track that owns a snippet.
    pub fn snippet_track(&self, snippet: Id) -> Option<Id> {
        self.find_snippet(snippet).map(|(t, _)| self.tracks[t].id)
    }

    /// Add an empty snippet to a track using the song tempo and 4/4.
    pub fn new_snippet(&mut self, track: Id) -> Option<Id> {
        let id = self.alloc_id();
        let bpm = self.bpm;
        let t = self.track_mut(track)?;
        let name = format!("Snippet {}", t.snippets.len() + 1);
        t.snippets.push(Snippet { id, name, bpm, beats_per_bar: 4, beat_unit: 4, bars: 4, takes: vec![], active: None });
        t.cued = Some(id);
        Some(id)
    }

    /// Delete a snippet and every placement of it.
    pub fn delete_snippet(&mut self, snippet: Id) {
        for t in &mut self.tracks {
            t.snippets.retain(|s| s.id != snippet);
            t.clips.retain(|c| !matches!(c.content, ClipContent::Snippet { snippet: s } if s == snippet));
            if t.cued == Some(snippet) {
                t.cued = t.snippets.last().map(|s| s.id);
            }
        }
    }

    /// Place a snippet on its track's timeline lane. The placement covers one
    /// loop of the snippet.
    pub fn place_snippet(&mut self, snippet: Id, start: i64, rate: u32) -> Option<Id> {
        let (ti, si) = self.find_snippet(snippet)?;
        let id = self.alloc_id();
        let s = &self.tracks[ti].snippets[si];
        let clip = Clip {
            id,
            content: ClipContent::Snippet { snippet },
            name: s.name.clone(),
            start: start.max(0),
            offset: 0,
            length: s.length(rate),
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
            muted: false,
        };
        self.tracks[ti].clips.push(clip);
        Some(id)
    }

    /// Give a placement its own copy of the snippet so it can differ from the
    /// other placements. Returns the new snippet id.
    pub fn make_unique(&mut self, clip: Id) -> Option<Id> {
        let (ti, ci) = self.find_clip(clip)?;
        let ClipContent::Snippet { snippet } = self.tracks[ti].clips[ci].content else { return None };
        let mut copy = self.snippet(snippet)?.clone();
        copy.id = self.alloc_id();
        copy.name = format!("{} (copy)", copy.name);
        let mut active = None;
        for take in &mut copy.takes {
            let new_id = self.alloc_id();
            if copy.active == Some(take.id) {
                active = Some(new_id);
            }
            take.id = new_id;
        }
        copy.active = active;
        let new_id = copy.id;
        let t = &mut self.tracks[ti];
        let si = t.snippets.iter().position(|s| s.id == snippet).map_or(t.snippets.len(), |i| i + 1);
        t.snippets.insert(si, copy);
        t.clips[ci].content = ClipContent::Snippet { snippet: new_id };
        t.clips[ci].name = t.snippets[si].name.clone();
        Some(new_id)
    }

    /// Locate a take: (track index, snippet index, take index).
    pub fn find_take(&self, take: Id) -> Option<(usize, usize, usize)> {
        self.tracks.iter().enumerate().find_map(|(ti, t)| {
            t.snippets
                .iter()
                .enumerate()
                .find_map(|(si, s)| s.takes.iter().position(|k| k.id == take).map(|ki| (ti, si, ki)))
        })
    }

    pub fn delete_take(&mut self, take: Id) {
        if let Some((ti, si, ki)) = self.find_take(take) {
            let s = &mut self.tracks[ti].snippets[si];
            s.takes.remove(ki);
            if s.active == Some(take) {
                s.active = s.takes.last().map(|k| k.id);
            }
        }
    }

    /// A clip or take by id (ids are unique project-wide), with its context.
    pub fn region_mut(&mut self, id: Id, rate: u32) -> Option<(RegionContext, &mut dyn Region)> {
        if let Some((t, c)) = self.find_clip(id) {
            let ctx = RegionContext {
                length: self.tracks[t].clips[c].length,
                samples_per_beat: self.samples_per_beat(rate),
                beats_per_bar: 4,
                owner: RegionOwner::Clip { track: self.tracks[t].id, start: self.tracks[t].clips[c].start },
            };
            return Some((ctx, &mut self.tracks[t].clips[c]));
        }
        let (t, s, k) = self.find_take(id)?;
        let sn = &self.tracks[t].snippets[s];
        let ctx = RegionContext {
            length: sn.length(rate),
            samples_per_beat: sn.samples_per_beat(rate),
            beats_per_bar: sn.beats_per_bar,
            owner: RegionOwner::Take { track: self.tracks[t].id, snippet: sn.id },
        };
        Some((ctx, &mut self.tracks[t].snippets[s].takes[k]))
    }

    /// Notes of a free MIDI clip or of a MIDI take.
    pub fn notes_mut(&mut self, id: Id) -> Option<&mut Vec<Note>> {
        let (_, region) = self.region_mut(id, 48000)?;
        match region.fields().content {
            ClipContent::Midi { notes } => Some(notes),
            _ => None,
        }
    }

    /// Audio sources still referenced by clips or takes.
    pub fn used_sources(&self) -> std::collections::HashSet<Id> {
        let clips = self.tracks.iter().flat_map(|t| t.clips.iter().map(|c| &c.content));
        let takes = self.tracks.iter().flat_map(|t| t.snippets.iter().flat_map(|s| s.takes.iter().map(|k| &k.content)));
        clips.chain(takes).filter_map(|c| if let ClipContent::Audio { source } = c { Some(*source) } else { None }).collect()
    }
}

// ---------------------------------------------------------------------------
// Undo / redo of clip and snippet edits

pub type Snapshot = Vec<(Id, Vec<Clip>, Vec<Snippet>, Option<Id>)>;

#[derive(Default)]
pub struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

fn snapshot(p: &Project) -> Snapshot {
    p.tracks.iter().map(|t| (t.id, t.clips.clone(), t.snippets.clone(), t.cued)).collect()
}

fn restore(p: &mut Project, s: Snapshot) {
    for (id, clips, snippets, cued) in s {
        if let Some(t) = p.track_mut(id) {
            t.clips = clips;
            t.snippets = snippets;
            t.cued = cued;
        }
    }
}

impl History {
    /// Record the current state before an edit.
    pub fn checkpoint(&mut self, p: &Project) {
        self.undo.push(snapshot(p));
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self, p: &mut Project) -> bool {
        let Some(s) = self.undo.pop() else { return false };
        self.redo.push(snapshot(p));
        restore(p, s);
        true
    }

    pub fn redo(&mut self, p: &mut Project) -> bool {
        let Some(s) = self.redo.pop() else { return false };
        self.undo.push(snapshot(p));
        restore(p, s);
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}
