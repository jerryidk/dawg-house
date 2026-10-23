//! Snippet columns (one per track) and the Takes tab of the detail panel.

use crate::app::{BottomTab, StudioApp};
use crate::audio::range_peak;
use crate::project::{ClipContent, Id, Snippet, TrackKind};
use crate::song::PlayScope;
use crate::theme::{self, ACCENT, BORDER, MONITOR, PANEL, PANEL_HI, REC, TEXT, TEXT_DIM};
use eframe::egui::{self, Color32, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use egui_phosphor::regular as icon;

const COLUMN_W: f32 = 220.0;
const CARD_H: f32 = 70.0;

/// Drag-and-drop payload: a snippet being dragged onto the timeline.
#[derive(Clone, Copy)]
pub struct SnippetDrag(pub Id);

enum Action {
    Select(Id),
    Play(Id),
    Record(Id),
    OpenTakes(Id),
    New(Id),
    Duplicate(Id),
    Delete(Id),
    PlaceAtPlayhead(Id),
}

impl StudioApp {
    pub fn snippet_panel(&mut self, ui: &mut Ui) {
        let mut folded = self.folds.snippets;
        StudioApp::fold_header(ui, &mut folded, &format!("{} Snippets", icon::COLUMNS), |ui| {
            ui.label(RichText::new("click: loop · ⏺: record takes · drag onto the timeline to place").small().color(TEXT_DIM));
        });
        self.folds.snippets = folded;
        if folded {
            return;
        }
        ui.add_space(4.0);
        let mut actions = vec![];
        self.snippet_rects.clear();
        let ids: Vec<Id> = self.project.tracks.iter().map(|t| t.id).collect();
        egui::ScrollArea::both().id_salt("snippet_cols").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for tid in ids {
                    self.snippet_column(ui, tid, &mut actions);
                }
                ui.vertical(|ui| {
                    if ui.button(format!("{} Audio track", icon::PLUS)).clicked() {
                        self.add_track(TrackKind::Audio);
                    }
                    if ui.button(format!("{} Instrument track", icon::PLUS)).clicked() {
                        self.add_track(TrackKind::Instrument);
                    }
                });
            });
        });
        for a in actions {
            self.apply(a);
        }
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::Select(id) => self.select_snippet(id),
            Action::Play(id) => {
                self.select_snippet(id);
                if !self.playing() {
                    self.toggle_play();
                }
            }
            Action::Record(id) => {
                if self.recording() {
                    self.toggle_record();
                } else {
                    self.select_snippet(id);
                    self.scope = PlayScope::Snippet(id);
                    self.toggle_record();
                }
            }
            Action::OpenTakes(id) => {
                self.select_snippet(id);
                self.bottom = BottomTab::Takes;
                self.folds.detail = false;
            }
            Action::New(track) => {
                self.history.checkpoint(&self.project);
                if let Some(id) = self.project.new_snippet(track) {
                    self.select_snippet(id);
                }
            }
            Action::Duplicate(id) => {
                let Some((ti, si)) = self.project.find_snippet(id) else { return };
                self.history.checkpoint(&self.project);
                let mut copy = self.project.tracks[ti].snippets[si].clone();
                copy.id = self.project.alloc_id();
                copy.name = format!("{} copy", copy.name);
                let mut active = None;
                for k in &mut copy.takes {
                    let nid = self.project.alloc_id();
                    if copy.active == Some(k.id) {
                        active = Some(nid);
                    }
                    k.id = nid;
                }
                copy.active = active;
                self.project.tracks[ti].snippets.insert(si + 1, copy);
            }
            Action::Delete(id) => {
                self.history.checkpoint(&self.project);
                self.project.delete_snippet(id);
                if self.sel.snippet == Some(id) {
                    self.sel.snippet = None;
                }
                if self.scope == PlayScope::Snippet(id) {
                    self.set_scope(PlayScope::Song);
                }
                self.arrangement_dirty = true;
            }
            Action::PlaceAtPlayhead(id) => {
                let at = if self.scope == PlayScope::Song { self.position() } else { 0 };
                self.place_snippet(id, at);
            }
        }
    }

    /// Place a snippet on its track's lane (linked placement).
    pub fn place_snippet(&mut self, snippet: Id, at: i64) {
        self.history.checkpoint(&self.project);
        let rate = self.rate();
        if let Some(clip) = self.project.place_snippet(snippet, at, rate) {
            self.sel.clip = Some(clip);
            self.sel.track = self.project.snippet_track(snippet);
            self.arrangement_dirty = true;
        }
    }

    fn snippet_column(&mut self, ui: &mut Ui, tid: Id, actions: &mut Vec<Action>) {
        let Some(t) = self.project.track(tid) else { return };
        let color = theme::rgb(t.color);
        let height = ui.available_height().max(120.0);
        egui::Frame::new().fill(PANEL).corner_radius(8).inner_margin(8).show(ui, |ui| {
            ui.set_width(COLUMN_W);
            ui.set_min_height(height - 16.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            // Header: name and recording controls (mirrors the timeline header).
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(4.0, 18.0), Sense::hover());
                ui.painter().rect_filled(r, 2, color);
                let t = self.project.track_mut(tid).unwrap();
                let kind = if t.kind == TrackKind::Audio { icon::MICROPHONE } else { icon::PIANO_KEYS };
                ui.label(RichText::new(format!("{kind} {}", t.name)).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::toggle(ui, &mut t.monitor, icon::HEADPHONES, MONITOR, "Monitor input");
                    theme::toggle(ui, &mut t.armed, icon::RECORD, REC, "Arm for recording");
                });
            });
            ui.add_space(4.0);
            let snippets: Vec<Id> = self.project.track(tid).map(|t| t.snippets.iter().map(|s| s.id).collect()).unwrap_or_default();
            for sid in snippets {
                let r = self.snippet_card(ui, tid, sid, color, actions);
                self.snippet_rects.push((sid, r));
                ui.add_space(4.0);
            }
            if ui.add(egui::Button::new(format!("{} New snippet", icon::PLUS)).min_size(vec2(COLUMN_W, 26.0))).clicked() {
                actions.push(Action::New(tid));
            }
            });
        });
    }

    fn snippet_card(&self, ui: &mut Ui, tid: Id, sid: Id, color: Color32, actions: &mut Vec<Action>) -> Rect {
        let Some(s) = self.project.snippet(sid) else { return Rect::NOTHING };
        let cued = self.project.track(tid).is_some_and(|t| t.cued == Some(sid));
        let selected = self.sel.snippet == Some(sid);
        let looping = self.scope == PlayScope::Snippet(sid);
        let recording = self.recording_snippet == Some(sid) && self.recording();
        let (rect, resp) = ui.allocate_exact_size(vec2(COLUMN_W, CARD_H), Sense::click_and_drag());
        let p = ui.painter();
        p.rect_filled(rect, 6, if cued { color.gamma_multiply(0.22) } else { PANEL_HI });
        let stroke = if recording {
            Stroke::new(2.0, REC)
        } else if selected {
            Stroke::new(1.5, TEXT)
        } else if cued {
            Stroke::new(1.0, color)
        } else {
            Stroke::new(1.0, BORDER)
        };
        p.rect_stroke(rect, 6, stroke, StrokeKind::Inside);
        p.text(rect.left_top() + vec2(8.0, 6.0), egui::Align2::LEFT_TOP, &s.name, FontId::proportional(13.0), TEXT);
        p.text(
            rect.left_top() + vec2(8.0, 24.0),
            egui::Align2::LEFT_TOP,
            format!("{} bars · {:.0} · {}", s.bars, s.bpm, s.meter()),
            FontId::proportional(11.0),
            TEXT_DIM,
        );
        let takes = match s.takes.len() {
            0 => "empty".to_string(),
            1 => "1 take".to_string(),
            n => format!("{n} takes"),
        };
        p.text(rect.right_top() + vec2(-8.0, 26.0), egui::Align2::RIGHT_TOP, takes, FontId::proportional(11.0), TEXT_DIM);
        let preview = Rect::from_min_max(pos2(rect.left() + 8.0, rect.bottom() - 24.0), pos2(rect.right() - 8.0, rect.bottom() - 6.0));
        self.draw_take_preview(p, preview, s, color);

        // Loop progress while this snippet plays.
        if looping && self.playing() {
            let len = s.length(self.rate()).max(1) as f32;
            let pos = self.position();
            if pos >= 0 {
                let x = preview.left() + preview.width() * ((pos as f32 % len) / len);
                p.line_segment([pos2(x, preview.top()), pos2(x, preview.bottom())], Stroke::new(1.5, if recording { REC } else { ACCENT }));
            }
            if recording {
                let pass = (pos.max(0) as f32 / len) as i64 + 1;
                let label = if pos < 0 { "count-in".to_string() } else { format!("● take {}", s.takes.len() as i64 + pass) };
                p.text(rect.right_top() + vec2(-8.0, 8.0), egui::Align2::RIGHT_TOP, label, FontId::proportional(11.0), REC);
            }
        }

        // Buttons on top of the card (interact-only so the layout cursor stays put).
        if !recording {
            let b = Rect::from_min_size(rect.right_top() + vec2(-58.0, 3.0), vec2(26.0, 20.0));
            if card_button(ui, b, ("play", sid), icon::PLAY, TEXT, "Loop this snippet") {
                actions.push(Action::Play(sid));
            }
        }
        let b = Rect::from_min_size(rect.right_top() + vec2(-30.0, 3.0), vec2(26.0, 20.0));
        let rec_tip = if recording { "Stop recording" } else { "Record takes into this snippet (loops; every pass is a take)" };
        if card_button(ui, b, ("rec", sid), icon::RECORD, REC, rec_tip) {
            actions.push(Action::Record(sid));
        }

        if resp.clicked() {
            actions.push(Action::Select(sid));
        }
        if resp.double_clicked() {
            actions.push(Action::OpenTakes(sid));
        }
        if resp.drag_started() {
            egui::DragAndDrop::set_payload(ui.ctx(), SnippetDrag(sid));
        }
        if resp.dragged()
            && let Some(pp) = ui.ctx().pointer_latest_pos()
        {
            let layer = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("snippet_dnd")));
            let galley = layer.layout_no_wrap(format!("{} {}", icon::LINK, s.name), FontId::proportional(12.5), TEXT);
            let r = Rect::from_min_size(pp + vec2(14.0, 4.0), galley.size());
            layer.rect_filled(r.expand(4.0), 4, color.gamma_multiply(0.6));
            layer.galley(r.min, galley, TEXT);
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        resp.context_menu(|ui| {
            if ui.button(format!("{} Place at playhead", icon::LINK)).clicked() {
                actions.push(Action::PlaceAtPlayhead(sid));
                ui.close();
            }
            if ui.button(format!("{} Takes…", icon::STACK)).clicked() {
                actions.push(Action::OpenTakes(sid));
                ui.close();
            }
            if ui.button(format!("{} Duplicate", icon::COPY)).clicked() {
                actions.push(Action::Duplicate(sid));
                ui.close();
            }
            ui.separator();
            if ui.button(format!("{} Delete (and its placements)", icon::TRASH)).clicked() {
                actions.push(Action::Delete(sid));
                ui.close();
            }
        });
        rect
    }

    /// Small drawing of a snippet's active take (one loop).
    pub fn draw_take_preview(&self, p: &egui::Painter, r: Rect, s: &Snippet, color: Color32) {
        p.rect_filled(r, 3, theme::BG);
        let Some(take) = s.active_take() else {
            p.text(r.center(), egui::Align2::CENTER_CENTER, "no takes yet", FontId::proportional(10.0), TEXT_DIM);
            return;
        };
        let len = s.length(self.rate()).max(1) as f32;
        match &take.content {
            ClipContent::Audio { source } => {
                let Some(d) = self.sources.get(source) else { return };
                let mid = r.center().y;
                for px in 0..r.width() as i32 {
                    let a = take.offset as usize + (px as f32 / r.width() * len) as usize;
                    let b = take.offset as usize + ((px + 1) as f32 / r.width() * len) as usize;
                    let (lo, hi) = range_peak(&d.channels, &d.peaks, a, b.max(a + 1));
                    let x = r.left() + px as f32;
                    p.line_segment([pos2(x, mid - hi * r.height() * 0.5), pos2(x, mid - lo * r.height() * 0.5 + 1.0)], Stroke::new(1.0, color));
                }
            }
            ClipContent::Midi { notes } => {
                let spb = s.samples_per_beat(self.rate());
                let (lo, hi) = notes.iter().fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.pitch), hi.max(n.pitch)));
                let span = (hi.saturating_sub(lo) as f32 + 1.0).max(8.0);
                for n in notes {
                    let x0 = r.left() + (n.start * spb) as f32 / len * r.width();
                    let x1 = r.left() + (n.end() * spb) as f32 / len * r.width();
                    let y = r.bottom() - 2.0 - (n.pitch.saturating_sub(lo) as f32 + 1.0) / span * (r.height() - 4.0);
                    p.rect_filled(Rect::from_min_max(pos2(x0, y), pos2(x1.min(r.right()).max(x0 + 2.0), y + 2.0)), 0, color);
                }
            }
            ClipContent::Snippet { .. } => {}
        }
    }

    // -----------------------------------------------------------------------
    // Takes tab

    pub fn snippet_detail(&mut self, ui: &mut Ui, id: Id) {
        let rate = self.rate();
        let kind = self.project.snippet_track(id).and_then(|t| self.project.track(t)).map(|t| t.kind);
        let placements = self
            .project
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .filter(|c| matches!(c.content, ClipContent::Snippet { snippet } if snippet == id))
            .count();
        let mut dirty = false;
        let mut set_active = None;
        let mut delete = None;
        let mut edit_notes = None;
        let mut action = None;
        let mut empty_take = false;
        let Some(s) = self.project.snippet_mut(id) else { return };
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(300.0);
                egui::Grid::new("snippet_settings").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Name");
                    dirty |= ui.text_edit_singleline(&mut s.name).changed();
                    ui.end_row();
                    ui.label("Tempo");
                    dirty |= ui.add(egui::DragValue::new(&mut s.bpm).range(30.0..=300.0).speed(0.2).fixed_decimals(1).suffix(" BPM")).changed();
                    ui.end_row();
                    ui.label("Meter");
                    ui.horizontal(|ui| {
                        dirty |= ui.add(egui::DragValue::new(&mut s.beats_per_bar).range(1..=16)).changed();
                        ui.label("/");
                        egui::ComboBox::from_id_salt("beat_unit").selected_text(s.beat_unit.to_string()).width(48.0).show_ui(ui, |ui| {
                            for u in [2, 4, 8, 16] {
                                dirty |= ui.selectable_value(&mut s.beat_unit, u, u.to_string()).changed();
                            }
                        });
                    });
                    ui.end_row();
                    ui.label("Length");
                    dirty |= ui.add(egui::DragValue::new(&mut s.bars).range(1..=128).suffix(" bars")).changed();
                    ui.end_row();
                });
                ui.label(
                    RichText::new(format!(
                        "{:.2} s per loop · placed {placements}× on the timeline (linked)",
                        s.length(rate) as f64 / rate as f64
                    ))
                    .small()
                    .color(TEXT_DIM),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.button(format!("{} Loop", icon::PLAY)).clicked() {
                        action = Some(Action::Play(id));
                    }
                    if ui.button(RichText::new(format!("{} Record takes", icon::RECORD)).color(REC)).clicked() {
                        action = Some(Action::Record(id));
                    }
                    if ui.button(format!("{} Place at playhead", icon::LINK)).clicked() {
                        action = Some(Action::PlaceAtPlayhead(id));
                    }
                    if kind == Some(TrackKind::Instrument) && ui.button(format!("{} Empty MIDI take", icon::PLUS)).on_hover_text("Draw notes in the piano roll instead of recording").clicked() {
                        empty_take = true;
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                theme::section_label(ui, "Takes — the active take plays wherever the snippet is placed");
                if s.takes.is_empty() {
                    ui.label(RichText::new("No takes yet. Press ⏺ Record takes: after a one-bar count-in the snippet loops and every pass becomes a take.").color(TEXT_DIM));
                }
                egui::ScrollArea::vertical().id_salt("takes").auto_shrink([false, false]).show(ui, |ui| {
                    for k in s.takes.iter_mut().rev() {
                        ui.horizontal(|ui| {
                            let active = s.active == Some(k.id);
                            if ui.radio(active, "").on_hover_text("Use this take").clicked() && !active {
                                set_active = Some(k.id);
                            }
                            ui.add(egui::TextEdit::singleline(&mut k.name).desired_width(120.0));
                            let info = match &k.content {
                                ClipContent::Audio { .. } => format!("{} audio", icon::WAVEFORM),
                                ClipContent::Midi { notes } => format!("{} {} notes", icon::MUSIC_NOTES, notes.len()),
                                ClipContent::Snippet { .. } => String::new(),
                            };
                            ui.label(RichText::new(info).small().color(TEXT_DIM));
                            if matches!(k.content, ClipContent::Midi { .. }) && ui.button(format!("{} Edit", icon::PENCIL_SIMPLE)).clicked() {
                                edit_notes = Some(k.id);
                            }
                            if theme::icon_button(ui, icon::TRASH, "Delete take").clicked() {
                                delete = Some(k.id);
                            }
                        });
                    }
                });
            });
        });

        if let Some(take) = set_active {
            self.history.checkpoint(&self.project);
            if let Some(s) = self.project.snippet_mut(id) {
                s.active = Some(take);
            }
            dirty = true;
        }
        if let Some(take) = delete {
            self.history.checkpoint(&self.project);
            self.project.delete_take(take);
            dirty = true;
        }
        if let Some(take) = edit_notes {
            self.sel.take = Some(take);
            self.bottom = BottomTab::Editor;
            self.roll.reset_for(take);
        }
        if empty_take {
            self.history.checkpoint(&self.project);
            let take = self.project.alloc_id();
            if let Some(s) = self.project.snippet_mut(id) {
                let name = format!("Take {}", s.takes.len() + 1);
                s.takes.push(crate::project::Take { id: take, name, content: ClipContent::Midi { notes: vec![] }, offset: 0 });
                s.active = Some(take);
            }
            self.sel.take = Some(take);
            self.bottom = BottomTab::Editor;
            self.roll.reset_for(take);
            dirty = true;
        }
        if dirty {
            self.arrangement_dirty = true;
        }
        if let Some(a) = action {
            self.apply(a);
        }
    }
}

/// Small icon button drawn over a card.
fn card_button(ui: &mut Ui, rect: Rect, id: (&str, Id), glyph: &str, color: Color32, tip: &str) -> bool {
    let resp = ui.interact(rect, ui.id().with(id), Sense::click()).on_hover_text(tip);
    if resp.hovered() {
        ui.painter().rect_filled(rect, 4, Color32::from_white_alpha(18));
    }
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, FontId::proportional(13.0), color);
    resp.clicked()
}
