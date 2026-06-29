// viewer — a fast native viewer for CSV, images, SVG, Office docs and PDF.
//
// Usage:  viewer [--quicklook] [--screenshot OUT.png [--headless] [--size WxH]] [FILE]
// Drag & drop a file onto the window, or press "Apri" / Ctrl+O.
//
// `--screenshot OUT.png FILE` renders FILE, captures the window once it has
// painted, writes the PNG and exits — a preview/thumbnail mode. Add `--headless`
// (implied when no display is available) to render offscreen with no window, so
// it works on a server / in CI; `--size WxH` sets the headless canvas size.

mod app;
mod clip;
mod headless;
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

/// Parse a `WxH` size string (e.g. `1280x720`) into pixel dimensions.
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

fn main() -> eframe::Result<()> {
    let _ = START.set(Instant::now());

    // Parse args: first non-flag is the file, `--quicklook` enables overlay mode,
    // `--screenshot OUT` renders the file to a PNG and exits (its argument is the
    // next token, the output path).
    let mut quicklook = false;
    let mut shot: Option<PathBuf> = None;
    let mut file: Option<PathBuf> = None;
    let mut headless = false;
    let mut size: Option<(u32, u32)> = None;
    let mut want_shot_out = false;
    let mut want_size = false;
    for a in std::env::args().skip(1) {
        if want_shot_out {
            shot = Some(PathBuf::from(a));
            want_shot_out = false;
            continue;
        }
        if want_size {
            size = Some(parse_size(&a).unwrap_or_else(|| {
                eprintln!("viewer: --size expects WxH, e.g. 1280x720");
                std::process::exit(2);
            }));
            want_size = false;
            continue;
        }
        match a.as_str() {
            "--quicklook" | "-q" => quicklook = true,
            "--screenshot" | "-s" => want_shot_out = true,
            "--headless" => headless = true,
            "--size" => want_size = true,
            _ => {
                if file.is_none() {
                    file = Some(PathBuf::from(a));
                }
            }
        }
    }
    if want_shot_out {
        eprintln!("viewer: --screenshot requires an output path");
        std::process::exit(2);
    }
    if want_size {
        eprintln!("viewer: --size requires a WxH value");
        std::process::exit(2);
    }
    if shot.is_some() && file.is_none() {
        eprintln!("viewer: --screenshot needs a FILE to render");
        std::process::exit(2);
    }
    if (headless || size.is_some()) && shot.is_none() {
        eprintln!("viewer: --headless/--size only apply with --screenshot");
        std::process::exit(2);
    }
    // Use an absolute path so the running instance resolves it regardless of CWD.
    let arg = file.map(|p| std::fs::canonicalize(&p).unwrap_or(p));

    // Headless screenshot: render offscreen with no window. Chosen explicitly via
    // `--headless`, or automatically when there's no display server to open a
    // window on (so the same command works on a desktop and on a bare server).
    if let Some(out) = &shot {
        let no_display =
            std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none();
        if headless || no_display {
            let path = arg.as_ref().expect("--screenshot requires a file");
            let (w, h) = size.unwrap_or((1100, 750));
            return match headless::capture(path, out, w, h) {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("viewer: headless screenshot failed: {e}");
                    std::process::exit(1);
                }
            };
        }
    }

    // Pipeline: by default decode the file on a worker thread, overlapping it
    // with window/GL init so the window is interactive in ~90 ms regardless of
    // file size. VIEWER_PIPELINE=sync forces the old inline decode.
    let parallel = !matches!(std::env::var("VIEWER_PIPELINE").as_deref(), Ok("sync"));

    // Single instance: hand the file to an already-running viewer, else serve.
    // Screenshot mode is a one-shot render+exit: it must NOT hand the file off to
    // a resident instance (that one would show it and our process would quit
    // capturing nothing), nor register as the server, so skip IPC entirely.
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    let listener = if shot.is_none() {
        let sock = ipc::socket_path();
        if let Some(path) = &arg {
            if ipc::try_handoff(&sock, path) {
                return Ok(());
            }
        }
        ipc::bind_listener(&sock)
    } else {
        None
    };

    // First-run convenience: fetch the MIDI SoundFont in the background so a later
    // `.mid` plays without a manual step. Best-effort; only the serving instance.
    viewer_core::midi::ensure_soundfont_background();

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
            // No IPC inbox in screenshot mode (we never bound the listener).
            let inbox = if shot.is_none() { Some(rx) } else { None };
            let mut app = App::new(inbox, quicklook, shot);
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
