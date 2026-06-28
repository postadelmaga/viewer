//! Image / SVG / PDF-page view with cursor-anchored zoom and drag-to-pan.

use eframe::egui;
use egui::{Color32, Rect, Sense, TextureHandle, Vec2};

pub(crate) struct ImageView {
    pub(crate) texture: TextureHandle,
    pub(crate) size: Vec2,
    pub(crate) zoom: f32,
    pub(crate) offset: Vec2,
    /// e.g. "1920×1080" or "SVG 64×64"
    pub(crate) kind: String,
}

pub(crate) fn image_view(ui: &mut egui::Ui, view: &mut ImageView) {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::from_gray(30));

    // Scale that fits the image into the view at zoom == 1.
    let fit = (rect.width() / view.size.x)
        .min(rect.height() / view.size.y)
        .min(1.0);
    let mut scale = fit * view.zoom;

    // Wheel / pinch zoom, anchored on the cursor.
    if response.hovered() {
        let wheel = ui.input(|i| i.raw_scroll_delta.y);
        let pinch = ui.input(|i| i.zoom_delta());
        let scroll = wheel + if pinch != 1.0 { (pinch - 1.0) * 200.0 } else { 0.0 };
        if scroll != 0.0 {
            if let Some(cursor) = response.hover_pos() {
                let new_zoom = (view.zoom * (scroll * 0.0015).exp()).clamp(0.02, 64.0);
                let new_scale = fit * new_zoom;
                let center = rect.center() + view.offset;
                // Keep the image point under the cursor fixed while zooming.
                let img_pt = (cursor - center) / scale;
                let new_center = cursor - img_pt * new_scale;
                view.offset = new_center - rect.center();
                view.zoom = new_zoom;
                scale = new_scale;
            }
        }
    }

    // Drag to pan.
    if response.dragged() {
        view.offset += response.drag_delta();
    }

    let displayed = view.size * scale;
    let img_rect = Rect::from_center_size(rect.center() + view.offset, displayed);
    painter.image(
        view.texture.id(),
        img_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}
