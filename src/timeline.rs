//! Arrangement view: ruler, track headers and clip lanes with
//! non-destructive editing (move, trim, fade, split, cross-track moves).

use crate::app::{ALL_MIDI, BottomTab, StudioApp};
use crate::audio::range_peak;
use crate::project::{Clip, ClipContent, Id, TRACK_COLORS, TrackKind};
use crate::snippets::SnippetDrag;
use crate::song::PlayScope;
use crate::theme::{self, ACCENT, BORDER, GRID, GRID_BAR, LANE, LANE_ALT, MONITOR, MUTE, PLAYHEAD, REC, SOLO, TEXT, TEXT_DIM};
use eframe::egui::{self, Color32, CursorIcon, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use egui_phosphor::regular as icon;

pub const HEADER_W: f32 = 300.0;
const RULER_H: f32 = 28.0;
const EDGE: f32 = 7.0;

pub struct View {
    pub px_per_sec: f32,
    /// Timeline time (seconds) at the left edge of the lanes.
    pub scroll: f64,
    pub track_h: f32,
}

impl Default for View {
    fn default() -> Self {
        View { px_per_sec: 50.0, scroll: 0.0, track_h: 92.0 }
    }
}

impl View {
    pub fn zoom(&mut self, factor: f32) {
        self.px_per_sec = (self.px_per_sec * factor).clamp(2.0, 4000.0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DragKind {
    Move,
    TrimStart,
    TrimEnd,
    FadeIn,
    FadeOut,
}

pub struct Drag {
    pub kind: DragKind,
    pub clip: Id,
    pub orig: Clip,
    /// Pointer position minus clip start when the drag began (samples).
    pub grab: i64,
}

fn short_port(p: &str) -> String {
    let s = p.rsplit(':').next().unwrap_or(p);
    if s.chars().count() > 28 { format!("{}…", s.chars().take(27).collect::<String>()) } else { s.to_string() }
}

impl StudioApp {
    fn x_of(&self, left: f32, sample: i64) -> f32 {
        left + ((sample as f64 / self.rate() as f64 - self.view.scroll) * self.view.px_per_sec as f64) as f32
    }

    fn sample_at(&self, left: f32, x: f32) -> i64 {
        (((x - left) as f64 / self.view.px_per_sec as f64 + self.view.scroll) * self.rate() as f64) as i64
    }

    /// Grid spacing in beats, adapted to the zoom level.
    fn grid_beats(&self) -> f64 {
        let px_per_beat = self.view.px_per_sec as f64 * 60.0 / self.project.bpm as f64;
        let mut step = 1.0 / 16.0;
        while step * px_per_beat < 14.0 {
            step *= 2.0;
        }
        step
    }

    pub fn snap(&self, s: i64, bypass: bool) -> i64 {
        if !self.project.snap || bypass {
            return s;
        }
        let grid = self.grid_beats() * self.project.samples_per_beat(self.rate());
        ((s as f64 / grid).round() * grid) as i64
    }

    fn draw_grid(&self, painter: &egui::Painter, rect: Rect, left: f32) {
        let spb = self.project.samples_per_beat(self.rate());
        let step = self.grid_beats();
        let first = (self.sample_at(left, rect.left()) as f64 / spb / step).floor() * step;
        let mut b = first.max(0.0);
        loop {
            let x = self.x_of(left, (b * spb) as i64);
            if x > rect.right() {
                break;
            }
            let bar = (b / 4.0).fract().abs() < 1e-9;
            painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, if bar { GRID_BAR } else { GRID }));
            b += step;
        }
    }

    /// The linear timeline with its fold header.
    pub fn timeline_panel(&mut self, ui: &mut Ui) {
        let mut folded = self.folds.timeline;
        egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            StudioApp::fold_header(ui, &mut folded, &format!("{} Timeline", icon::LIST), |ui| {
                ui.label(RichText::new("assemble the song: drag snippets here · placements are linked").small().color(TEXT_DIM));
            });
        });
        self.folds.timeline = folded;
        if !folded {
            self.timeline(ui);
        }
    }

    pub fn timeline(&mut self, ui: &mut Ui) {
        let full = ui.available_rect_before_wrap();
        let left = full.left() + HEADER_W;
        let ruler = Rect::from_min_max(pos2(left, full.top()), pos2(full.right(), full.top() + RULER_H));
        let corner = Rect::from_min_max(full.min, pos2(left, full.top() + RULER_H));
        let lanes = Rect::from_min_max(pos2(full.left(), full.top() + RULER_H), full.max);

        ui.scope_builder(UiBuilder::new().max_rect(corner.shrink2(vec2(8.0, 3.0))), |ui| {
            ui.horizontal_centered(|ui| {
                if ui.button(format!("{} Audio", icon::PLUS)).on_hover_text("Add audio track (Ctrl+T)").clicked() {
                    self.add_track(TrackKind::Audio);
                }
                if ui.button(format!("{} Instrument", icon::PLUS)).on_hover_text("Add instrument track (Ctrl+Shift+T)").clicked() {
                    self.add_track(TrackKind::Instrument);
                }
            });
        });
        self.ruler(ui, ruler, left);

        ui.scope_builder(UiBuilder::new().max_rect(lanes), |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 1.0);
                self.lane_rects.clear();
                let ids: Vec<Id> = self.project.tracks.iter().map(|t| t.id).collect();
                for (i, id) in ids.into_iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                        self.track_header(ui, id);
                        self.lane(ui, id, i, left);
                    });
                }
                let rest = ui.available_rect_before_wrap();
                if rest.height() > 30.0 {
                    let r = Rect::from_min_size(rest.min, vec2(rest.width(), rest.height().min(120.0)));
                    ui.painter().text(
                        r.center(),
                        egui::Align2::CENTER_CENTER,
                        "Drop WAV files here to import · double-click an instrument lane to create a MIDI clip",
                        FontId::proportional(12.5),
                        TEXT_DIM.gamma_multiply(0.6),
                    );
                }
            });
        });

        // Horizontal scrolling / zooming over the arrangement.
        let ctx = ui.ctx().clone();
        let hover = ctx.input(|i| i.pointer.hover_pos());
        if let Some(p) = hover
            && (lanes.contains(p) || ruler.contains(p))
            && p.x > left
        {
            let (scroll, zoom) = ctx.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            if zoom != 1.0 {
                let anchor = self.sample_at(left, p.x);
                self.view.zoom(zoom);
                // Keep the time under the pointer fixed.
                self.view.scroll = anchor as f64 / self.rate() as f64 - (p.x - left) as f64 / self.view.px_per_sec as f64;
            }
            if scroll.x != 0.0 {
                self.view.scroll -= scroll.x as f64 / self.view.px_per_sec as f64;
            }
            self.view.scroll = self.view.scroll.max(0.0);
        }
        // Follow the playhead.
        if self.playing() && self.scope == PlayScope::Song && self.drag.is_none() {
            let x = self.x_of(left, self.song_position());
            if x > full.right() - 20.0 || x < left {
                self.view.scroll = self.song_position() as f64 / self.rate() as f64 - 0.1 * (full.right() - left) as f64 / self.view.px_per_sec as f64;
                self.view.scroll = self.view.scroll.max(0.0);
            }
        }
    }

    fn ruler(&mut self, ui: &mut Ui, rect: Rect, left: f32) {
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0, theme::PANEL);
        let spb = self.project.samples_per_beat(self.rate());
        let px_per_bar = self.view.px_per_sec as f64 * 4.0 * 60.0 / self.project.bpm as f64;
        let mut every = 1;
        while (every as f64) * px_per_bar < 48.0 {
            every *= 2;
        }
        let first_bar = (self.sample_at(left, rect.left()) as f64 / spb / 4.0).floor().max(0.0) as i64;
        let mut bar = first_bar - first_bar % every;
        loop {
            let x = self.x_of(left, (bar as f64 * 4.0 * spb) as i64);
            if x > rect.right() {
                break;
            }
            p.line_segment([pos2(x, rect.bottom() - 10.0), pos2(x, rect.bottom())], Stroke::new(1.0, TEXT_DIM));
            p.text(pos2(x + 4.0, rect.top() + 4.0), egui::Align2::LEFT_TOP, format!("{}", bar + 1), FontId::proportional(11.5), TEXT_DIM);
            if every == 1 && px_per_bar > 80.0 {
                for beat in 1..4 {
                    let bx = self.x_of(left, ((bar as f64 * 4.0 + beat as f64) * spb) as i64);
                    p.line_segment([pos2(bx, rect.bottom() - 5.0), pos2(bx, rect.bottom())], Stroke::new(1.0, BORDER));
                }
            }
            bar += every;
        }
        p.line_segment([pos2(rect.left(), rect.bottom()), pos2(rect.right(), rect.bottom())], Stroke::new(1.0, BORDER));
        // Playhead marker
        let x = self.x_of(left, self.song_position());
        if x >= rect.left() {
            let tri = vec![pos2(x - 6.0, rect.top() + 14.0), pos2(x + 6.0, rect.top() + 14.0), pos2(x, rect.bottom())];
            p.add(egui::Shape::convex_polygon(tri, ACCENT, Stroke::NONE));
        }
        if (resp.clicked() || resp.dragged())
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let alt = ui.input(|i| i.modifiers.alt);
            let s = self.snap(self.sample_at(left, pos.x), alt).max(0);
            self.set_scope(PlayScope::Song);
            self.locate(s);
        }
        resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Click or drag to move the playhead");
    }

    // -----------------------------------------------------------------------

    fn track_header(&mut self, ui: &mut Ui, id: Id) {
        let selected = self.sel.track == Some(id);
        let (rect, resp) = ui.allocate_exact_size(vec2(HEADER_W, self.view.track_h), Sense::click());
        if resp.clicked() {
            self.sel.track = Some(id);
        }
        let Some(t) = self.project.track(id) else { return };
        let color = theme::rgb(t.color);
        let p = ui.painter();
        p.rect_filled(rect, 0, if selected { theme::PANEL_HI } else { theme::PANEL });
        p.rect_filled(Rect::from_min_size(rect.min, vec2(4.0, rect.height())), 0, color);
        p.line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, BORDER));

        let mut inputs_changed = false;
        let mut delete = false;
        let StudioApp { project, audio_ports, midi_ports, meters, .. } = self;
        let t = project.track_mut(id).unwrap();
        let meter = meters.get(&id).copied().unwrap_or_default();
        let inner = Rect::from_min_max(rect.min + vec2(12.0, 6.0), rect.max - vec2(8.0, 4.0));
        ui.scope_builder(UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 5.0);
            ui.horizontal(|ui| {
                let kind_icon = if t.kind == TrackKind::Audio { icon::MICROPHONE } else { icon::PIANO_KEYS };
                ui.label(RichText::new(kind_icon).color(color));
                ui.add(egui::TextEdit::singleline(&mut t.name).frame(egui::Frame::NONE).desired_width(112.0).font(egui::TextStyle::Body));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::toggle(ui, &mut t.solo, "S", SOLO, "Solo");
                    theme::toggle(ui, &mut t.mute, "M", MUTE, "Mute");
                    let mon_tip = if t.kind == TrackKind::Audio { "Monitor input (hear the input through the track's devices)" } else { "Monitor MIDI input (play the instrument live)" };
                    theme::toggle(ui, &mut t.monitor, icon::HEADPHONES, MONITOR, mon_tip);
                    theme::toggle(ui, &mut t.armed, icon::RECORD, REC, "Arm for recording");
                });
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = 118.0;
                let vol = ui
                    .add(egui::Slider::new(&mut t.volume_db, -60.0..=6.0).show_value(false).trailing_fill(true))
                    .on_hover_text(format!("Volume {:.1} dB (double-click: 0 dB)", t.volume_db));
                if vol.double_clicked() {
                    t.volume_db = 0.0;
                }
                ui.label(RichText::new(format!("{:+.1}", t.volume_db)).small().monospace().color(TEXT_DIM));
                ui.spacing_mut().slider_width = 54.0;
                let pan = ui.add(egui::Slider::new(&mut t.pan, -1.0..=1.0).show_value(false)).on_hover_text(format!(
                    "Pan {}",
                    if t.pan.abs() < 0.01 { "C".into() } else if t.pan < 0.0 { format!("L{:.0}", -t.pan * 100.0) } else { format!("R{:.0}", t.pan * 100.0) }
                ));
                if pan.double_clicked() {
                    t.pan = 0.0;
                }
            });
            ui.horizontal(|ui| {
                let label = match t.kind {
                    TrackKind::Audio => match (&t.input_l, t.stereo) {
                        (None, _) => "No input".to_string(),
                        (Some(l), false) => short_port(l),
                        (Some(l), true) => format!("{} + {}", short_port(l), t.input_r.as_deref().map(short_port).unwrap_or_default()),
                    },
                    TrackKind::Instrument => match t.input_l.as_deref() {
                        None => "No MIDI input".into(),
                        Some(ALL_MIDI) => "All MIDI inputs".into(),
                        Some(p) => short_port(p),
                    },
                };
                let combo = egui::ComboBox::from_id_salt(("input", id)).selected_text(RichText::new(label).small()).width(132.0);
                combo
                    .show_ui(ui, |ui| match t.kind {
                        TrackKind::Audio => {
                            if ui.selectable_label(t.input_l.is_none(), "No input").clicked() {
                                t.input_l = None;
                                inputs_changed = true;
                            }
                            ui.separator();
                            theme::section_label(ui, "Mono");
                            for port in audio_ports.iter() {
                                if ui.selectable_label(!t.stereo && t.input_l.as_ref() == Some(port), port).clicked() {
                                    t.input_l = Some(port.clone());
                                    t.stereo = false;
                                    inputs_changed = true;
                                }
                            }
                            ui.separator();
                            theme::section_label(ui, "Stereo");
                            for pair in audio_ports.windows(2) {
                                let client = |p: &str| p.split(':').next().unwrap_or("").to_string();
                                if client(&pair[0]) != client(&pair[1]) {
                                    continue;
                                }
                                let on = t.stereo && t.input_l.as_ref() == Some(&pair[0]);
                                if ui.selectable_label(on, format!("{} + {}", pair[0], short_port(&pair[1]))).clicked() {
                                    t.input_l = Some(pair[0].clone());
                                    t.input_r = Some(pair[1].clone());
                                    t.stereo = true;
                                    inputs_changed = true;
                                }
                            }
                        }
                        TrackKind::Instrument => {
                            for (value, text) in [(None, "No MIDI input"), (Some(ALL_MIDI), "All MIDI inputs")] {
                                if ui.selectable_label(t.input_l.as_deref() == value, text).clicked() {
                                    t.input_l = value.map(String::from);
                                    inputs_changed = true;
                                }
                            }
                            ui.separator();
                            for port in midi_ports.iter() {
                                if ui.selectable_label(t.input_l.as_ref() == Some(port), port).clicked() {
                                    t.input_l = Some(port.clone());
                                    inputs_changed = true;
                                }
                            }
                        }
                    })
                    .response
                    .on_hover_text(t.input_l.clone().unwrap_or_default());
                match t.kind {
                    TrackKind::Audio => {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            theme::meter(ui, 104.0, [meter[0], meter[1]], "Input level (pre-fader)");
                            theme::meter(ui, 104.0, [meter[2], meter[3]], "Output level (post-fader)");
                        });
                    }
                    TrackKind::Instrument => {
                        let active = t.shared.midi_activity.swap(false, std::sync::atomic::Ordering::Relaxed);
                        ui.label(RichText::new(icon::CIRCLE).size(9.0).color(if active { SOLO } else { theme::BG })).on_hover_text("MIDI input activity");
                        theme::meter(ui, 90.0, [meter[2], meter[3]], "Output level (post-fader)");
                    }
                }
            });
        });

        resp.context_menu(|ui| {
            ui.label(RichText::new(&t.name).strong());
            ui.horizontal(|ui| {
                for c in TRACK_COLORS {
                    let (r, cr) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::click());
                    ui.painter().rect_filled(r, 4, theme::rgb(c));
                    if cr.clicked() {
                        t.color = c;
                    }
                }
            });
            ui.separator();
            if ui.button(format!("{} Delete track", icon::TRASH)).clicked() {
                delete = true;
                ui.close();
            }
        });

        if inputs_changed {
            self.connect_track_inputs(id);
        }
        if delete {
            self.remove_track(id);
        }
    }

    // -----------------------------------------------------------------------

    fn clip_rect(&self, lane: Rect, left: f32, c: &Clip) -> Rect {
        Rect::from_min_max(pos2(self.x_of(left, c.start), lane.top() + 2.0), pos2(self.x_of(left, c.end()), lane.bottom() - 2.0))
    }

    fn hit_test(&self, track: Id, lane: Rect, left: f32, p: Pos2) -> Option<(Id, DragKind)> {
        let t = self.project.track(track)?;
        let c = t.clips.iter().rev().find(|c| self.clip_rect(lane, left, c).contains(p))?;
        let r = self.clip_rect(lane, left, c);
        if r.width() < 3.0 * EDGE {
            return Some((c.id, DragKind::Move));
        }
        let is_audio = matches!(c.content, ClipContent::Audio { .. });
        if is_audio && p.y < r.top() + 14.0 {
            let fi = self.x_of(left, c.start + c.fade_in as i64);
            let fo = self.x_of(left, c.end() - c.fade_out as i64);
            if (p.x - fi).abs() < 8.0 {
                return Some((c.id, DragKind::FadeIn));
            }
            if (p.x - fo).abs() < 8.0 {
                return Some((c.id, DragKind::FadeOut));
            }
        }
        if p.x - r.left() < EDGE {
            Some((c.id, DragKind::TrimStart))
        } else if r.right() - p.x < EDGE {
            Some((c.id, DragKind::TrimEnd))
        } else {
            Some((c.id, DragKind::Move))
        }
    }

    fn lane(&mut self, ui: &mut Ui, id: Id, index: usize, left: f32) {
        let w = ui.available_width().max(10.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(w, self.view.track_h), Sense::click_and_drag());
        self.lane_rects.push((id, rect));
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, if index.is_multiple_of(2) { LANE } else { LANE_ALT });
        self.draw_grid(&painter, rect, left);

        let Some(track) = self.project.track(id) else { return };
        let color = theme::rgb(track.color);
        for c in &track.clips {
            let r = self.clip_rect(rect, left, c);
            if r.right() < rect.left() || r.left() > rect.right() {
                continue;
            }
            self.draw_clip(&painter, r, left, c, color, self.sel.clip == Some(c.id));
        }

        // Live recording overlay.
        if self.recording() && self.scope == PlayScope::Song && track.armed {
            let start = self.engine.shared.record_start.load(std::sync::atomic::Ordering::Relaxed);
            let r = Rect::from_min_max(pos2(self.x_of(left, start), rect.top() + 2.0), pos2(self.x_of(left, self.position()), rect.bottom() - 2.0));
            painter.rect_filled(r, 4, REC.gamma_multiply(0.25));
            painter.rect_stroke(r, 4, Stroke::new(1.0, REC), StrokeKind::Inside);
            if let Some(rec) = self.audio_take_live(id)
                && let Ok(live) = rec.live.try_lock()
            {
                let mid = r.center().y;
                let h = r.height() * 0.45;
                let x0 = r.left().max(rect.left()) as i32;
                for px in x0..r.right().min(rect.right()) as i32 {
                    let a = (self.sample_at(left, px as f32) - start).max(0) as usize;
                    let b = (self.sample_at(left, px as f32 + 1.0) - start).max(0) as usize;
                    let (lo, hi) = range_peak(&live.channels, &live.peaks, a, b);
                    painter.line_segment([pos2(px as f32, mid - hi * h), pos2(px as f32, mid - lo * h)], Stroke::new(1.0, REC));
                }
            }
            if let Some(take) = self.midi_takes.get(&id) {
                let now = self.position();
                let notes = take.notes.iter().map(|n| (n.0, n.1, n.2)).chain(take.open_notes().map(|(s, p)| (s, now, p)));
                for (s, e, pitch) in notes {
                    let y = r.bottom() - 4.0 - (pitch as f32 / 127.0) * (r.height() - 8.0);
                    painter.line_segment([pos2(self.x_of(left, s), y), pos2(self.x_of(left, e).max(self.x_of(left, s) + 2.0), y)], Stroke::new(2.5, REC));
                }
            }
        }

        // Playhead
        let x = self.x_of(left, self.song_position());
        if x >= rect.left() {
            painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, PLAYHEAD.gamma_multiply(0.8)));
        }

        self.lane_interaction(ui, &resp, id, rect, left);
    }

    fn draw_clip(&self, painter: &egui::Painter, r: Rect, left: f32, c: &Clip, color: Color32, selected: bool) {
        let base = if c.muted { Color32::from_gray(90) } else { color };
        painter.rect_filled(r, 5, base.gamma_multiply(0.28));
        let head = Rect::from_min_max(r.min, pos2(r.right(), r.top() + 15.0));
        painter.rect_filled(head, egui::CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }, base.gamma_multiply(0.7));
        let name_clip = painter.with_clip_rect(head.intersect(painter.clip_rect()));
        // Linked placements show the snippet's live name and active take.
        let title = match &c.content {
            ClipContent::Snippet { snippet } => match self.project.snippet(*snippet) {
                Some(s) => format!("{} {} · {}", icon::LINK, s.name, s.active_take().map_or("no takes", |k| k.name.as_str())),
                None => format!("{} missing snippet", icon::LINK),
            },
            _ => c.name.clone(),
        };
        name_clip.text(head.left_center() + vec2(6.0, 0.0), egui::Align2::LEFT_CENTER, title, FontId::proportional(11.0), Color32::from_rgb(0x10, 0x12, 0x16));
        let body = Rect::from_min_max(pos2(r.left(), head.bottom()), r.max);
        let vis = body.intersect(painter.clip_rect());
        match &c.content {
            ClipContent::Audio { source } => {
                if let Some(data) = self.sources.get(source) {
                    let mid = body.center().y;
                    let h = body.height() * 0.48 * c.gain.min(4.0);
                    let stroke = Stroke::new(1.0, base.gamma_multiply(0.95));
                    for px in vis.left() as i32..vis.right() as i32 {
                        let t0 = self.sample_at(left, px as f32) - c.start;
                        let t1 = self.sample_at(left, px as f32 + 1.0) - c.start;
                        let a = (c.offset as i64 + t0.max(0)) as usize;
                        let b = (c.offset as i64 + t1.clamp(0, c.length as i64)) as usize;
                        let (lo, hi) = data.range_peak(a, b.max(a + 1));
                        painter.line_segment([pos2(px as f32, mid - hi * h), pos2(px as f32, mid - lo * h + 1.0)], stroke);
                    }
                    // Fades
                    let w = |s: u64| (s as f64 / self.rate() as f64 * self.view.px_per_sec as f64) as f32;
                    let fs = Stroke::new(1.2, TEXT.gamma_multiply(0.7));
                    let fi = r.left() + w(c.fade_in);
                    let fo = r.right() - w(c.fade_out);
                    if c.fade_in > 0 {
                        painter.line_segment([pos2(r.left(), r.bottom()), pos2(fi, head.bottom())], fs);
                    }
                    if c.fade_out > 0 {
                        painter.line_segment([pos2(fo, head.bottom()), pos2(r.right(), r.bottom())], fs);
                    }
                    if selected && r.width() > 40.0 {
                        for x in [fi, fo] {
                            painter.rect_filled(Rect::from_center_size(pos2(x, head.bottom()), vec2(7.0, 7.0)), 2, TEXT);
                        }
                    }
                } else {
                    painter.text(body.center(), egui::Align2::CENTER_CENTER, "missing audio", FontId::proportional(11.0), REC);
                }
            }
            ClipContent::Midi { notes } => {
                let spb = self.project.samples_per_beat(self.rate());
                let (lo, hi) = notes.iter().fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.pitch), hi.max(n.pitch)));
                let span = (hi.saturating_sub(lo) as f32 + 1.0).max(12.0);
                let nh = ((body.height() - 6.0) / span).clamp(2.0, 6.0);
                let px_per_sample = self.view.px_per_sec / self.rate() as f32;
                for n in notes {
                    let s = n.start * spb - c.offset as f64;
                    let e = n.end() * spb - c.offset as f64;
                    if e <= 0.0 || s >= c.length as f64 {
                        continue;
                    }
                    let x0 = r.left() + s.max(0.0) as f32 * px_per_sample;
                    let x1 = r.left() + e.min(c.length as f64) as f32 * px_per_sample;
                    let y = body.bottom() - 3.0 - (n.pitch.saturating_sub(lo) as f32 + 1.0) / span * (body.height() - 6.0);
                    painter.rect_filled(Rect::from_min_max(pos2(x0, y), pos2(x1.max(x0 + 2.0), y + nh)), 1, base);
                }
            }
            ClipContent::Snippet { snippet } => {
                if let Some(s) = self.project.snippet(*snippet) {
                    self.draw_snippet_placement(painter, body, vis, left, c, s, base);
                }
            }
        }
        let stroke = if selected { Stroke::new(1.5, TEXT) } else { Stroke::new(1.0, base.gamma_multiply(0.8)) };
        painter.rect_stroke(r, 5, stroke, StrokeKind::Inside);
    }

    /// Draw a linked placement: the snippet's active take repeated per loop,
    /// with loop boundaries and the snippet's own bar lines.
    #[allow(clippy::too_many_arguments)]
    fn draw_snippet_placement(&self, painter: &egui::Painter, body: Rect, vis: Rect, left: f32, c: &Clip, s: &crate::project::Snippet, color: Color32) {
        let rate = self.rate();
        let period = s.length(rate).max(1) as i64;
        let bar = (s.samples_per_beat(rate) * s.beats_per_bar as f64) as i64;
        // Content time (samples into the looping snippet) at timeline x.
        let content_at = |x: f32| self.sample_at(left, x) - c.start + c.offset as i64;
        let x_of_content = |t: i64| self.x_of(left, c.start + t - c.offset as i64);
        // Bar lines and loop boundaries.
        let (t0, t1) = (content_at(vis.left()).max(c.offset as i64), content_at(vis.right()));
        if bar > 0 && (bar as f32) * self.view.px_per_sec / rate as f32 > 6.0 {
            let mut b = (t0 / bar) * bar;
            while b <= t1 {
                let x = x_of_content(b);
                let boundary = b % period == 0;
                let stroke = if boundary { Stroke::new(1.0, color.gamma_multiply(0.9)) } else { Stroke::new(1.0, color.gamma_multiply(0.25)) };
                if x > body.left() + 1.0 {
                    painter.line_segment([pos2(x, body.top()), pos2(x, body.bottom())], stroke);
                }
                b += bar;
            }
        }
        let Some(take) = s.active_take() else {
            painter.text(body.center(), egui::Align2::CENTER_CENTER, "no takes yet", FontId::proportional(11.0), TEXT_DIM);
            return;
        };
        match &take.content {
            ClipContent::Audio { source } => {
                let Some(data) = self.sources.get(source) else { return };
                let mid = body.center().y;
                let h = body.height() * 0.48 * c.gain.min(4.0);
                let stroke = Stroke::new(1.0, color.gamma_multiply(0.95));
                for px in vis.left() as i32..vis.right() as i32 {
                    let a = content_at(px as f32).rem_euclid(period);
                    let b = content_at(px as f32 + 1.0) - content_at(px as f32) + a;
                    let (sa, sb) = (take.offset as usize + a as usize, take.offset as usize + b.min(period) as usize);
                    let (lo, hi) = data.range_peak(sa, sb.max(sa + 1));
                    painter.line_segment([pos2(px as f32, mid - hi * h), pos2(px as f32, mid - lo * h + 1.0)], stroke);
                }
            }
            ClipContent::Midi { notes } => {
                let spb = s.samples_per_beat(rate);
                let (lo, hi) = notes.iter().fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.pitch), hi.max(n.pitch)));
                let span = (hi.saturating_sub(lo) as f32 + 1.0).max(12.0);
                let nh = ((body.height() - 6.0) / span).clamp(2.0, 6.0);
                let (w0, w1) = (c.offset as i64, (c.offset + c.length) as i64);
                let first = (t0.max(w0) / period).max(0);
                let mut k = first;
                while k * period < t1.min(w1) {
                    for n in notes {
                        let ns = (k * period + (n.start * spb) as i64).max(w0);
                        let ne = (k * period + (n.end() * spb).min(period as f64) as i64).min(w1);
                        if ne <= ns {
                            continue;
                        }
                        let (x0, x1) = (x_of_content(ns), x_of_content(ne));
                        let y = body.bottom() - 3.0 - (n.pitch.saturating_sub(lo) as f32 + 1.0) / span * (body.height() - 6.0);
                        painter.rect_filled(Rect::from_min_max(pos2(x0, y), pos2(x1.max(x0 + 2.0), y + nh)), 1, color);
                    }
                    k += 1;
                }
            }
            ClipContent::Snippet { .. } => {}
        }
    }

    fn lane_interaction(&mut self, ui: &mut Ui, resp: &egui::Response, id: Id, rect: Rect, left: f32) {
        let alt = ui.input(|i| i.modifiers.alt);
        let hover = resp.hover_pos();

        // Snippets dragged in from the snippet columns become linked placements.
        if let Some(payload) = resp.dnd_hover_payload::<SnippetDrag>() {
            let ok = self.project.snippet_track(payload.0) == Some(id);
            ui.painter().rect_stroke(rect.shrink(1.0), 4, Stroke::new(2.0, if ok { ACCENT } else { REC }), StrokeKind::Inside);
        }
        if let Some(payload) = resp.dnd_release_payload::<SnippetDrag>() {
            if self.project.snippet_track(payload.0) == Some(id) {
                let x = ui.ctx().pointer_latest_pos().map_or(rect.left(), |p| p.x);
                let at = self.snap(self.sample_at(left, x), alt).max(0);
                self.set_scope(PlayScope::Song);
                self.place_snippet(payload.0, at);
            } else {
                self.notify("A snippet can only be placed on its own track");
            }
        }
        if self.drag.is_none()
            && let Some(p) = hover
            && let Some((_, kind)) = self.hit_test(id, rect, left, p)
        {
            ui.ctx().set_cursor_icon(match kind {
                DragKind::Move => CursorIcon::Grab,
                _ => CursorIcon::ResizeHorizontal,
            });
        }

        if resp.drag_started()
            && let Some(origin) = ui.input(|i| i.pointer.press_origin())
            && let Some((clip, kind)) = self.hit_test(id, rect, left, origin)
            && let Some((ti, ci)) = self.project.find_clip(clip)
        {
            self.history.checkpoint(&self.project);
            let orig = self.project.tracks[ti].clips[ci].clone();
            let grab = self.sample_at(left, origin.x) - orig.start;
            self.drag = Some(Drag { kind, clip, orig, grab });
            self.sel = crate::app::Selection { track: Some(id), clip: Some(clip), ..Default::default() };
        }
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.apply_drag(p, left, alt);
            if let Some(d) = &self.drag {
                ui.ctx().set_cursor_icon(if d.kind == DragKind::Move { CursorIcon::Grabbing } else { CursorIcon::ResizeHorizontal });
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }

        let hit = hover.and_then(|p| self.hit_test(id, rect, left, p));
        let snippet_of = |app: &Self, clip: Id| match app.project.find_clip(clip).map(|(t, c)| &app.project.tracks[t].clips[c].content) {
            Some(ClipContent::Snippet { snippet }) => Some(*snippet),
            _ => None,
        };
        if resp.clicked() {
            self.set_scope(PlayScope::Song);
            match hit {
                Some((clip, _)) => {
                    let snippet = snippet_of(self, clip).or(self.sel.snippet);
                    self.sel = crate::app::Selection { track: Some(id), clip: Some(clip), snippet, ..Default::default() };
                }
                None => {
                    self.sel = crate::app::Selection { track: Some(id), clip: None, snippet: self.sel.snippet, ..Default::default() };
                    if let Some(p) = hover {
                        let s = self.snap(self.sample_at(left, p.x), alt).max(0);
                        self.locate(s);
                    }
                }
            }
        }
        if resp.double_clicked() {
            match hit {
                Some((clip, _)) if snippet_of(self, clip).is_some() => {
                    self.sel.snippet = snippet_of(self, clip);
                    self.bottom = BottomTab::Takes;
                }
                Some((clip, _)) => {
                    self.sel.clip = Some(clip);
                    self.sel.take = None;
                    self.bottom = BottomTab::Editor;
                    self.roll.reset_for(clip);
                }
                None if self.project.track(id).is_some_and(|t| t.kind == TrackKind::Instrument) => {
                    if let Some(p) = hover {
                        self.create_midi_clip(id, self.sample_at(left, p.x));
                    }
                }
                None => {}
            }
        }
        if resp.secondary_clicked() {
            self.sel = crate::app::Selection { track: Some(id), clip: hit.map(|h| h.0), ..Default::default() };
        }
        let clip = self.sel.clip.filter(|_| self.sel.track == Some(id));
        let linked = clip.and_then(|c| snippet_of(self, c));
        resp.context_menu(|ui| match clip {
            Some(c) => {
                if let Some(snippet) = linked {
                    if ui.button(format!("{} Open snippet", icon::STACK)).on_hover_text("Show its takes").clicked() {
                        self.sel.snippet = Some(snippet);
                        self.bottom = BottomTab::Takes;
                        ui.close();
                    }
                    if ui.button(format!("{} Make unique", icon::LINK_SIMPLE)).on_hover_text("Give this placement its own copy of the snippet").clicked() {
                        self.history.checkpoint(&self.project);
                        self.sel.snippet = self.project.make_unique(c);
                        self.arrangement_dirty = true;
                        ui.close();
                    }
                    ui.separator();
                }
                if ui.button(format!("{} Split at playhead", icon::SCISSORS)).clicked() {
                    self.split_selected();
                    ui.close();
                }
                if ui.button(format!("{} Duplicate", icon::COPY)).clicked() {
                    self.duplicate_selected_clip();
                    ui.close();
                }
                let muted = self.project.clip_mut(c).map(|c| c.muted).unwrap_or(false);
                if ui.button(if muted { "Unmute clip" } else { "Mute clip" }).clicked() {
                    self.history.checkpoint(&self.project);
                    if let Some(cl) = self.project.clip_mut(c) {
                        cl.muted = !cl.muted;
                    }
                    self.arrangement_dirty = true;
                    ui.close();
                }
                if linked.is_none() && ui.button(format!("{} Edit", icon::PENCIL_SIMPLE)).clicked() {
                    self.sel.take = None;
                    self.bottom = BottomTab::Editor;
                    self.roll.reset_for(c);
                    ui.close();
                }
                ui.separator();
                if ui.button(format!("{} Delete", icon::TRASH)).clicked() {
                    self.delete_selected_clip();
                    ui.close();
                }
            }
            None => {
                let is_inst = self.project.track(id).is_some_and(|t| t.kind == TrackKind::Instrument);
                if is_inst && ui.button(format!("{} New MIDI clip at playhead", icon::PLUS)).clicked() {
                    self.create_midi_clip(id, self.song_position());
                    ui.close();
                }
                if ui.button("Import audio…").clicked() {
                    if let Some(files) = rfd::FileDialog::new().add_filter("WAV audio", &["wav", "WAV"]).pick_files() {
                        self.import_files(files, Some(id));
                    }
                    ui.close();
                }
            }
        });
    }

    fn create_midi_clip(&mut self, track: Id, at: i64) {
        let spb = self.project.samples_per_beat(self.rate());
        let bar = (spb * 4.0) as i64;
        let start = if self.project.snap { (at.max(0) / bar) * bar } else { at.max(0) };
        self.history.checkpoint(&self.project);
        let id = self.project.alloc_id();
        let Some(t) = self.project.track_mut(track) else { return };
        let name = format!("{} clip", t.name);
        t.clips.push(Clip {
            id,
            content: ClipContent::Midi { notes: vec![] },
            name,
            start,
            offset: 0,
            length: bar as u64,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
            muted: false,
        });
        self.sel = crate::app::Selection { track: Some(track), clip: Some(id), ..Default::default() };
        self.bottom = BottomTab::Editor;
        self.roll.reset_for(id);
        self.arrangement_dirty = true;
    }

    fn apply_drag(&mut self, p: Pos2, left: f32, alt: bool) {
        let Some(drag) = &self.drag else { return };
        let (kind, clip_id, orig, grab) = (drag.kind, drag.clip, drag.orig.clone(), drag.grab);
        let s = self.sample_at(left, p.x);
        let min_len = (self.rate() / 100) as i64;
        let source_len = match &orig.content {
            ClipContent::Audio { source } => self.sources.get(source).map(|d| d.len() as i64),
            ClipContent::Midi { .. } | ClipContent::Snippet { .. } => None,
        };
        let snapped = |v: i64| self.snap(v, alt);
        let new_start = snapped(s - grab).max(0);
        let trim_start = snapped(s).clamp((orig.start - orig.offset as i64).max(0), orig.end() - min_len);
        let max_end = source_len.map(|l| orig.start + l - orig.offset as i64).unwrap_or(i64::MAX);
        let trim_end = snapped(s).clamp(orig.start + min_len, max_end);

        // Moving to another track of a compatible kind.
        // Snippet placements stay on the track that owns the snippet.
        let target = if kind == DragKind::Move && !matches!(orig.content, ClipContent::Snippet { .. }) {
            let want = if matches!(orig.content, ClipContent::Audio { .. }) { TrackKind::Audio } else { TrackKind::Instrument };
            self.lane_rects
                .iter()
                .find(|(_, r)| p.y >= r.top() && p.y <= r.bottom())
                .map(|(id, _)| *id)
                .filter(|id| self.project.track(*id).is_some_and(|t| t.kind == want))
        } else {
            None
        };
        if let Some(target) = target
            && let Some((ti, ci)) = self.project.find_clip(clip_id)
            && self.project.tracks[ti].id != target
        {
            let c = self.project.tracks[ti].clips.remove(ci);
            if let Some(t) = self.project.track_mut(target) {
                t.clips.push(c);
            }
            self.sel.track = Some(target);
        }

        let Some(c) = self.project.clip_mut(clip_id) else { return };
        match kind {
            DragKind::Move => c.start = new_start,
            DragKind::TrimStart => {
                let delta = trim_start - orig.start;
                c.start = trim_start;
                c.offset = (orig.offset as i64 + delta).max(0) as u64;
                c.length = (orig.length as i64 - delta) as u64;
                c.fade_in = c.fade_in.min(c.length);
            }
            DragKind::TrimEnd => {
                c.length = (trim_end - c.start) as u64;
                c.fade_out = c.fade_out.min(c.length);
            }
            DragKind::FadeIn => c.fade_in = (s - c.start).clamp(0, c.length as i64 - c.fade_out as i64) as u64,
            DragKind::FadeOut => c.fade_out = (c.end() - s).clamp(0, c.length as i64 - c.fade_in as i64) as u64,
        }
        self.arrangement_dirty = true;
    }
}
