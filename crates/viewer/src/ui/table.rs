//! Virtualised CSV / spreadsheet table and its filter toolbar widget.
//!
//! Filtering is a *view* concern, so it lives here (in the app), wrapping the
//! UI-agnostic [`CsvData`] / [`SheetData`] the core library decodes.

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use viewer_core::{CsvData, SheetData};

/// A decoded table plus the app's live filter state over it.
pub(crate) struct CsvView {
    pub(crate) data: CsvData,
    filter: String,
    /// Indices into `data.rows` matching the current filter.
    filtered: Vec<usize>,
}

impl CsvView {
    pub(crate) fn new(data: CsvData) -> Self {
        let filtered = (0..data.rows.len()).collect();
        CsvView {
            data,
            filter: String::new(),
            filtered,
        }
    }

    fn refilter(&mut self) {
        if self.filter.is_empty() {
            self.filtered = (0..self.data.rows.len()).collect();
            return;
        }
        let needle = self.filter.to_lowercase();
        self.filtered = self
            .data
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.iter().any(|c| contains_ci(c, &needle)))
            .map(|(i, _)| i)
            .collect();
    }
}

/// A workbook plus the currently selected sheet.
pub(crate) struct SheetView {
    pub(crate) sheets: Vec<(String, CsvView)>,
    pub(crate) current: usize,
}

impl SheetView {
    pub(crate) fn new(data: SheetData) -> Self {
        SheetView {
            sheets: data
                .sheets
                .into_iter()
                .map(|(name, csv)| (name, CsvView::new(csv)))
                .collect(),
            current: 0,
        }
    }
}

pub(crate) fn csv_toolbar(ui: &mut egui::Ui, view: &mut CsvView) {
    ui.label(format!(
        "{} righe × {} col",
        view.data.rows.len(),
        view.data.headers.len()
    ));
    ui.separator();
    ui.label("🔍");
    let resp = ui.add(
        egui::TextEdit::singleline(&mut view.filter)
            .hint_text("filtra…")
            .desired_width(200.0),
    );
    if resp.changed() {
        view.refilter();
    }
    if !view.filter.is_empty() {
        ui.label(format!("{} match", view.filtered.len()));
    }
}

pub(crate) fn csv_table(ui: &mut egui::Ui, view: &CsvView) {
    let data = &view.data;
    let text_height = egui::TextStyle::Body.resolve(ui.style()).size + 6.0;
    let ncols = data.headers.len();

    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::auto()) // row number
        .columns(Column::initial(140.0).at_least(40.0).clip(true), ncols)
        .min_scrolled_height(0.0)
        .auto_shrink([false, false])
        .header(text_height + 4.0, |mut header| {
            header.col(|ui| {
                ui.strong("#");
            });
            for h in &data.headers {
                header.col(|ui| {
                    ui.strong(h);
                });
            }
        })
        .body(|body| {
            body.rows(text_height, view.filtered.len(), |mut row| {
                let src = view.filtered[row.index()];
                row.col(|ui| {
                    ui.weak((src + 1).to_string());
                });
                let cells = &data.rows[src];
                for c in 0..ncols {
                    row.col(|ui| {
                        if let Some(v) = cells.get(c) {
                            ui.label(v);
                        }
                    });
                }
            });
        });
}

/// Case-insensitive substring test. `needle` must already be lowercased.
/// Avoids allocating a lowercased copy of every cell on the common ASCII path.
fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        hay.to_lowercase().contains(needle)
    }
}
