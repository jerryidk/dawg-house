//! Waveform editor for audio regions: timeline clips and snippet takes alike.
//!
//! The region is shown against its own musical grid (song tempo for clips,
//! the snippet's tempo and meter for takes). Drag the audio to slip it under
//! the bars, drag the edges to trim (clips), drag the top corners for fades.

use crate::app::StudioApp;
use crate::project::{ClipContent, Id, RegionOwner};
use crate::song::PlayScope;
use crate::theme::{self, BORDER, GRID, GRID_BAR, PLAYHEAD, TEXT, TEXT_DIM};
use crate::util::{db_to_gain, gain_to_db};
use eframe::egui::{self, Color32, CursorIcon, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as icon;

const RULER_H: f32 = 18.0;
const EDGE: f32 = 6.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WaveDragKind {
    Slip,
    TrimStart,
    TrimEnd,
    FadeIn,
    FadeOut,
}

struct WaveDrag {
    kind: WaveDragKind,
    /// Region time (samples) under the pointer when the drag started.
    grab: f64,
    offset: u64,
    start: i64,
    length: u64,
}

pub struct WaveState {
    target: Option<Id>,
    /// Pixels per beat.
    pub ppb: f32,
    /// Region time (beats) at the left edge; negative shows audio before it.
    pub scroll: f64,
    drag: Option<WaveDrag>,
    needs_fit: bool,
    /// Drawing area from the last frame (used by UI tests).
    pub area: Rect,
}

impl Default for WaveState {
    fn default() -> Self {
        WaveState { target: None, ppb: 40.0, scroll: 0.0, drag: None, needs_fit: true, area: Rect::NOTHING }
    }
}

impl WaveState {
    pub fn reset_for(&mut self, id: Id) {
        if self.target != Some(id) {
            self.target = Some(id);
            self.drag = None;
            self.needs_fit = true;
        }
    }
}

/// Grid spacing in beats so lines are at least 12 px apart.
fn grid_step(ppb: f32) -> f64 {
    let mut step = 1.0 / 16.0;
    while step * (ppb as f64) < 12.0 {
        step *= 2.0;
    }
    step
}

impl StudioApp {
    pub fn wave_editor(&mut self, ui: &mut Ui, id: Id) {
        self.wave.reset_for(id);
        let rate = self.rate();
        let Some((ctx, region)) = self.project.region_mut(id, rate) else { return };
        let f = region.fields();
        let ClipContent::Audio { source } = *f.content else { return };
        let (name, offset, gain, fade_in, fade_out) = (f.name.clone(), *f.offset, *f.gain, *f.fade_in, *f.fade_out);
        let Some(data) = self.sources.get(&source).cloned() else {
            ui.label(RichText::new("Audio file missing").color(theme::REC));
            return;
        };
        let (track, is_clip) = match ctx.owner {
            RegionOwner::Clip { track, .. } => (track, true),
            RegionOwner::Take { track, .. } => (track, false),
        };
        let color = self.project.track(track).map_or(theme::ACCENT, |t| theme::rgb(t.color));
        let spb = ctx.samples_per_beat;
        let bpb = ctx.beats_per_bar.max(1) as f64;
        let len = ctx.length as f64;
        let src_len = data.len() as f64;

        // ---- Toolbar
        let mut new_name = name.clone();
        let mut gain_db = gain_to_db(gain).max(-40.0);
        let mut gain_changed = false;
        let mut reset_offset = false;
        let mut zoom = 1.0f32;
        let mut fit = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::WAVEFORM).color(color));
            let r = ui.add(egui::TextEdit::singleline(&mut new_name).desired_width(150.0));
            if r.gained_focus() {
                self.history.checkpoint(&self.project);
            }
            ui.label(RichText::new(if is_clip { "clip" } else { "snippet take" }).small().color(TEXT_DIM));
            ui.separator();
            ui.label(RichText::new("Gain").small().color(TEXT_DIM));
            let r = ui.add(egui::Slider::new(&mut gain_db, -40.0..=12.0).suffix(" dB").fixed_decimals(1));
            if r.drag_started() {
                self.history.checkpoint(&self.project);
            }
            gain_changed = r.changed();
            ui.separator();
            ui.label(RichText::new(format!("Offset {:.3} s", offset as f64 / rate as f64)).small().color(TEXT_DIM));
            if ui.small_button("Reset").on_hover_text("Start the region at the beginning of its audio").clicked() {
                reset_offset = true;
            }
            ui.separator();
            if theme::icon_button(ui, icon::MAGNIFYING_GLASS_MINUS, "Zoom out").clicked() {
                zoom = 0.8;
            }
            if theme::icon_button(ui, icon::MAGNIFYING_GLASS_PLUS, "Zoom in").clicked() {
                zoom = 1.25;
            }
            if ui.small_button("Fit").on_hover_text("Fit the region in view").clicked() {
                fit = true;
            }
            let hint = if is_clip {
                "drag: slip against bars · edges: trim · top corners: fades · Alt: no snap · Ctrl+wheel: zoom"
            } else {
                "drag: slip against bars · top corners: fades · Alt: no snap · Ctrl+wheel: zoom"
            };
            ui.label(RichText::new(hint).small().color(TEXT_DIM));
        });

        // ---- Layout
        let area = ui.available_rect_before_wrap();
        if area.height() < 60.0 || area.width() < 100.0 {
            return;
        }
        self.wave.area = area;
        if self.wave.needs_fit || fit {
            self.wave.needs_fit = false;
            let beats = (len / spb).max(1.0) as f32;
            self.wave.ppb = ((area.width() - 60.0) / beats).clamp(1.0, 2000.0);
            self.wave.scroll = -(30.0 / self.wave.ppb) as f64;
        }
        if zoom != 1.0 {
            let mid = self.wave.scroll + (area.width() / 2.0 / self.wave.ppb) as f64;
            self.wave.ppb = (self.wave.ppb * zoom).clamp(1.0, 2000.0);
            self.wave.scroll = mid - (area.width() / 2.0 / self.wave.ppb) as f64;
        }
        let resp = ui.allocate_rect(area, Sense::click_and_drag());
        let (ppb, scroll) = (self.wave.ppb, self.wave.scroll);
        let x_of = |t: f64| area.left() + ((t / spb - scroll) * ppb as f64) as f32;
        let t_at = |x: f32| ((x - area.left()) / ppb) as f64 * spb + scroll * spb;
        let ruler = Rect::from_min_max(area.min, pos2(area.right(), area.top() + RULER_H));
        let wave = Rect::from_min_max(pos2(area.left(), ruler.bottom()), area.max);

        // ---- Grid and ruler
        let p = ui.painter_at(area);
        p.rect_filled(area, 0, theme::LANE);
        p.rect_filled(ruler, 0, theme::PANEL);
        let step = grid_step(ppb);
        let mut b = (scroll / step).floor() * step;
        while x_of(b * spb) <= area.right() {
            let x = x_of(b * spb);
            let bar = (b / bpb).fract().abs() < 1e-9;
            let beat = b.fract().abs() < 1e-9;
            let c = if bar { GRID_BAR.gamma_multiply(1.6) } else if beat { GRID_BAR } else { GRID };
            p.line_segment([pos2(x, wave.top()), pos2(x, wave.bottom())], Stroke::new(1.0, c));
            if bar && (bpb * ppb as f64) > 28.0 {
                let n = (b / bpb).round() as i64;
                p.text(pos2(x + 3.0, ruler.top() + 2.0), egui::Align2::LEFT_TOP, format!("{}", n + 1), FontId::proportional(10.5), TEXT_DIM);
                p.line_segment([pos2(x, ruler.bottom() - 6.0), pos2(x, ruler.bottom())], Stroke::new(1.0, TEXT_DIM));
            }
            b += step;
        }

        // ---- Region and waveform
        let (rx0, rx1) = (x_of(0.0), x_of(len));
        let region_rect = Rect::from_min_max(pos2(rx0, wave.top()), pos2(rx1, wave.bottom()));
        p.rect_filled(region_rect, 0, color.gamma_multiply(0.10));
        let channels = data.channels.len().max(1);
        let lane_h = wave.height() / channels as f32;
        let amp = gain.min(4.0);
        for px in wave.left() as i32..wave.right() as i32 {
            let (t0, t1) = (t_at(px as f32), t_at(px as f32 + 1.0));
            let (a, b) = (offset as f64 + t0, offset as f64 + t1);
            if b <= 0.0 || a >= src_len {
                continue;
            }
            let (a, b) = (a.max(0.0) as usize, (b.min(src_len) as usize).max(a.max(0.0) as usize + 1));
            let inside = t0 >= 0.0 && t0 < len;
            let c = if inside { color } else { Color32::from_gray(85) };
            for ch in 0..channels {
                // Per-channel detail when zoomed in; the overview peaks otherwise.
                let (lo, hi) = if b - a <= 4096 {
                    let s = data.channels[ch].get(a..b.min(data.channels[ch].len())).unwrap_or(&[]);
                    s.iter().fold((0f32, 0f32), |(lo, hi), &v| (lo.min(v), hi.max(v)))
                } else {
                    data.range_peak(a, b)
                };
                let mid = wave.top() + lane_h * (ch as f32 + 0.5);
                let h = lane_h * 0.46 * amp;
                p.line_segment([pos2(px as f32, mid - hi * h), pos2(px as f32, mid - lo * h + 1.0)], Stroke::new(1.0, c));
            }
        }
        if channels == 2 {
            p.line_segment([pos2(wave.left(), wave.center().y), pos2(wave.right(), wave.center().y)], Stroke::new(1.0, BORDER));
        }
        // Region edges, fades and handles.
        for x in [rx0, rx1] {
            p.line_segment([pos2(x, wave.top()), pos2(x, wave.bottom())], Stroke::new(1.5, TEXT));
        }
        if !is_clip {
            p.text(pos2(rx1 - 6.0, wave.bottom() - 4.0), egui::Align2::RIGHT_BOTTOM, format!("{} loops here", icon::REPEAT), FontId::proportional(10.5), TEXT_DIM);
        }
        let (fx_in, fx_out) = (x_of(fade_in as f64), x_of(len - fade_out as f64));
        let fade = Stroke::new(1.2, TEXT.gamma_multiply(0.8));
        if fade_in > 0 {
            p.line_segment([pos2(rx0, wave.bottom()), pos2(fx_in, wave.top())], fade);
        }
        if fade_out > 0 {
            p.line_segment([pos2(fx_out, wave.top()), pos2(rx1, wave.bottom())], fade);
        }
        for x in [fx_in, fx_out] {
            p.rect_filled(Rect::from_center_size(pos2(x, wave.top() + 4.0), vec2(8.0, 8.0)), 2, TEXT);
        }
        // Playhead (region time).
        let pos = self.position();
        let ph = match (ctx.owner, self.scope) {
            (RegionOwner::Take { snippet, .. }, PlayScope::Snippet(s)) if s == snippet && pos >= 0 => Some((pos as f64) % len.max(1.0)),
            (RegionOwner::Clip { start, .. }, PlayScope::Song) => Some((pos - start) as f64),
            _ => None,
        };
        if let Some(t) = ph {
            let x = x_of(t);
            if x >= area.left() && x <= area.right() {
                p.line_segment([pos2(x, area.top()), pos2(x, area.bottom())], Stroke::new(1.0, PLAYHEAD));
            }
        }

        // ---- Interaction
        let zone = |pt: Pos2| -> WaveDragKind {
            if pt.y < wave.top() + 14.0 {
                if (pt.x - fx_in).abs() < 7.0 {
                    return WaveDragKind::FadeIn;
                }
                if (pt.x - fx_out).abs() < 7.0 {
                    return WaveDragKind::FadeOut;
                }
            }
            if is_clip && (pt.x - rx0).abs() < EDGE {
                WaveDragKind::TrimStart
            } else if is_clip && (pt.x - rx1).abs() < EDGE {
                WaveDragKind::TrimEnd
            } else {
                WaveDragKind::Slip
            }
        };
        if let Some(hp) = resp.hover_pos() {
            ui.ctx().set_cursor_icon(match self.wave.drag.as_ref().map(|d| d.kind).unwrap_or(zone(hp)) {
                WaveDragKind::Slip if self.wave.drag.is_some() => CursorIcon::Grabbing,
                WaveDragKind::Slip => CursorIcon::Grab,
                _ => CursorIcon::ResizeHorizontal,
            });
        }
        // Wheel: Ctrl zooms around the pointer, otherwise scrolls.
        if let Some(hp) = resp.hover_pos() {
            let (d, z) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            if z != 1.0 {
                let anchor = t_at(hp.x) / spb;
                self.wave.ppb = (self.wave.ppb * z).clamp(1.0, 2000.0);
                self.wave.scroll = anchor - ((hp.x - area.left()) / self.wave.ppb) as f64;
            }
            let dx = if d.x != 0.0 { d.x } else { d.y };
            self.wave.scroll -= (dx / self.wave.ppb) as f64;
        }

        let alt = ui.input(|i| i.modifiers.alt);
        let snapping = self.project.snap && !alt;
        let snap = |v: f64| {
            if !snapping {
                v
            } else {
                let g = grid_step(ppb) * spb;
                (v / g).round() * g
            }
        };
        if resp.drag_started()
            && let Some(origin) = ui.input(|i| i.pointer.press_origin())
        {
            self.history.checkpoint(&self.project);
            let (start, length) = self.project.clip_mut(id).map_or((0, ctx.length), |c| (c.start, c.length));
            self.wave.drag = Some(WaveDrag { kind: zone(origin), grab: t_at(origin.x), offset, start, length });
        }
        let mut changed = false;
        if resp.dragged()
            && let (Some(pt), Some(d)) = (resp.interact_pointer_pos(), &self.wave.drag)
        {
            let (kind, grab, o_off, o_start, o_len) = (d.kind, d.grab, d.offset as f64, d.start, d.length as f64);
            let t = t_at(pt.x);
            let delta = snap(t - grab);
            let min_len = rate as f64 / 100.0;
            match kind {
                WaveDragKind::Slip => {
                    // Moving the audio right means starting earlier in the source.
                    let off = (o_off - delta).clamp(0.0, src_len) as u64;
                    if let Some((_, r)) = self.project.region_mut(id, rate) {
                        *r.fields().offset = off;
                    }
                }
                WaveDragKind::TrimStart => {
                    let d = delta.clamp(-o_off, o_len - min_len);
                    if let Some(c) = self.project.clip_mut(id) {
                        c.start = o_start + d as i64;
                        c.offset = (o_off + d) as u64;
                        c.length = (o_len - d) as u64;
                        c.fade_in = c.fade_in.min(c.length);
                    }
                }
                WaveDragKind::TrimEnd => {
                    let new_len = (o_len + delta).clamp(min_len, (src_len - o_off).max(min_len));
                    if let Some(c) = self.project.clip_mut(id) {
                        c.length = new_len as u64;
                        c.fade_out = c.fade_out.min(c.length);
                    }
                }
                WaveDragKind::FadeIn => {
                    if let Some((_, r)) = self.project.region_mut(id, rate) {
                        let f = r.fields();
                        *f.fade_in = t.clamp(0.0, len - *f.fade_out as f64) as u64;
                    }
                }
                WaveDragKind::FadeOut => {
                    if let Some((_, r)) = self.project.region_mut(id, rate) {
                        let f = r.fields();
                        *f.fade_out = (len - t).clamp(0.0, len - *f.fade_in as f64) as u64;
                    }
                }
            }
            changed = true;
        }
        if resp.drag_stopped() {
            self.wave.drag = None;
        }

        // ---- Toolbar edits
        if reset_offset && offset != 0 {
            self.history.checkpoint(&self.project);
        }
        if let Some((_, r)) = self.project.region_mut(id, rate) {
            let f = r.fields();
            if new_name != name {
                *f.name = new_name;
            }
            if gain_changed {
                *f.gain = db_to_gain(gain_db);
                changed = true;
            }
            if reset_offset && *f.offset != 0 {
                *f.offset = 0;
                changed = true;
            }
        }
        if changed {
            self.arrangement_dirty = true;
        }
    }
}
