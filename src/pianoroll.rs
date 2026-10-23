//! Piano roll editor for MIDI clips.

use crate::app::StudioApp;
use crate::project::{ClipContent, Id, Note};
use crate::song::PlayScope;
use crate::theme::{self, BORDER, GRID, GRID_BAR, PLAYHEAD, TEXT, TEXT_DIM};
use eframe::egui::{self, Color32, CursorIcon, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use egui_phosphor::regular as icon;

const KEY_W: f32 = 46.0;

struct NoteView {
    name: String,
    color: Color32,
    notes: Vec<Note>,
    /// Content window shown as "inside" (samples).
    offset: u64,
    length: u64,
    spb: f64,
    /// Playhead in content beats, when it is inside this content.
    playhead: Option<f64>,
}
const VEL_H: f32 = 56.0;
const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

fn is_black(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

#[derive(Clone, Copy, PartialEq)]
enum NoteDragKind {
    Move,
    Resize,
}

struct NoteDrag {
    kind: NoteDragKind,
    index: usize,
    orig: Note,
    grab_beat: f64,
    grab_pitch: i32,
}

pub struct RollState {
    clip: Option<Id>,
    ppb: f32,
    key_h: f32,
    /// Content beat at the left edge.
    scroll_x: f64,
    /// Pixels scrolled down from the top (pitch 127).
    scroll_y: f32,
    grid: f64,
    note_len: f64,
    pub selected: Option<usize>,
    drag: Option<NoteDrag>,
    needs_init: bool,
    /// Note grid area from the last frame (used by UI tests).
    pub grid_rect: Rect,
}

impl Default for RollState {
    fn default() -> Self {
        RollState {
            clip: None,
            ppb: 90.0,
            key_h: 11.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            grid: 0.25,
            note_len: 0.5,
            selected: None,
            drag: None,
            needs_init: true,
            grid_rect: Rect::NOTHING,
        }
    }
}

impl RollState {
    pub fn reset_for(&mut self, clip: Id) {
        if self.clip != Some(clip) {
            self.clip = Some(clip);
            self.selected = None;
            self.drag = None;
            self.needs_init = true;
        }
    }
}

impl StudioApp {
    fn clip_notes_mut(&mut self, id: Id) -> Option<&mut Vec<Note>> {
        self.project.notes_mut(id)
    }

    /// What the piano roll shows for a free MIDI clip or a MIDI take.
    fn note_view(&self, id: Id) -> Option<NoteView> {
        let rate = self.rate();
        if let Some((ti, ci)) = self.project.find_clip(id) {
            let t = &self.project.tracks[ti];
            let c = &t.clips[ci];
            let ClipContent::Midi { notes } = &c.content else { return None };
            let spb = self.project.samples_per_beat(rate);
            let playhead = (self.scope == PlayScope::Song).then(|| (self.position() - c.start + c.offset as i64) as f64 / spb);
            return Some(NoteView { name: c.name.clone(), color: theme::rgb(t.color), notes: notes.clone(), offset: c.offset, length: c.length, spb, playhead });
        }
        let (ti, si, ki) = self.project.find_take(id)?;
        let t = &self.project.tracks[ti];
        let s = &t.snippets[si];
        let ClipContent::Midi { notes } = &s.takes[ki].content else { return None };
        let spb = s.samples_per_beat(rate);
        let looping = self.scope == PlayScope::Snippet(s.id) && self.position() >= 0;
        let playhead = looping.then(|| (self.position() % s.length(rate).max(1) as i64) as f64 / spb);
        Some(NoteView {
            name: format!("{} · {}", s.name, s.takes[ki].name),
            color: theme::rgb(t.color),
            notes: notes.clone(),
            offset: 0,
            length: s.length(rate),
            spb,
            playhead,
        })
    }

    pub fn delete_selected_note(&mut self) {
        let (Some(clip), Some(i)) = (self.roll.clip, self.roll.selected.take()) else { return };
        self.history.checkpoint(&self.project);
        if let Some(notes) = self.clip_notes_mut(clip)
            && i < notes.len()
        {
            notes.remove(i);
        }
        self.arrangement_dirty = true;
    }

    pub fn quantize_selected_clip(&mut self) {
        let Some(clip) = self.roll.clip else { return };
        let grid = self.roll.grid;
        self.history.checkpoint(&self.project);
        if let Some(notes) = self.clip_notes_mut(clip) {
            for n in notes.iter_mut() {
                n.start = (n.start / grid).round() * grid;
                n.length = ((n.length / grid).round() * grid).max(grid);
            }
        }
        self.arrangement_dirty = true;
    }

    /// Grow the clip so it contains every note (rounded up to a bar).
    fn fit_clip_to_notes(&mut self, clip: Id) {
        let spb = self.project.samples_per_beat(self.rate());
        let Some(c) = self.project.clip_mut(clip) else { return };
        let ClipContent::Midi { notes } = &c.content else { return };
        let end_beats = notes.iter().map(|n| n.end()).fold(0.0, f64::max);
        let needed = (((end_beats / 4.0).ceil() * 4.0) * spb - c.offset as f64).max(0.0) as u64;
        if needed > c.length {
            c.length = needed;
        }
    }

    pub fn piano_roll(&mut self, ui: &mut Ui, clip_id: Id) {
        self.roll.reset_for(clip_id);
        let Some(view) = self.note_view(clip_id) else { return };
        let (spb, color, notes) = (view.spb, view.color, &view.notes);
        let off_b = view.offset as f64 / spb;
        let len_b = view.length as f64 / spb;

        // ---- Toolbar
        let mut quantize = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", icon::PIANO_KEYS, view.name)).strong().color(color));
            ui.separator();
            ui.label(RichText::new("Grid").small().color(TEXT_DIM));
            let grids = [(4.0, "1 bar"), (2.0, "1/2"), (1.0, "1/4"), (0.5, "1/8"), (0.25, "1/16"), (0.125, "1/32")];
            let current = grids.iter().find(|g| (g.0 - self.roll.grid).abs() < 1e-9).map_or("?", |g| g.1);
            egui::ComboBox::from_id_salt("roll_grid").selected_text(current).width(70.0).show_ui(ui, |ui| {
                for (v, l) in grids {
                    ui.selectable_value(&mut self.roll.grid, v, l);
                }
            });
            if ui.button("Quantize").on_hover_text("Snap note starts and lengths to the grid (Q)").clicked() {
                quantize = true;
            }
            if let Some(i) = self.roll.selected
                && let Some(n) = notes.get(i)
            {
                ui.separator();
                let octave = n.pitch as i32 / 12 - 1;
                ui.label(RichText::new(format!("{}{octave}", NAMES[n.pitch as usize % 12])).monospace());
                let mut vel = n.velocity as i32;
                ui.label(RichText::new("Velocity").small().color(TEXT_DIM));
                if ui.add(egui::DragValue::new(&mut vel).range(1..=127)).changed() {
                    let v = vel as u8;
                    if let Some(notes) = self.clip_notes_mut(clip_id)
                        && let Some(n) = notes.get_mut(i)
                    {
                        n.velocity = v;
                    }
                    self.arrangement_dirty = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, icon::MAGNIFYING_GLASS_PLUS, "Zoom in").clicked() {
                    self.roll.ppb = (self.roll.ppb * 1.25).min(800.0);
                }
                if theme::icon_button(ui, icon::MAGNIFYING_GLASS_MINUS, "Zoom out").clicked() {
                    self.roll.ppb = (self.roll.ppb * 0.8).max(10.0);
                }
                ui.label(RichText::new("click: add · drag: move · edge: resize · right-click: delete · Ctrl+wheel: zoom").small().color(TEXT_DIM));
            });
        });
        if quantize {
            self.quantize_selected_clip();
        }

        // ---- Layout
        let area = ui.available_rect_before_wrap();
        if area.height() < VEL_H + 40.0 {
            return;
        }
        let grid = Rect::from_min_max(pos2(area.left() + KEY_W, area.top()), pos2(area.right(), area.bottom() - VEL_H - 4.0));
        let keys = Rect::from_min_max(area.min, pos2(area.left() + KEY_W, grid.bottom()));
        let vel = Rect::from_min_max(pos2(grid.left(), grid.bottom() + 4.0), pos2(area.right(), area.bottom()));
        self.roll.grid_rect = grid;
        if self.roll.needs_init {
            // Show the clip start and centre the notes (or middle C) vertically.
            self.roll.needs_init = false;
            self.roll.scroll_x = off_b;
            let (lo, hi) = notes.iter().fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.pitch), hi.max(n.pitch)));
            let center = if notes.is_empty() { 60.0 } else { (lo as f32 + hi as f32) / 2.0 };
            let max = (128.0 * self.roll.key_h - grid.height()).max(0.0);
            self.roll.scroll_y = ((127.0 - center + 0.5) * self.roll.key_h - grid.height() / 2.0).clamp(0.0, max);
        }
        let resp = ui.allocate_rect(grid, Sense::click_and_drag());
        let vresp = ui.allocate_rect(vel, Sense::click_and_drag());
        ui.allocate_rect(keys, Sense::hover());

        let r = &self.roll;
        let (ppb, kh, sx, sy) = (r.ppb, r.key_h, r.scroll_x, r.scroll_y);
        let x_of = |b: f64| grid.left() + ((b - sx) * ppb as f64) as f32;
        let beat_at = |x: f32| sx + ((x - grid.left()) / ppb) as f64;
        let y_of = |p: u8| grid.top() + (127 - p as i32) as f32 * kh - sy;
        let pitch_at = |y: f32| (127 - ((y - grid.top() + sy) / kh).floor() as i32).clamp(0, 127);
        let note_rect = |n: &Note| Rect::from_min_max(pos2(x_of(n.start), y_of(n.pitch) + 1.0), pos2(x_of(n.end()).max(x_of(n.start) + 3.0), y_of(n.pitch) + kh - 1.0));

        // ---- Paint grid
        let p = ui.painter_at(grid);
        p.rect_filled(grid, 0, theme::LANE);
        for pitch in 0..=127u8 {
            let y = y_of(pitch);
            if y > grid.bottom() || y + kh < grid.top() {
                continue;
            }
            let row = Rect::from_min_max(pos2(grid.left(), y), pos2(grid.right(), y + kh));
            if is_black(pitch) {
                p.rect_filled(row, 0, Color32::from_rgb(0x16, 0x18, 0x1d));
            }
            if pitch % 12 == 0 {
                p.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0, GRID_BAR));
            }
        }
        let step = self.roll.grid;
        let mut b = (beat_at(grid.left()) / step).floor() * step;
        while x_of(b) <= grid.right() {
            let bar = (b / 4.0).fract().abs() < 1e-9;
            let beat = b.fract().abs() < 1e-9;
            let c = if bar { GRID_BAR.gamma_multiply(1.6) } else if beat { GRID_BAR } else { GRID };
            p.line_segment([pos2(x_of(b), grid.top()), pos2(x_of(b), grid.bottom())], Stroke::new(1.0, c));
            b += step;
        }
        // Outside the clip: dimmed
        let dim = Color32::from_black_alpha(110);
        let (cl, cr) = (x_of(off_b), x_of(off_b + len_b));
        if cl > grid.left() {
            p.rect_filled(Rect::from_min_max(grid.min, pos2(cl, grid.bottom())), 0, dim);
        }
        if cr < grid.right() {
            p.rect_filled(Rect::from_min_max(pos2(cr, grid.top()), grid.max), 0, dim);
        }
        for (i, n) in notes.iter().enumerate() {
            let nr = note_rect(n);
            if !nr.intersects(grid) {
                continue;
            }
            let a = 0.45 + 0.55 * n.velocity as f32 / 127.0;
            p.rect_filled(nr, 2, color.gamma_multiply(a));
            let sel = self.roll.selected == Some(i);
            p.rect_stroke(nr, 2, Stroke::new(if sel { 1.5 } else { 1.0 }, if sel { TEXT } else { color }), StrokeKind::Inside);
        }
        // Playhead (content beat)
        if let Some(ph) = view.playhead
            && ph >= off_b
            && ph <= off_b + len_b
        {
            let x = x_of(ph);
            p.line_segment([pos2(x, grid.top()), pos2(x, grid.bottom())], Stroke::new(1.0, PLAYHEAD));
        }

        // ---- Keyboard
        let kp = ui.painter_at(keys);
        kp.rect_filled(keys, 0, theme::PANEL);
        for pitch in 0..=127u8 {
            let y = y_of(pitch);
            if y > keys.bottom() || y + kh < keys.top() {
                continue;
            }
            let row = Rect::from_min_max(pos2(keys.left(), y), pos2(keys.right() - 1.0, y + kh));
            let (fill, w) = if is_black(pitch) { (Color32::from_rgb(0x22, 0x25, 0x2c), 0.62) } else { (Color32::from_rgb(0xc9, 0xcd, 0xd5), 1.0) };
            kp.rect_filled(Rect::from_min_size(row.min, vec2(row.width() * w, kh - 1.0)), 1, fill);
            if pitch % 12 == 0 {
                kp.text(
                    pos2(row.right() - 3.0, row.center().y),
                    egui::Align2::RIGHT_CENTER,
                    format!("C{}", pitch as i32 / 12 - 1),
                    FontId::proportional(9.0),
                    Color32::from_rgb(0x30, 0x34, 0x3c),
                );
            }
        }

        // ---- Velocity lane
        let vp = ui.painter_at(vel);
        vp.rect_filled(vel, 3, theme::BG);
        for (i, n) in notes.iter().enumerate() {
            let x = x_of(n.start);
            if x < vel.left() || x > vel.right() {
                continue;
            }
            let h = (vel.height() - 6.0) * n.velocity as f32 / 127.0;
            let c = if self.roll.selected == Some(i) { TEXT } else { color };
            vp.line_segment([pos2(x, vel.bottom() - 3.0), pos2(x, vel.bottom() - 3.0 - h)], Stroke::new(2.0, c));
            vp.circle_filled(pos2(x, vel.bottom() - 3.0 - h), 2.5, c);
        }
        vp.rect_stroke(vel, 3, Stroke::new(1.0, BORDER), StrokeKind::Inside);

        // ---- Scroll / zoom
        if let Some(hp) = ui.input(|i| i.pointer.hover_pos())
            && (grid.contains(hp) || keys.contains(hp))
        {
            let (d, z) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            if z != 1.0 {
                let anchor = beat_at(hp.x);
                self.roll.ppb = (self.roll.ppb * z).clamp(10.0, 800.0);
                self.roll.scroll_x = anchor - ((hp.x - grid.left()) / self.roll.ppb) as f64;
            }
            self.roll.scroll_x -= (d.x / self.roll.ppb) as f64;
            self.roll.scroll_y = (self.roll.scroll_y - d.y).clamp(0.0, (128.0 * kh - grid.height()).max(0.0));
        }

        // ---- Note editing
        let alt = ui.input(|i| i.modifiers.alt);
        let snap = |b: f64| if alt { b } else { (b / step).floor() * step };
        let snap_round = |b: f64| if alt { b } else { (b / step).round() * step };
        let hit = |pos: Pos2| -> Option<(usize, bool)> {
            notes.iter().enumerate().rev().find(|(_, n)| note_rect(n).contains(pos)).map(|(i, n)| (i, note_rect(n).right() - pos.x < 6.0))
        };
        if let Some(hp) = resp.hover_pos() {
            match hit(hp) {
                Some((_, true)) => ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal),
                Some((_, false)) => ui.ctx().set_cursor_icon(CursorIcon::Grab),
                None => {}
            }
        }

        let mut changed = false;
        let new_note = |pos: Pos2, len: f64| Note { pitch: pitch_at(pos.y) as u8, velocity: 100, start: snap(beat_at(pos.x)).max(0.0), length: len };
        if resp.drag_started()
            && let Some(origin) = ui.input(|i| i.pointer.press_origin())
            && ui.input(|i| i.pointer.primary_down())
        {
            self.history.checkpoint(&self.project);
            let (index, kind, orig) = match hit(origin) {
                Some((i, resize)) => (i, if resize { NoteDragKind::Resize } else { NoteDragKind::Move }, notes[i]),
                None => {
                    let n = new_note(origin, self.roll.grid);
                    let len = notes.len();
                    if let Some(list) = self.clip_notes_mut(clip_id) {
                        list.push(n);
                    }
                    changed = true;
                    (len, NoteDragKind::Resize, n)
                }
            };
            self.roll.selected = Some(index);
            self.roll.drag = Some(NoteDrag { kind, index, orig, grab_beat: beat_at(origin.x), grab_pitch: pitch_at(origin.y) });
        }
        if resp.dragged()
            && let Some(pos) = resp.interact_pointer_pos()
            && let Some(d) = &self.roll.drag
        {
            let (kind, index, orig, gb, gp) = (d.kind, d.index, d.orig, d.grab_beat, d.grab_pitch);
            let mut new_len = None;
            if let Some(n) = self.clip_notes_mut(clip_id).and_then(|l| l.get_mut(index)) {
                match kind {
                    NoteDragKind::Move => {
                        n.start = snap_round(orig.start + beat_at(pos.x) - gb).max(0.0);
                        n.pitch = (orig.pitch as i32 + pitch_at(pos.y) - gp).clamp(0, 127) as u8;
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    }
                    NoteDragKind::Resize => {
                        let end = snap_round(beat_at(pos.x)).max(orig.start + if alt { 0.01 } else { step });
                        n.length = end - n.start;
                        new_len = Some(n.length);
                        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                    }
                }
                changed = true;
            }
            if let Some(l) = new_len {
                self.roll.note_len = l;
            }
        }
        if resp.drag_stopped() {
            self.roll.drag = None;
        }
        if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            match hit(pos) {
                Some((i, _)) => self.roll.selected = Some(i),
                None => {
                    self.history.checkpoint(&self.project);
                    let n = new_note(pos, self.roll.note_len);
                    let idx = notes.len();
                    if let Some(list) = self.clip_notes_mut(clip_id) {
                        list.push(n);
                    }
                    self.roll.selected = Some(idx);
                    changed = true;
                }
            }
        }
        if (resp.secondary_clicked() || resp.double_clicked())
            && let Some(pos) = resp.interact_pointer_pos()
            && let Some((i, _)) = hit(pos)
        {
            self.roll.selected = Some(i);
            self.delete_selected_note();
        }

        // Velocity lane: drag to set the velocity of the note under the pointer.
        if vresp.drag_started() || vresp.clicked() {
            self.history.checkpoint(&self.project);
        }
        if (vresp.dragged() || vresp.clicked())
            && let Some(pos) = vresp.interact_pointer_pos()
        {
            let v = (((vel.bottom() - 3.0 - pos.y) / (vel.height() - 6.0)) * 127.0).clamp(1.0, 127.0) as u8;
            let target = notes
                .iter()
                .enumerate()
                .filter(|(_, n)| (x_of(n.start) - pos.x).abs() < 6.0)
                .min_by(|a, b| (x_of(a.1.start) - pos.x).abs().total_cmp(&(x_of(b.1.start) - pos.x).abs()))
                .map(|(i, _)| i);
            if let Some(i) = target
                && let Some(n) = self.clip_notes_mut(clip_id).and_then(|l| l.get_mut(i))
            {
                n.velocity = v;
                self.roll.selected = Some(i);
                changed = true;
            }
        }

        if changed {
            self.fit_clip_to_notes(clip_id);
            self.arrangement_dirty = true;
        }
    }
}
