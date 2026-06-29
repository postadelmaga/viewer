//! Application state and the egui update loop.

use crate::ui::image_view::{image_view, ImageView};
use crate::ui::media_view::{media_toolbar, media_view, MediaView};
use crate::ui::mesh_view::{mesh_view, MeshView};
use crate::ui::pdf_view::{prepare_pdf, PdfView};
use crate::ui::table::{csv_table, csv_toolbar, CsvView, SheetView};
use crate::ui::text_view::{text_view, TextView};
use crate::{bench, wake_ui};
use eframe::egui;
use egui::{Color32, TextureOptions, Vec2};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;
use viewer_core::media::media_worker;
use viewer_core::midi::midi_worker;
use viewer_core::pdf::{pdf_worker, PdfMsg, PdfReq};
use viewer_core::{decode, spawn_decode, Decoded};

/// What is currently being displayed.
pub(crate) enum Content {
    Empty,
    Csv(CsvView),
    Sheets(SheetView),
    Image(ImageView),
    Markdown(String),
    Pdf(PdfView),
    Mesh(MeshView),
    Text(TextView),
    Media(MediaView),
    Error(String),
}

pub(crate) struct App {
    content: Content,
    pub(crate) file_name: String,
    pub(crate) file_path: Option<PathBuf>,
    md_cache: egui_commonmark::CommonMarkCache,
    first_frame: bool,
    /// Quick Look overlay mode: dismiss on Space / focus loss, stay resident.
    quicklook: bool,
    ever_focused: bool,
    /// Hidden-but-alive (resident): the window is dismissed but the process
    /// stays so the next open is instant.
    hidden: bool,
    hidden_since: Option<Instant>,
    fullscreen: bool,
    /// Files pushed by later `viewer` invocations (single-instance).
    inbox: Option<Receiver<PathBuf>>,
    /// Parallel pipeline: decode files on a worker thread.
    pub(crate) parallel: bool,
    /// In-flight parallel decode (the path and where its result will arrive).
    pub(crate) pending_load: Option<(PathBuf, Receiver<Decoded>)>,
    /// Whether content_ready has already been reported (for VIEWER_BENCH).
    reported_ready: bool,
    /// Transient "copied" feedback shown briefly in the toolbar.
    copy_status: Option<(String, Instant)>,
    /// The glow context, captured once so a replaced mesh's GPU buffers can be
    /// freed while the context is still current (fixes a file-switch leak).
    gl: Option<std::sync::Arc<eframe::glow::Context>>,
}

/// Apply the app's visual theme: a modern dark palette with an accent colour,
/// rounded widgets, and slightly translucent panel fills. Combined with the
/// transparent window, the toolbar reads as frosted glass (KWin blurs it when
/// the Blur effect is on). Purely a paint-style change — no performance cost.
pub(crate) fn configure_style(ctx: &egui::Context) {
    use egui::{Color32, Rounding};

    let mut v = egui::Visuals::dark();
    let accent = Color32::from_rgb(98, 134, 248);

    // Near-opaque so content stays readable; the desktop only barely shows.
    v.panel_fill = Color32::from_rgba_unmultiplied(24, 25, 31, 246);
    v.window_fill = Color32::from_rgba_unmultiplied(28, 29, 36, 242);
    v.extreme_bg_color = Color32::from_rgba_unmultiplied(16, 17, 21, 246);
    v.window_rounding = Rounding::same(10.0);
    v.menu_rounding = Rounding::same(8.0);
    v.selection.bg_fill = accent.gamma_multiply(0.45);
    v.selection.stroke = egui::Stroke::new(1.0, accent);
    v.hyperlink_color = accent;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = Rounding::same(6.0);
    }
    v.widgets.hovered.bg_fill = Color32::from_rgb(48, 50, 60);
    v.widgets.active.bg_fill = accent.gamma_multiply(0.55);
    ctx.set_visuals(v);

    let mut s = (*ctx.style()).clone();
    s.spacing.button_padding = egui::vec2(9.0, 5.0);
    s.spacing.item_spacing = egui::vec2(8.0, 6.0);

    // Explicit type scale: slightly larger and airier than egui's defaults, so
    // body text, tables, toolbar and code read consistently across the app.
    use egui::{FontFamily, FontId, TextStyle};
    s.text_styles = [
        (TextStyle::Small, FontId::new(10.5, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();

    ctx.set_style(s);
}

/// Display name for the title bar / toolbar.
pub(crate) fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Whether `path`'s extension is one of `exts` (case-insensitive). `exts` comes
/// from [`viewer_core::supported_extensions`], so the open dialog and arrow-key
/// folder navigation stay in lockstep with the decoder registry.
fn ext_in(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
}

impl Default for App {
    fn default() -> Self {
        App {
            content: Content::Empty,
            file_name: String::new(),
            file_path: None,
            md_cache: egui_commonmark::CommonMarkCache::default(),
            first_frame: true,
            quicklook: false,
            ever_focused: false,
            hidden: false,
            hidden_since: None,
            fullscreen: false,
            inbox: None,
            parallel: false,
            pending_load: None,
            reported_ready: false,
            copy_status: None,
            gl: None,
        }
    }
}

impl App {
    /// How long a resident (hidden) Quick Look instance lingers before quitting.
    const IDLE_QUIT: std::time::Duration = std::time::Duration::from_secs(300);

    pub(crate) fn new(inbox: Option<Receiver<PathBuf>>, quicklook: bool) -> Self {
        App {
            inbox,
            quicklook,
            ..Default::default()
        }
    }

    /// Dismiss the window: in Quick Look mode hide but stay resident (instant
    /// reopen); otherwise close the process.
    fn dismiss(&mut self, ctx: &egui::Context) {
        if self.quicklook {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
            self.hidden_since = Some(Instant::now());
            ctx.request_repaint_after(Self::IDLE_QUIT);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }

    /// Bring a (possibly hidden) window back to the front for a new file.
    fn reveal(&mut self, ctx: &egui::Context) {
        if self.hidden {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            self.hidden = false;
        }
        self.hidden_since = None;
        self.ever_focused = false; // re-arm focus-loss dismissal
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Open a file. In the parallel pipeline the heavy decode runs on a worker
    /// thread (the window stays live); in the sync pipeline it runs inline.
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        if self.parallel {
            self.begin_load(path);
        } else {
            let decoded = decode(&path);
            self.finalize(ctx, &path, decoded);
        }
    }

    /// Parallel pipeline: kick off the decode thread and show a spinner.
    fn begin_load(&mut self, path: PathBuf) {
        self.file_name = file_label(&path);
        self.file_path = Some(path.clone());
        self.set_content(Content::Empty);
        let rx = spawn_decode(path.clone(), wake_ui);
        self.pending_load = Some((path, rx));
    }

    /// Build textures (needs ctx) and show the decoded content.
    pub(crate) fn finalize(&mut self, ctx: &egui::Context, path: &Path, decoded: Decoded) {
        self.file_name = file_label(path);
        self.file_path = Some(path.to_path_buf());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "viewer — {}",
            self.file_name
        )));
        self.set_content(build_content(ctx, decoded, path));
    }

    /// Replace the shown content, first releasing a previous mesh's GPU buffers
    /// while the glow context is live (dropping the renderer can't free them).
    fn set_content(&mut self, new: Content) {
        if let (Content::Mesh(view), Some(gl)) = (&self.content, &self.gl) {
            view.destroy(gl);
        }
        self.content = new;
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let exts = viewer_core::supported_extensions();
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Supportati", &exts)
            .add_filter("Tutti i file", &["*"])
            .pick_file()
        {
            self.open(ctx, path);
        }
    }

    /// Step to the previous/next openable file in the current file's folder
    /// (`delta` = -1 / +1), wrapping at the ends. Files are sorted by name
    /// (case-insensitive), matching the file manager's usual order.
    fn navigate_dir(&mut self, ctx: &egui::Context, delta: i32) {
        let Some(cur) = self.file_path.clone() else {
            return;
        };
        let Some(dir) = cur.parent() else {
            return;
        };
        let exts = viewer_core::supported_extensions();
        let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && ext_in(p, &exts))
                .collect(),
            Err(_) => return,
        };
        if files.len() < 2 {
            return; // nothing to step to
        }
        files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));

        let cur_name = cur.file_name();
        let here = files.iter().position(|p| p.file_name() == cur_name);
        let n = files.len() as i32;
        // Wrap so Left at the first file lands on the last, and vice versa.
        let next = match here {
            Some(i) => (((i as i32 + delta) % n) + n) % n,
            None => 0,
        } as usize;
        let target = files[next].clone();
        if target != cur {
            self.open(ctx, target);
        }
    }

    /// Copy the current file to the clipboard — image data for raster images, the
    /// file itself (uri-list) otherwise — falling back to the path as text.
    fn copy_current(&mut self, ctx: &egui::Context) {
        let Some(path) = self.file_path.clone() else {
            return;
        };
        let msg = match crate::clip::copy_file(&path) {
            Ok(m) => m,
            Err(_) => {
                ctx.copy_text(path.to_string_lossy().to_string());
                "Percorso copiato".to_string()
            }
        };
        self.copy_status = Some((msg, Instant::now()));
    }

    /// Per-content controls shown in the toolbar.
    fn toolbar_extras(&mut self, ui: &mut egui::Ui) {
        match &mut self.content {
            Content::Csv(data) => csv_toolbar(ui, data),
            Content::Sheets(sd) => {
                egui::ComboBox::from_id_salt("sheet")
                    .selected_text(sd.sheets[sd.current].0.clone())
                    .show_ui(ui, |ui| {
                        for (i, (name, _)) in sd.sheets.iter().enumerate() {
                            ui.selectable_value(&mut sd.current, i, name);
                        }
                    });
                ui.separator();
                csv_toolbar(ui, &mut sd.sheets[sd.current].1);
            }
            Content::Image(view) => {
                ui.label(&view.kind);
                ui.separator();
                if ui.button("➖").clicked() {
                    view.zoom = (view.zoom / 1.25).max(0.02);
                }
                ui.label(format!("{:.0}%", view.zoom * 100.0));
                if ui.button("➕").clicked() {
                    view.zoom = (view.zoom * 1.25).min(64.0);
                }
                if ui.button("Adatta").clicked() {
                    view.zoom = 1.0;
                    view.offset = Vec2::ZERO;
                }
            }
            Content::Mesh(view) => {
                ui.label(&view.kind);
                ui.separator();
                if ui.button("Adatta").clicked() {
                    view.reset();
                }
                ui.label("trascina = ruota · rotella = zoom");
            }
            Content::Text(view) => {
                if ui.button("➖").clicked() {
                    view.zoom_out();
                }
                ui.label(format!("{:.0}%", view.zoom_pct()));
                if ui.button("➕").clicked() {
                    view.zoom_in();
                }
                if ui.button("Adatta").clicked() {
                    view.zoom_reset();
                }
                ui.separator();
                if ui
                    .selectable_label(view.wrap(), "a-capo")
                    .on_hover_text("Manda a capo le righe lunghe")
                    .clicked()
                {
                    view.toggle_wrap();
                }
                ui.label("Ctrl+rotella = zoom");
            }
            Content::Media(view) => media_toolbar(ui, view),
            Content::Pdf(pdf) => {
                if ui.button("◀").clicked() && pdf.page > 0 {
                    pdf.page -= 1;
                }
                ui.label(format!("pag {}/{}", pdf.page + 1, pdf.pages.max(1)));
                if ui.button("▶").clicked() && pdf.page + 1 < pdf.pages {
                    pdf.page += 1;
                }
                if let Some(view) = &mut pdf.view {
                    ui.separator();
                    if ui.button("➖").clicked() {
                        view.zoom = (view.zoom / 1.25).max(0.05);
                    }
                    ui.label(format!("{:.0}%", view.zoom * 100.0));
                    if ui.button("➕").clicked() {
                        view.zoom = (view.zoom * 1.25).min(16.0);
                    }
                    if ui.button("Adatta").clicked() {
                        view.zoom = 1.0;
                        view.offset = Vec2::ZERO;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Build the live `Content` from a decode result, creating GPU textures.
fn build_content(ctx: &egui::Context, decoded: Decoded, path: &Path) -> Content {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    match decoded {
        Decoded::Csv(c) => Content::Csv(CsvView::new(c)),
        Decoded::Sheets(s) => Content::Sheets(SheetView::new(s)),
        Decoded::Markdown(s) => Content::Markdown(s),
        Decoded::Text(s) => Content::Text(TextView::new(s, ext)),
        Decoded::Pdf(bytes) => Content::Pdf(prepare_pdf(bytes)),
        Decoded::Mesh(m) => Content::Mesh(MeshView::new(m)),
        Decoded::Media(info) => {
            // Spawn the playback worker streaming from the path; it wakes the UI
            // when a frame or status arrives. The view owns the control channel.
            // MIDI is synthesised (rustysynth + SoundFont); everything else is
            // demuxed/decoded by FFmpeg. Both speak the same MediaCmd/MediaMsg.
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
            let (msg_tx, msg_rx) = std::sync::mpsc::channel();
            // Data plane for decoded video frames (latest-wins, Arc-backed). For MIDI
            // (audio-only) the sender is simply dropped, closing the empty channel.
            let (frame_tx, frame_rx) = micro_media::latest::<micro_media::Frame>();
            let ctx2 = ctx.clone();
            let p = path.to_path_buf();
            let is_midi = matches!(ext.as_str(), "mid" | "midi");
            std::thread::spawn(move || {
                let wake = move || ctx2.request_repaint();
                if is_midi {
                    drop(frame_tx);
                    midi_worker(p, cmd_rx, msg_tx, wake)
                } else {
                    media_worker(p, cmd_rx, msg_tx, frame_tx, wake)
                }
            });
            Content::Media(MediaView::new(info, cmd_tx, msg_rx, frame_rx))
        }
        Decoded::Error(e) => Content::Error(e),
        Decoded::Image {
            rgba,
            w,
            h,
            premultiplied,
            kind,
        } => {
            let size = [w as usize, h as usize];
            let color = if premultiplied {
                let pixels = rgba
                    .chunks_exact(4)
                    .map(|c| Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3]))
                    .collect();
                egui::ColorImage { size, pixels }
            } else {
                egui::ColorImage::from_rgba_unmultiplied(size, &rgba)
            };
            let texture = ctx.load_texture("image", color, TextureOptions::LINEAR);
            Content::Image(ImageView {
                texture,
                size: Vec2::new(w as f32, h as f32),
                zoom: 1.0,
                offset: Vec2::ZERO,
                kind,
            })
        }
        // `Decoded` is #[non_exhaustive]: tolerate variants added by future
        // viewer-core releases rather than failing to compile.
        _ => Content::Error("Tipo di contenuto non supportato da questa versione".into()),
    }
}

/// Whether the content is actually on screen (not a spinner/placeholder).
fn content_displayable(c: &Content) -> bool {
    match c {
        Content::Empty => false,
        Content::Pdf(p) => p.view.is_some() || p.error.is_some(),
        // The mesh paints once its GPU upload happens (next frame), but it's
        // effectively ready as soon as we hold the geometry.
        _ => true,
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let is_first = self.first_frame;
        // Capture the glow context once so set_content() can free a replaced
        // mesh's GPU buffers while the context is still current.
        if self.gl.is_none() {
            self.gl = frame.gl().cloned();
        }

        // Files handed over by later invocations (single instance).
        let mut incoming: Option<PathBuf> = None;
        if let Some(rx) = &self.inbox {
            while let Ok(p) = rx.try_recv() {
                incoming = Some(p);
            }
        }
        if let Some(p) = incoming {
            // Same file again while visible → toggle closed; otherwise show it.
            if self.quicklook && !self.hidden && self.file_path.as_deref() == Some(p.as_path()) {
                self.dismiss(ctx);
            } else {
                self.open(ctx, p);
                self.reveal(ctx);
            }
        }

        // Parallel pipeline: collect a finished decode and finalize it.
        if let Some((_, rx)) = &self.pending_load {
            match rx.try_recv() {
                Ok(decoded) => {
                    let (path, _) = self.pending_load.take().unwrap();
                    self.finalize(ctx, &path, decoded);
                }
                // The worker wakes us via wake_ui() when done; this is just a
                // safety net, so the main thread stays idle and the worker keeps
                // a full core (no allocator/CPU contention during decode).
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(200))
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending_load = None;
                }
            }
        }

        // Quick Look overlay: dismiss on Space (unless typing) or on focus loss.
        if self.quicklook && !self.hidden {
            let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
            if focused {
                self.ever_focused = true;
            }
            // Space toggles playback for media; only dismiss with it otherwise.
            let is_media = matches!(self.content, Content::Media(_));
            let space = ctx.input(|i| i.key_pressed(egui::Key::Space));
            if (space && !is_media && !ctx.wants_keyboard_input())
                || (self.ever_focused && !focused)
            {
                self.dismiss(ctx);
            }
        }
        // Resident idle timeout: quit if hidden too long.
        if self.hidden {
            if let Some(t) = self.hidden_since {
                if t.elapsed() >= Self::IDLE_QUIT {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    ctx.request_repaint_after(Self::IDLE_QUIT - t.elapsed());
                }
            }
        }

        // Drag & drop.
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(file) = dropped.into_iter().find_map(|f| f.path) {
            self.open(ctx, file);
        }

        // Keyboard: Ctrl+O open, F fullscreen, Esc exit fullscreen / dismiss.
        // Guard single-key shortcuts so typing in the filter field is unaffected.
        let typing = ctx.wants_keyboard_input();
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.open_dialog(ctx);
        }
        // Ctrl+C: copy the file (skip when a text field has focus so its own
        // selection-copy still works).
        if !typing && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) {
            self.copy_current(ctx);
        }
        if !typing && ctx.input(|i| i.key_pressed(egui::Key::F)) {
            self.toggle_fullscreen(ctx);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.fullscreen {
                self.toggle_fullscreen(ctx);
            } else {
                self.dismiss(ctx);
            }
        }
        // PDF page navigation with PageUp / PageDown (the arrows step through the
        // folder instead — see below).
        if let Content::Pdf(pdf) = &mut self.content {
            let next = ctx.input(|i| i.key_pressed(egui::Key::PageDown));
            let prev = ctx.input(|i| i.key_pressed(egui::Key::PageUp));
            if next && pdf.page + 1 < pdf.pages {
                pdf.page += 1;
            }
            if prev && pdf.page > 0 {
                pdf.page -= 1;
            }
        }

        // Media transport keys: Space play/pause, arrows seek ±5s. These take the
        // arrows over folder navigation while a clip is open.
        if let Content::Media(view) = &mut self.content {
            if !typing && ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                view.toggle_play();
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                view.seek_by(5.0);
            } else if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                view.seek_by(-5.0);
            }
        } else if !typing {
            // Arrow keys: scroll through the openable files in the current folder.
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                self.navigate_dir(ctx, 1);
            } else if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                self.navigate_dir(ctx, -1);
            }
        }

        // Frosted, more translucent than the content panels so it reads as glass.
        let bar_frame = egui::Frame::default()
            .fill(egui::Color32::from_rgba_unmultiplied(28, 29, 36, 205))
            .inner_margin(egui::Margin::symmetric(8.0, 6.0));
        egui::TopBottomPanel::top("toolbar").frame(bar_frame).show(ctx, |ui| {
            // Frameless window: the toolbar background is a window-drag handle.
            // Register it FIRST so the buttons drawn next sit on top and keep
            // receiving their clicks (egui gives priority to later widgets).
            let drag_rect = egui::Rect::from_min_size(
                ui.max_rect().min,
                egui::vec2(ui.available_width(), ui.spacing().interact_size.y + 8.0),
            );
            let drag =
                ui.interact(drag_rect, ui.id().with("windowdrag"), egui::Sense::click_and_drag());
            if drag.drag_started_by(egui::PointerButton::Primary) {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }

            ui.horizontal(|ui| {
                if ui.button("📂 Apri").clicked() {
                    self.open_dialog(ctx);
                }
                ui.separator();
                self.toolbar_extras(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✕").on_hover_text("Chiudi (Esc)").clicked() {
                        self.dismiss(ctx);
                    }
                    if ui
                        .button("⛶")
                        .on_hover_text("Schermo intero (F)")
                        .clicked()
                    {
                        self.toggle_fullscreen(ctx);
                    }
                    ui.separator();
                    if !self.file_name.is_empty() {
                        ui.label(&self.file_name);
                    }
                    // Brief "copied" feedback after Ctrl+C.
                    if let Some((msg, t)) = &self.copy_status {
                        if t.elapsed() < std::time::Duration::from_secs(2) {
                            ui.separator();
                            ui.colored_label(Color32::from_rgb(120, 200, 120), format!("✔ {msg}"));
                            ctx.request_repaint_after(std::time::Duration::from_millis(250));
                        }
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match &mut self.content {
            Content::Empty => {
                if self.pending_load.is_some() {
                    ui.centered_and_justified(|ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.add_space(6.0);
                            let name = &self.file_name;
                            ui.label(if name.is_empty() {
                                "Caricamento…".to_string()
                            } else {
                                format!("Caricamento di {name}…")
                            });
                        });
                    });
                    // Animate the spinner while decoding. Throttled to ~30 fps so
                    // the UI thread barely competes with the decode worker.
                    ctx.request_repaint_after(std::time::Duration::from_millis(33));
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            "Trascina qui un file: CSV, immagine, SVG, Excel/Word/PowerPoint, PDF, 3D (OBJ/glTF/STL), testo/codice\n(oppure premi Apri / Ctrl+O)",
                        );
                    });
                }
            }
            Content::Error(e) => {
                ui.centered_and_justified(|ui| {
                    ui.colored_label(Color32::from_rgb(220, 80, 80), e.clone());
                });
            }
            Content::Text(view) => text_view(ui, view),
            Content::Media(view) => media_view(ui, view, &self.file_name),
            Content::Csv(data) => csv_table(ui, data),
            Content::Sheets(sd) => csv_table(ui, &sd.sheets[sd.current].1),
            Content::Image(view) => image_view(ui, view),
            Content::Mesh(view) => mesh_view(ui, view, frame.gl()),
            Content::Markdown(text) => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.md_cache, text);
                });
            }
            Content::Pdf(pdf) => {
                // Start the render worker one frame after the window is shown.
                if !is_first {
                    if let Some((bytes, req_rx, res_tx)) = pdf.spawn.take() {
                        let ctx2 = ctx.clone();
                        std::thread::spawn(move || {
                            pdf_worker(bytes, req_rx, res_tx, move || ctx2.request_repaint())
                        });
                    }
                }
                if pdf.spawn.is_some() {
                    ctx.request_repaint();
                }
                // Consume any results produced by the worker thread.
                while let Ok(msg) = pdf.res_rx.try_recv() {
                    match msg {
                        PdfMsg::Meta(count) => pdf.pages = count,
                        PdfMsg::Page { page, rgba, w, h } => {
                            if page == pdf.page {
                                let color = egui::ColorImage::from_rgba_unmultiplied(
                                    [w as usize, h as usize],
                                    &rgba,
                                );
                                let texture =
                                    ctx.load_texture("pdf", color, TextureOptions::LINEAR);
                                pdf.view = Some(ImageView {
                                    texture,
                                    size: Vec2::new(w as f32, h as f32),
                                    zoom: 1.0,
                                    offset: Vec2::ZERO,
                                    kind: String::new(),
                                });
                                pdf.rendered = page;
                            }
                        }
                        // The app doesn't draw the text layer yet; the worker
                        // serves it for library consumers (click-to-locate).
                        PdfMsg::Text { .. } => {}
                        PdfMsg::Err(e) => pdf.error = Some(e),
                    }
                }
                // Request the current page if it isn't rendered or already pending.
                if pdf.error.is_none() && pdf.rendered != pdf.page && pdf.dispatched != pdf.page {
                    let _ = pdf.req_tx.send(PdfReq::Render(pdf.page));
                    pdf.dispatched = pdf.page;
                }

                if let Some(e) = &pdf.error {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(Color32::from_rgb(220, 80, 80), e);
                    });
                } else if let Some(view) = &mut pdf.view {
                    image_view(ui, view);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                }
            }
        });

        // Benchmark checkpoints (VIEWER_BENCH): window shown, then content ready.
        if self.first_frame {
            self.first_frame = false;
            bench("first_frame", "");
        }
        if !self.reported_ready && content_displayable(&self.content) {
            self.reported_ready = true;
            bench("content_ready", "");
            if std::env::var_os("VIEWER_BENCH").is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Transparent clear colour so the panels paint their own (frosted) fills
    /// over the desktop — the basis for the translucent toolbar / window edges.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    /// Free GPU resources the egui texture manager doesn't own (the 3D mesh's
    /// buffers/program) while the glow context is still alive.
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let (Content::Mesh(view), Some(gl)) = (&self.content, gl) {
            view.destroy(gl);
        }
    }
}
