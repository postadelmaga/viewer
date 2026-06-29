//! Read-only text/code view: syntax highlighting, font zoom, line numbers, wrap.
//!
//! Highlighting uses `egui_extras::syntax_highlighting`, which ships a small
//! built-in highlighter (no `syntect`/extra dependency): keyword colouring for
//! Rust/C-family/Python/TOML and generic comment/string colouring otherwise. We
//! recolour its output to a **discreet** palette harmonised with the app accent
//! (identifiers stay near-neutral; only keywords/strings/comments are tinted),
//! and scale the monospace font through `override_font_id` so colours and size
//! zoom together. Line numbers live in a synced side gutter (no-wrap mode).

use eframe::egui;
use egui::text::LayoutJob;
use egui::{Color32, FontId, TextStyle};

/// A text/code document plus its view state.
pub(crate) struct TextView {
    text: String,
    /// Highlighter language key (the file extension, lowercased); `""` = generic.
    language: String,
    /// Font-size multiplier applied to the monospace text style.
    zoom: f32,
    /// Soft-wrap long lines instead of scrolling horizontally.
    wrap: bool,
    /// Displayed row count (logical lines), for the number gutter.
    lines: usize,
    /// Last text scroll offset, mirrored into the gutter so the two stay aligned.
    gutter_offset: f32,
}

impl TextView {
    pub(crate) fn new(text: String, language: String) -> Self {
        let lines = text.bytes().filter(|&b| b == b'\n').count() + 1;
        Self {
            text,
            language,
            zoom: 1.0,
            wrap: false,
            lines,
            gutter_offset: 0.0,
        }
    }

    pub(crate) fn zoom_in(&mut self) {
        self.zoom = (self.zoom * 1.1).min(6.0);
    }
    pub(crate) fn zoom_out(&mut self) {
        self.zoom = (self.zoom / 1.1).max(0.4);
    }
    pub(crate) fn zoom_reset(&mut self) {
        self.zoom = 1.0;
    }
    pub(crate) fn zoom_pct(&self) -> f32 {
        self.zoom * 100.0
    }
    pub(crate) fn wrap(&self) -> bool {
        self.wrap
    }
    pub(crate) fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
    }
}

/// Draw the document, handling Ctrl+scroll zoom.
pub(crate) fn text_view(ui: &mut egui::Ui, view: &mut TextView) {
    // Ctrl+wheel zoom (plain wheel is left to the scroll area).
    if ui.rect_contains_pointer(ui.max_rect()) {
        let scroll = ui.input(|i| {
            if i.modifiers.command {
                i.raw_scroll_delta.y
            } else {
                0.0
            }
        });
        if scroll > 0.0 {
            view.zoom_in();
        } else if scroll < 0.0 {
            view.zoom_out();
        }
    }

    // Scale the monospace font by the zoom, keeping zoom = 1.0 at the app default.
    let base = TextStyle::Monospace.resolve(ui.style()).size;
    let font_id = FontId::monospace(base * view.zoom);
    let mut style = (**ui.style()).clone();
    style.override_font_id = Some(font_id.clone());
    // `from_style` (not `from_memory`) builds the token formats from
    // `override_font_id`, so the scaled font reaches the highlighted text too —
    // otherwise the colours would render at a fixed base size and ignore zoom.
    let theme = egui_extras::syntax_highlighting::CodeTheme::from_style(&style);

    let style = std::sync::Arc::new(style);
    let lang = view.language.clone();
    let wrap = view.wrap;
    let mut layouter = move |ui: &egui::Ui, text: &str, wrap_width: f32| {
        let mut job =
            egui_extras::syntax_highlighting::highlight(ui.ctx(), &style, &theme, text, &lang);
        harmonize(&mut job);
        job.wrap.max_width = if wrap { wrap_width } else { f32::INFINITY };
        ui.fonts(|f| f.layout_job(job))
    };

    ui.horizontal_top(|ui| {
        // Line-number gutter (only when not wrapping: with wrap on, a logical
        // line spans several rows and the 1:1 numbering would drift).
        if !wrap {
            line_gutter(ui, view, &font_id);
            ui.add_space(8.0);
        }

        let avail = ui.available_width();
        let scroll = if wrap {
            egui::ScrollArea::vertical()
        } else {
            egui::ScrollArea::both()
        };
        let out = scroll.id_salt("text_body").show(ui, |ui| {
            let mut text = view.text.as_str();
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .desired_width(if wrap { avail } else { f32::INFINITY })
                    .frame(false)
                    .margin(egui::Margin::ZERO)
                    .font(font_id.clone())
                    .layouter(&mut layouter),
            );
        });
        view.gutter_offset = out.state.offset.y;
    });
}

/// Right-aligned line numbers in their own vertical scroll area, its offset
/// mirrored from the text body so the two scroll together (one frame of lag).
fn line_gutter(ui: &mut egui::Ui, view: &TextView, font_id: &FontId) {
    let width = view.lines.to_string().len();
    let mut numbers = String::with_capacity(view.lines * (width + 1));
    for n in 1..=view.lines {
        use std::fmt::Write;
        let _ = writeln!(numbers, "{n:>width$}");
    }
    let job = LayoutJob::simple(
        numbers,
        font_id.clone(),
        Color32::from_gray(96), // dim, recedes behind the code
        f32::INFINITY,
    );
    egui::ScrollArea::vertical()
        .id_salt("line_gutter")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .vertical_scroll_offset(view.gutter_offset)
        .show(ui, |ui| {
            ui.add(egui::Label::new(job).selectable(false));
        });
}

// Default colours emitted by egui_extras' built-in highlighter, remapped to a
// restrained palette: identifiers go near-neutral so the page reads calmly, and
// only keywords (a desaturated accent blue, kept clear of the error red),
// strings (soft sage) and comments (dim grey) carry tint.
fn harmonize(job: &mut LayoutJob) {
    for section in &mut job.sections {
        let c = section.format.color;
        section.format.color = match (c.r(), c.g(), c.b()) {
            (255, 100, 100) => Color32::from_rgb(150, 170, 235), // keyword
            (109, 147, 226) => Color32::from_rgb(158, 188, 150), // string literal
            (120, 120, 120) => Color32::from_gray(112),          // comment
            (87, 165, 171) => Color32::from_gray(205),           // identifiers/numbers
            (192, 192, 192) => Color32::from_gray(202),          // punctuation / plain text
            _ => c,
        };
    }
}
