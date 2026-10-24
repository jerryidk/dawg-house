//! Song logic, independent of the UI: compiling the project into the engine's
//! playback list (resolving linked snippet placements and looping takes) and
//! turning loop recordings into snippet takes.

use crate::audio::AudioData;
use crate::engine::{Arrangement, PlayClip, PlayContent};
use crate::project::{ClipContent, Id, Note, Project, Snippet};
use std::collections::HashMap;
use std::sync::Arc;

pub type Sources = HashMap<Id, Arc<AudioData>>;

/// What the transport plays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayScope {
    /// The assembled song on the timeline.
    Song,
    /// One snippet looping, with the other tracks' cued snippets as backing.
    Snippet(Id),
}

/// Sorted (frame, message) pairs for the notes inside the content window
/// `offset..offset + length` (samples), relative to the window start.
pub fn midi_events(notes: &[Note], offset: u64, length: u64, spb: f64) -> Vec<(u64, [u8; 3])> {
    let (off, len) = (offset as f64, length as f64);
    let mut ev = vec![];
    for n in notes {
        let on = n.start * spb - off;
        let end = (n.end() * spb - off).min(len - 1.0);
        if end <= 0.0 || on >= len || end <= on.max(0.0) {
            continue;
        }
        ev.push((on.max(0.0) as u64, [0x90, n.pitch, n.velocity.max(1)]));
        ev.push((end as u64, [0x80, n.pitch, 0]));
    }
    // Note-offs before note-ons on the same frame so repeated notes retrigger.
    ev.sort_by_key(|(f, m)| (*f, m[0] == 0x90));
    ev
}

/// Playback clips for the window `offset..offset + length` of a snippet whose
/// active take loops forever, placed at timeline position `start`.
#[allow(clippy::too_many_arguments)]
pub fn snippet_window(
    s: &Snippet,
    sources: &Sources,
    rate: u32,
    start: i64,
    offset: u64,
    length: u64,
    gain: f32,
    fades: (u64, u64),
) -> Vec<PlayClip> {
    let Some(take) = s.active_take() else { return vec![] };
    let period = s.length(rate).max(1);
    let spb = s.samples_per_beat(rate);
    let (w0, w1) = (offset, offset + length);
    let mut out = vec![];
    let mut k = w0 / period;
    while k * period < w1 {
        let (a, b) = (w0.max(k * period), w1.min((k + 1) * period));
        let inner = a - k * period;
        let content = match &take.content {
            ClipContent::Audio { source } => sources.get(source).map(|d| PlayContent::Audio { data: d.clone(), offset: (take.offset + inner) as usize }),
            ClipContent::Midi { notes } => Some(PlayContent::Midi { events: Arc::new(midi_events(notes, inner, b - a, spb)) }),
            ClipContent::Snippet { .. } => None,
        };
        if let Some(content) = content
            && b > a
        {
            // The take's own fades apply at each pass boundary; the
            // placement's fades win at the edges of the placement.
            let pass_start = a == k * period;
            let pass_end = b == (k + 1) * period;
            let fade_in = if a == w0 && fades.0 > 0 { fades.0 } else if pass_start { take.fade_in } else { 0 };
            let fade_out = if b == w1 && fades.1 > 0 { fades.1 } else if pass_end { take.fade_out } else { 0 };
            out.push(PlayClip {
                start: start + (a - w0) as i64,
                length: (b - a) as usize,
                gain: gain * take.gain,
                fade_in: fade_in as usize,
                fade_out: fade_out as usize,
                content,
            });
        }
        k += 1;
    }
    out
}

/// The assembled song: every unmuted clip on the timeline.
pub fn compile_song(project: &Project, sources: &Sources, rate: u32) -> Arrangement {
    let spb = project.samples_per_beat(rate);
    let mut arr = Arrangement::default();
    for t in &project.tracks {
        let mut clips = vec![];
        for c in t.clips.iter().filter(|c| !c.muted) {
            let single = |content| PlayClip {
                start: c.start,
                length: c.length as usize,
                gain: c.gain,
                fade_in: c.fade_in as usize,
                fade_out: c.fade_out as usize,
                content,
            };
            match &c.content {
                ClipContent::Audio { source } => {
                    if let Some(data) = sources.get(source) {
                        clips.push(single(PlayContent::Audio { data: data.clone(), offset: c.offset as usize }));
                    }
                }
                ClipContent::Midi { notes } => {
                    clips.push(single(PlayContent::Midi { events: Arc::new(midi_events(notes, c.offset, c.length, spb)) }));
                }
                ClipContent::Snippet { snippet } => {
                    if let Some(s) = t.snippets.iter().find(|s| s.id == *snippet) {
                        clips.extend(snippet_window(s, sources, rate, c.start, c.offset, c.length, c.gain, (c.fade_in, c.fade_out)));
                    }
                }
            }
        }
        arr.tracks.push((t.id, clips));
    }
    arr
}

/// Loop playback for snippet work. `focus` loops over its own length on its
/// track; every other track plays its cued snippet from the same start. With
/// `mute_focus_track` the focus track is silent (recording a new take).
/// Returns the arrangement and the loop length.
pub fn compile_snippet_loop(project: &Project, sources: &Sources, rate: u32, focus: Id, mute_focus_track: bool) -> (Arrangement, u64) {
    let Some(focus_snippet) = project.snippet(focus) else { return (Arrangement::default(), 0) };
    let loop_len = focus_snippet.length(rate);
    let focus_track = project.snippet_track(focus);
    let mut arr = Arrangement::default();
    for t in &project.tracks {
        let own = Some(t.id) == focus_track;
        let pick = if own { Some(focus) } else { t.cued };
        let snippet = pick.and_then(|id| t.snippets.iter().find(|s| s.id == id));
        let clips = match snippet {
            Some(s) if !(own && mute_focus_track) => snippet_window(s, sources, rate, 0, 0, loop_len, 1.0, (0, 0)),
            _ => vec![],
        };
        arr.tracks.push((t.id, clips));
    }
    (arr, loop_len)
}

/// Offsets of the takes in a continuous loop recording of `total` frames.
/// Incomplete trailing passes are dropped unless nothing complete was recorded.
pub fn audio_passes(total: u64, period: u64) -> Vec<u64> {
    let period = period.max(1);
    match total / period {
        0 if total > 0 => vec![0],
        0 => vec![],
        full => (0..full).map(|k| k * period).collect(),
    }
}

/// Split recorded notes — (start, end, pitch, velocity) in frames since the
/// loop start — into one note list per loop pass. Passes without notes are
/// skipped; notes held over a loop boundary are cut at the pass end.
pub fn midi_passes(notes: &[(i64, i64, u8, u8)], period: u64, spb: f64) -> Vec<Vec<Note>> {
    let p = period.max(1) as i64;
    let mut passes: std::collections::BTreeMap<i64, Vec<Note>> = Default::default();
    for &(s, e, pitch, velocity) in notes {
        if s < 0 {
            continue;
        }
        let k = s / p;
        let rs = s - k * p;
        let re = (e - k * p).min(p).max(rs + 1);
        passes.entry(k).or_default().push(Note { pitch, velocity, start: rs as f64 / spb, length: (re - rs) as f64 / spb });
    }
    passes.into_values().collect()
}
