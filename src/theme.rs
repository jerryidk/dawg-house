//! Visual style and small shared widgets.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Rect, Response, Sense, Stroke, TextStyle, Ui, Vec2, pos2,
};

pub const BG: Color32 = Color32::from_rgb(0x15, 0x17, 0x1c);
pub const PANEL: Color32 = Color32::from_rgb(0x1d, 0x20, 0x27);
pub const PANEL_HI: Color32 = Color32::from_rgb(0x26, 0x2a, 0x33);
pub const LANE: Color32 = Color32::from_rgb(0x19, 0x1c, 0x22);
pub const LANE_ALT: Color32 = Color32::from_rgb(0x1c, 0x1f, 0x26);
pub const BORDER: Color32 = Color32::from_rgb(0x2e, 0x33, 0x3d);
pub const GRID: Color32 = Color32::from_rgb(0x24, 0x28, 0x30);
pub const GRID_BAR: Color32 = Color32::from_rgb(0x30, 0x35, 0x40);
pub const TEXT: Color32 = Color32::from_rgb(0xd8, 0xdc, 0xe4);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8b, 0x92, 0xa1);
pub const ACCENT: Color32 = Color32::from_rgb(0x5e, 0x9c, 0xff);
pub const REC: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);
pub const MONITOR: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
pub const MUTE: Color32 = Color32::from_rgb(0xea, 0xb3, 0x08);
pub const SOLO: Color32 = Color32::from_rgb(0x22, 0xc5, 0x5e);
pub const PLAYHEAD: Color32 = Color32::from_rgb(0xf8, 0xfa, 0xfc);

pub fn apply(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(6.0, 5.0);
        style.spacing.button_padding = Vec2::new(8.0, 3.0);
        style.spacing.interact_size.y = 22.0;
        style.spacing.slider_width = 120.0;
        style.spacing.window_margin = Margin::same(12);

        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = PANEL;
        v.window_fill = PANEL;
        v.extreme_bg_color = BG;
        v.faint_bg_color = PANEL_HI;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.selection.bg_fill = ACCENT.gamma_multiply(0.55);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;
        v.override_text_color = Some(TEXT);
        v.slider_trailing_fill = true;
        let r = CornerRadius::same(5);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = r;
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.inactive.bg_fill = PANEL_HI;
        v.widgets.inactive.weak_bg_fill = PANEL_HI;
        v.widgets.inactive.bg_stroke = Stroke::NONE;
        v.widgets.hovered.bg_fill = Color32::from_rgb(0x31, 0x36, 0x41);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x31, 0x36, 0x41);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3d, 0x44, 0x52));
        v.widgets.active.bg_fill = Color32::from_rgb(0x3a, 0x41, 0x4e);
        v.widgets.active.weak_bg_fill = Color32::from_rgb(0x3a, 0x41, 0x4e);
    });
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Small square toggle button (arm / monitor / mute / solo).
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str, color: Color32, tip: &str) -> Response {
    let size = Vec2::new(24.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let (fill, fg) = if *on {
        (color, Color32::from_rgb(0x10, 0x12, 0x16))
    } else if resp.hovered() {
        (Color32::from_rgb(0x34, 0x39, 0x45), TEXT)
    } else {
        (PANEL_HI, TEXT_DIM)
    };
    let p = ui.painter();
    p.rect_filled(rect, 4, fill);
    p.text(rect.center(), egui::Align2::CENTER_CENTER, label, FontId::proportional(12.5), fg);
    resp.on_hover_text(tip)
}

/// Icon button with a tooltip.
pub fn icon_button(ui: &mut Ui, icon: &str, tip: &str) -> Response {
    ui.add(egui::Button::new(egui::RichText::new(icon).size(15.0)).frame(false)).on_hover_text(tip)
}

fn level_to_x(db: f32) -> f32 {
    ((db + 60.0) / 66.0).clamp(0.0, 1.0)
}

/// Horizontal stereo peak meter. `levels` are linear peak values.
pub fn meter(ui: &mut Ui, width: f32, levels: [f32; 2], tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 10.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2, BG);
    for (i, lv) in levels.iter().enumerate() {
        let h = (rect.height() - 1.0) / 2.0;
        let y0 = rect.top() + i as f32 * (h + 1.0);
        let row = Rect::from_min_max(pos2(rect.left(), y0), pos2(rect.right(), y0 + h));
        let x = level_to_x(crate::util::gain_to_db(*lv));
        let w = row.width();
        // Green up to -12 dB, yellow up to -3 dB, red above.
        let segments = [(0.0, level_to_x(-12.0), SOLO), (level_to_x(-12.0), level_to_x(-3.0), MUTE), (level_to_x(-3.0), 1.0, REC)];
        for (a, b, c) in segments {
            let e = b.min(x);
            if e > a {
                p.rect_filled(Rect::from_min_max(pos2(row.left() + a * w, row.top()), pos2(row.left() + e * w, row.bottom())), 0, c);
            }
        }
    }
    // 0 dB tick
    let zx = rect.left() + level_to_x(0.0) * rect.width();
    p.line_segment([pos2(zx, rect.top()), pos2(zx, rect.bottom())], Stroke::new(1.0, TEXT_DIM.gamma_multiply(0.6)));
    resp.on_hover_text(tip)
}

pub fn section_label(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().color(TEXT_DIM));
}
