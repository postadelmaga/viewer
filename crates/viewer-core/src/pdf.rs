//! PDF: bind pdfium and render pages off-thread, delivered as raw RGBA.
//!
//! UI-agnostic: the render worker reports pages over a channel and signals
//! progress through a caller-supplied `wake` callback (e.g. "request a UI
//! repaint"), so no windowing toolkit is referenced here.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

/// Width, in pixels, of a rendered full page. Page height follows the aspect
/// ratio. Word boxes are reported separately in normalised coordinates, so they
/// stay valid no matter what size the caller ends up displaying the page at.
const RENDER_WIDTH: i32 = 1800;

/// A request to the PDF worker. Rendering a page and reading its text layer are
/// independent operations on the same open document, so the caller picks which
/// it wants per message.
///
/// `#[non_exhaustive]`: more request kinds (region render, search) may be added
/// without a breaking change.
#[non_exhaustive]
pub enum PdfReq {
    /// Render the page to RGBA at [`RENDER_WIDTH`]. Answered with [`PdfMsg::Page`].
    Render(usize),
    /// Extract the page's text layer (words + boxes). Answered with
    /// [`PdfMsg::Text`]. Far cheaper than rendering, so it is safe to ask for it
    /// eagerly (e.g. to enable click-to-locate before the user zooms).
    Text(usize),
}

/// One word of a page's text layer with its bounding box.
///
/// The box is in **normalised page coordinates**: `x`/`y` is the top-left corner
/// and `w`/`h` the size, each in `0.0..=1.0` of the page width/height, with the
/// origin at the page's top-left — the same convention as the rendered image.
/// To draw a highlight, multiply by the on-screen page rectangle; to recover PDF
/// points, multiply by `page_w_pt`/`page_h_pt` from [`PdfMsg::Text`]. Being
/// resolution-independent, a word box is valid at any zoom level.
pub struct PdfWord {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Messages from the PDF worker thread.
pub enum PdfMsg {
    /// Total page count, sent once the document opens.
    Meta(usize),
    /// A rendered page as raw, unmultiplied RGBA.
    Page {
        page: usize,
        rgba: Vec<u8>,
        w: u32,
        h: u32,
    },
    /// A page's text layer: words with normalised boxes, plus the page size in
    /// PDF points (for callers that need absolute coordinates).
    Text {
        page: usize,
        words: Vec<PdfWord>,
        page_w_pt: f32,
        page_h_pt: f32,
    },
    Err(String),
}

/// Locate and bind `libpdfium`. Searches, in order: `$PDFIUM_LIB`, next to the
/// current executable, `~/.local/lib`, `/usr/lib`, `/usr/local/lib`, then the
/// system library.
pub fn bind_pdfium() -> Result<pdfium_render::prelude::Pdfium, String> {
    use pdfium_render::prelude::Pdfium;

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("PDFIUM_LIB") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("libpdfium.so"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/lib/libpdfium.so"));
    }
    candidates.push(PathBuf::from("/usr/lib/libpdfium.so"));
    candidates.push(PathBuf::from("/usr/local/lib/libpdfium.so"));

    for path in candidates {
        if path.exists() {
            if let Ok(b) = Pdfium::bind_to_library(&path) {
                return Ok(Pdfium::new(b));
            }
        }
    }
    if let Ok(b) = Pdfium::bind_to_system_library() {
        return Ok(Pdfium::new(b));
    }
    Err(
        "Libreria pdfium non trovata.\nInstalla libpdfium.so (es. in ~/.local/lib/)\noppure imposta la variabile PDFIUM_LIB."
            .into(),
    )
}

/// PDF worker: binds pdfium and opens the document once, then serves [`PdfReq`]s
/// (render a page, read a page's text layer) on its own thread so the caller
/// never blocks on pdfium. Read requests from `req_rx`, get [`PdfMsg`]s back on
/// `res_tx`; `wake` is called after every message so a UI can pick the result up
/// without busy-polling (pass a no-op `|| {}` if you don't need it).
pub fn pdf_worker(
    bytes: std::sync::Arc<[u8]>,
    req_rx: Receiver<PdfReq>,
    res_tx: Sender<PdfMsg>,
    wake: impl Fn(),
) {
    let pdfium = match bind_pdfium() {
        Ok(p) => p,
        Err(e) => {
            let _ = res_tx.send(PdfMsg::Err(e));
            wake();
            return;
        }
    };
    let doc = match pdfium.load_pdf_from_byte_slice(&bytes, None) {
        Ok(d) => d,
        Err(e) => {
            let _ = res_tx.send(PdfMsg::Err(format!("PDF non apribile:\n{e}")));
            wake();
            return;
        }
    };
    let count = doc.pages().len() as usize;
    let _ = res_tx.send(PdfMsg::Meta(count));
    wake();
    if count == 0 {
        return;
    }

    while let Ok(req) = req_rx.recv() {
        let msg = match req {
            PdfReq::Render(page) => render_page(&doc, page, count),
            PdfReq::Text(page) => text_layer(&doc, page, count),
        };
        if res_tx.send(msg).is_err() {
            break;
        }
        wake();
    }
}

/// Render one page to RGBA at [`RENDER_WIDTH`].
fn render_page(doc: &pdfium_render::prelude::PdfDocument, page: usize, count: usize) -> PdfMsg {
    use pdfium_render::prelude::PdfRenderConfig;

    let idx = page.min(count - 1) as u16;
    match doc.pages().get(idx) {
        Ok(p) => match p.render_with_config(&PdfRenderConfig::new().set_target_width(RENDER_WIDTH)) {
            Ok(bmp) => {
                let rgba = bmp.as_image().into_rgba8();
                let (w, h) = rgba.dimensions();
                PdfMsg::Page {
                    page,
                    rgba: rgba.into_raw(),
                    w,
                    h,
                }
            }
            Err(e) => PdfMsg::Err(format!("Rendering fallito:\n{e}")),
        },
        Err(e) => PdfMsg::Err(format!("Pagina non leggibile:\n{e}")),
    }
}

/// Extract one page's text layer: words grouped on whitespace, each with the
/// union of its characters' bounding boxes, in normalised top-left coordinates.
fn text_layer(doc: &pdfium_render::prelude::PdfDocument, page: usize, count: usize) -> PdfMsg {
    let idx = page.min(count - 1) as u16;
    let p = match doc.pages().get(idx) {
        Ok(p) => p,
        Err(e) => return PdfMsg::Err(format!("Pagina non leggibile:\n{e}")),
    };
    // Char boxes are in the unrotated page frame, but render_page honours the
    // page's /Rotate, so the rendered image is rotated. Capture the rotation to
    // map word boxes into the same frame the highlight is drawn over.
    let rotation = p
        .rotation()
        .unwrap_or(pdfium_render::prelude::PdfPageRenderRotation::None);
    let page_w_pt = p.width().value;
    let page_h_pt = p.height().value;
    // A zero-sized page would make every box NaN; report it as empty instead.
    if page_w_pt <= 0.0 || page_h_pt <= 0.0 {
        return PdfMsg::Text {
            page,
            words: Vec::new(),
            page_w_pt,
            page_h_pt,
        };
    }
    let text = match p.text() {
        Ok(t) => t,
        Err(e) => return PdfMsg::Err(format!("Testo non estraibile:\n{e}")),
    };

    let mut words: Vec<PdfWord> = Vec::new();
    let mut cur = String::new();
    // Running union of the current word's char boxes, in PDF points
    // (left, bottom, right, top); `None` until the word has a box.
    let mut bbox: Option<(f32, f32, f32, f32)> = None;

    for ch in text.chars().iter() {
        let c = ch.unicode_char();
        if c.is_none_or(|c| c.is_whitespace()) {
            flush_word(&mut cur, &mut bbox, page_w_pt, page_h_pt, rotation, &mut words);
            continue;
        }
        cur.push(c.unwrap());
        if let Ok(r) = ch.loose_bounds() {
            let (l, b, rt, t) = (
                r.left().value,
                r.bottom().value,
                r.right().value,
                r.top().value,
            );
            bbox = Some(match bbox {
                Some((ml, mb, mr, mt)) => (ml.min(l), mb.min(b), mr.max(rt), mt.max(t)),
                None => (l, b, rt, t),
            });
        }
    }
    flush_word(&mut cur, &mut bbox, page_w_pt, page_h_pt, rotation, &mut words);

    PdfMsg::Text {
        page,
        words,
        page_w_pt,
        page_h_pt,
    }
}

/// Emit the accumulated word (if any) as a normalised, top-left-origin box and
/// reset the accumulators. A word with no box (chars whose bounds pdfium could
/// not report) is dropped, since a highlight needs coordinates.
fn flush_word(
    cur: &mut String,
    bbox: &mut Option<(f32, f32, f32, f32)>,
    page_w_pt: f32,
    page_h_pt: f32,
    rotation: pdfium_render::prelude::PdfPageRenderRotation,
    out: &mut Vec<PdfWord>,
) {
    use pdfium_render::prelude::PdfPageRenderRotation as Rot;
    if let (false, Some((l, b, r, t))) = (cur.is_empty(), *bbox) {
        // Normalised top-left-origin corners in the *unrotated* page frame
        // (PDF Y grows upward from the bottom, so flip).
        let (x0, y0) = (l / page_w_pt, (page_h_pt - t) / page_h_pt);
        let (x1, y1) = (r / page_w_pt, (page_h_pt - b) / page_h_pt);
        // Rotate each corner clockwise to match the rendered (rotated) image.
        let rot = |x: f32, y: f32| match rotation {
            Rot::None => (x, y),
            Rot::Degrees90 => (1.0 - y, x),
            Rot::Degrees180 => (1.0 - x, 1.0 - y),
            Rot::Degrees270 => (y, 1.0 - x),
        };
        let (ax, ay) = rot(x0, y0);
        let (bx, by) = rot(x1, y1);
        // Clamp the corners jointly so x+w / y+h can't overflow the page rect.
        let xmin = ax.min(bx).clamp(0.0, 1.0);
        let xmax = ax.max(bx).clamp(0.0, 1.0);
        let ymin = ay.min(by).clamp(0.0, 1.0);
        let ymax = ay.max(by).clamp(0.0, 1.0);
        out.push(PdfWord {
            text: std::mem::take(cur),
            x: xmin,
            y: ymin,
            w: xmax - xmin,
            h: ymax - ymin,
        });
    }
    cur.clear();
    *bbox = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    #[test]
    fn text_layer_reports_words_with_normalised_boxes() {
        let bytes = std::fs::read(format!("{DIR}/test.pdf")).expect("test.pdf");
        let pdfium = bind_pdfium().expect("binding libpdfium");
        let doc = pdfium
            .load_pdf_from_byte_slice(&bytes, None)
            .expect("load pdf");
        let count = doc.pages().len() as usize;

        match text_layer(&doc, 0, count) {
            PdfMsg::Text {
                page,
                words,
                page_w_pt,
                page_h_pt,
            } => {
                assert_eq!(page, 0);
                assert!(page_w_pt > 0.0 && page_h_pt > 0.0, "page size in points");
                // The fixture reads "Ciao PDF" — two whitespace-separated words.
                let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
                assert!(texts.contains(&"Ciao"), "words: {texts:?}");
                assert!(texts.contains(&"PDF"), "words: {texts:?}");
                for w in &words {
                    assert!(w.w > 0.0 && w.h > 0.0, "non-empty box for {:?}", w.text);
                    assert!(
                        w.x >= 0.0 && w.y >= 0.0 && w.x + w.w <= 1.0001 && w.y + w.h <= 1.0001,
                        "box of {:?} inside the page: x={} y={} w={} h={}",
                        w.text,
                        w.x,
                        w.y,
                        w.w,
                        w.h
                    );
                }
                // "Ciao" precedes "PDF" on the same line: same row, further left.
                let ciao = words.iter().find(|w| w.text == "Ciao").unwrap();
                let pdf = words.iter().find(|w| w.text == "PDF").unwrap();
                assert!(ciao.x < pdf.x, "reading order left-to-right");
                assert!((ciao.y - pdf.y).abs() < 0.05, "same line");
            }
            other => panic!("expected Text, got {}", msg_variant(&other)),
        }
    }

    fn msg_variant(m: &PdfMsg) -> &'static str {
        match m {
            PdfMsg::Meta(_) => "Meta",
            PdfMsg::Page { .. } => "Page",
            PdfMsg::Text { .. } => "Text",
            PdfMsg::Err(_) => "Err",
        }
    }
}
