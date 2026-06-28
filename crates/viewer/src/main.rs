// viewer — a fast native viewer for CSV, images, SVG, Office docs and PDF.
//
// Usage:  viewer [--quicklook] [FILE]
// Drag & drop a file onto the window, or press "Apri" / Ctrl+O.

mod app;
mod clip;
mod ipc;
mod ui;

use app::{file_label, App};
use eframe::egui;
use viewer_core::{decode, spawn_decode, Decoded};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::OnceLock;
use std::time::Instant;

/// Fast allocator: the decoders create millions of small `String`s; mimalloc is
/// markedly faster than the system allocator here and contends far less when the
/// decode worker runs alongside the UI thread.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Process start, used by the optional VIEWER_BENCH timing.
static START: OnceLock<Instant> = OnceLock::new();

/// egui context, published once the window exists, so decode worker threads can
/// wake the UI exactly when their result is ready (no busy polling).
static WAKE: OnceLock<egui::Context> = OnceLock::new();

pub(crate) fn wake_ui() {
    if let Some(ctx) = WAKE.get() {
        ctx.request_repaint();
    }
}

/// Print a timing checkpoint when VIEWER_BENCH is set.
pub(crate) fn bench(label: &str, extra: &str) {
    if std::env::var_os("VIEWER_BENCH").is_some() {
        if let Some(t0) = START.get() {
            eprintln!(
                "BENCH {label}_ms={:.1} {extra}",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

fn main() -> eframe::Result<()> {
    let _ = START.set(Instant::now());

    // Parse args: first non-flag is the file, `--quicklook` enables overlay mode.
    let mut quicklook = false;
    let mut file: Option<PathBuf> = None;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--quicklook" | "-q" => quicklook = true,
            _ => {
                if file.is_none() {
                    file = Some(PathBuf::from(a));
                }
            }
        }
    }
    // Use an absolute path so the running instance resolves it regardless of CWD.
    let arg = file.map(|p| std::fs::canonicalize(&p).unwrap_or(p));

    // Pipeline: by default decode the file on a worker thread, overlapping it
    // with window/GL init so the window is interactive in ~90 ms regardless of
    // file size. VIEWER_PIPELINE=sync forces the old inline decode.
    let parallel = !matches!(std::env::var("VIEWER_PIPELINE").as_deref(), Ok("sync"));

    // Single instance: hand the file to an already-running viewer, else serve.
    let sock = ipc::socket_path();
    if let Some(path) = &arg {
        if ipc::try_handoff(&sock, path) {
            return Ok(());
        }
    }
    let listener = ipc::bind_listener(&sock);
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();

    // Parallel pipeline: start decoding the initial file NOW, before run_native,
    // so it runs concurrently with the ~65 ms of window/GL initialisation.
    let initial: Option<(PathBuf, Option<Receiver<Decoded>>)> = arg.map(|path| {
        if parallel {
            let drx = spawn_decode(path.clone(), wake_ui);
            (path, Some(drx))
        } else {
            (path, None)
        }
    });

    // Backend: glow (OpenGL). Measured ~3x faster to first frame than wgpu/Vulkan
    // for this workload (trivial GPU work; Vulkan's heavier init dominates).
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("viewer")
        .with_decorations(false)
        // Transparent surface so the frosted toolbar (and rounded edges) let the
        // desktop show through; on KWin the compositor blurs it for ~free.
        .with_transparent(true)
        .with_drag_and_drop(true);
    viewport = if quicklook {
        viewport.with_inner_size([900.0, 640.0]).with_always_on_top()
    } else {
        viewport.with_inner_size([1100.0, 750.0])
    };
    let options = eframe::NativeOptions {
        viewport,
        // A depth buffer so the 3D mesh view can occlude correctly; harmless and
        // cheap for the 2D content types (egui draws without depth).
        depth_buffer: 24,
        ..Default::default()
    };

    eframe::run_native(
        "viewer",
        options,
        Box::new(move |cc| {
            let _ = WAKE.set(cc.egui_ctx.clone());
            app::configure_style(&cc.egui_ctx);

            // Listen for files sent by later invocations and wake the UI.
            if let Some(listener) = listener {
                ipc::serve(listener, tx, cc.egui_ctx.clone());
            }
            bench("eframe_init", "");
            let mut app = App::new(Some(rx), quicklook);
            app.parallel = parallel;
            if let Some((path, decoded_rx)) = initial {
                app.file_name = file_label(&path);
                app.file_path = Some(path.clone());
                match decoded_rx {
                    // Parallel: attach the in-flight decode; update() finalizes it.
                    Some(drx) => app.pending_load = Some((path, drx)),
                    // Sync: decode inline now, before the first frame.
                    None => {
                        let decoded = decode(&path);
                        app.finalize(&cc.egui_ctx, &path, decoded);
                    }
                }
            }
            bench("loaded", "");
            Ok(Box::new(app))
        }),
    )
}
