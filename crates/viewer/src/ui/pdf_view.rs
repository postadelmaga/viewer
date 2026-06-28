//! PDF view state: holds the rendered page (an egui `ImageView`) and the
//! channels wiring the UI to `viewer_core::pdf::pdf_worker`.

use crate::ui::image_view::ImageView;
use std::sync::mpsc::{Receiver, Sender};
use viewer_core::pdf::{PdfMsg, PdfReq};

/// Owned bytes + the channels a render worker needs, handed over when started.
pub(crate) type WorkerPayload = (std::sync::Arc<[u8]>, Receiver<PdfReq>, Sender<PdfMsg>);

/// A PDF document. Pages are rendered off-thread so the window never blocks.
pub(crate) struct PdfView {
    pub(crate) page: usize,
    /// Page currently held in `view` (`usize::MAX` = none yet).
    pub(crate) rendered: usize,
    /// Page already requested from the worker (avoids duplicate requests).
    pub(crate) dispatched: usize,
    pub(crate) pages: usize,
    pub(crate) view: Option<ImageView>,
    pub(crate) error: Option<String>,
    pub(crate) req_tx: Sender<PdfReq>,
    pub(crate) res_rx: Receiver<PdfMsg>,
    /// Worker payload, spawned one frame late so the window paints first
    /// (avoids dynamic-linker contention with GL symbol resolution at startup).
    pub(crate) spawn: Option<WorkerPayload>,
}

/// Prepare a PDF view. The render worker is started later (first update frame)
/// so it doesn't fight the GL context for the dynamic-linker lock at startup.
pub(crate) fn prepare_pdf(bytes: Vec<u8>) -> PdfView {
    let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(bytes);
    let (req_tx, req_rx) = std::sync::mpsc::channel::<PdfReq>();
    let (res_tx, res_rx) = std::sync::mpsc::channel::<PdfMsg>();
    let _ = req_tx.send(PdfReq::Render(0));
    PdfView {
        page: 0,
        rendered: usize::MAX,
        dispatched: 0,
        pages: 0,
        view: None,
        error: None,
        req_tx,
        res_rx,
        spawn: Some((bytes, req_rx, res_tx)),
    }
}
