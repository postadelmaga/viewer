//! Audio/video player UI: paints frames from the [`media_worker`] and drives it.
//!
//! The heavy lifting (demux/decode/audio/clock) lives in `viewer_core::media`;
//! this just drains decoded frames into a texture, draws it fit-to-window, and
//! turns the transport controls into [`MediaCmd`]s. Mirrors how `pdf_view` talks
//! to `pdf_worker`.

use eframe::egui;
use egui::{Color32, Pos2, TextureHandle, TextureOptions, Vec2};
use micro_media::{Frame, LatestReceiver, PixelFormat, Scope};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;
use viewer_core::media::{MediaCmd, MediaMsg};
use viewer_core::MediaInfo;

pub(crate) struct MediaView {
    pub(crate) info: MediaInfo,
    cmd_tx: Sender<MediaCmd>,
    msg_rx: Receiver<MediaMsg>,
    /// Data plane: decoded video frames, latest-wins. Empty for audio-only sources.
    frame_rx: LatestReceiver<Frame>,
    /// Tap of recently-played audio samples, drawn as the oscilloscope.
    scope: Scope,
    /// Scratch buffer for the latest oscilloscope window (reused each repaint).
    scope_buf: Vec<f32>,
    texture: Option<TextureHandle>,
    tex_size: Vec2,
    position: f64,
    playing: bool,
    ended: bool,
    error: Option<String>,
    /// While the user drags the seek bar we show their target, not the worker's
    /// reported position, and only seek on release.
    scrubbing: bool,
    scrub_pos: f64,
    /// Auto-hide bookkeeping for the video overlay controls.
    last_activity: Instant,
    last_pointer: Option<Pos2>,
    /// Set when the user double-clicks the video; the app consumes it to toggle
    /// fullscreen (app-level state lives there, not in the view).
    pub(crate) pending_fullscreen: bool,
}

impl MediaView {
    pub(crate) fn new(
        info: MediaInfo,
        cmd_tx: Sender<MediaCmd>,
        msg_rx: Receiver<MediaMsg>,
        frame_rx: LatestReceiver<Frame>,
        scope: Scope,
    ) -> Self {
        Self {
            info,
            cmd_tx,
            msg_rx,
            frame_rx,
            scope,
            scope_buf: Vec::new(),
            texture: None,
            tex_size: Vec2::ZERO,
            position: 0.0,
            playing: true,
            ended: false,
            error: None,
            scrubbing: false,
            scrub_pos: 0.0,
            last_activity: Instant::now(),
            last_pointer: None,
            pending_fullscreen: false,
        }
    }

    fn send(&self, cmd: MediaCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub(crate) fn toggle_play(&mut self) {
        if self.ended {
            // Restart from the top when toggling after the end.
            self.send(MediaCmd::Seek(0.0));
            self.ended = false;
            self.playing = true;
            self.send(MediaCmd::Play);
        } else {
            self.playing = !self.playing;
            self.send(MediaCmd::TogglePlay);
        }
    }

    pub(crate) fn seek_by(&mut self, delta: f64) {
        let t = (self.position + delta).clamp(0.0, self.info.duration.max(0.0));
        self.position = t;
        self.ended = false;
        self.send(MediaCmd::Seek(t));
    }

    /// Tell the worker to shut down (called when the view is dropped/replaced).
    pub(crate) fn stop(&self) {
        self.send(MediaCmd::Stop);
    }

    /// Drain decoded frames (data plane) and status (control plane) into view state.
    fn pump(&mut self, ctx: &egui::Context) {
        // Data plane: the channel is latest-wins, so this loop sees at most the one
        // freshest frame the worker decoded since the last paint — never a backlog.
        while let Ok(Some(frame)) = self.frame_rx.try_recv() {
            let size = [frame.width as usize, frame.height as usize];
            // Honour the frame's declared layout. The engine emits Rgba8 today,
            // but Bgra8 (common GPU swapchain order) would otherwise paint with
            // red/blue swapped, so convert it instead of assuming RGBA.
            let image = match frame.format {
                PixelFormat::Rgba8 => egui::ColorImage::from_rgba_unmultiplied(size, &frame.pixels),
                PixelFormat::Bgra8 => {
                    let pixels = frame
                        .pixels
                        .chunks_exact(4)
                        .map(|p| Color32::from_rgba_unmultiplied(p[2], p[1], p[0], p[3]))
                        .collect();
                    egui::ColorImage { size, pixels }
                }
            };
            self.tex_size = Vec2::new(frame.width as f32, frame.height as f32);
            match &mut self.texture {
                Some(tex) => tex.set(image, TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("media", image, TextureOptions::LINEAR)),
            }
        }
        // Control plane: small status events.
        while let Ok(msg) = self.msg_rx.try_recv() {
            match msg {
                MediaMsg::Position(p) => {
                    if !self.scrubbing {
                        self.position = p;
                    }
                }
                MediaMsg::Eof => {
                    self.ended = true;
                    self.playing = false;
                    // Snap the bar to the very end: the last reported position can
                    // sit a hair short of the duration.
                    if self.info.duration > 0.0 && !self.scrubbing {
                        self.position = self.info.duration;
                    }
                }
                MediaMsg::Error(e) => self.error = Some(e),
            }
        }
    }
}

impl Drop for MediaView {
    fn drop(&mut self) {
        self.stop();
    }
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{:02}:{:02}", t / 60, t % 60)
}

const ACCENT: Color32 = Color32::from_rgb(98, 134, 248);

impl MediaView {
    fn play_label(&self) -> &'static str {
        if self.ended {
            "↻"
        } else if self.playing {
            "⏸"
        } else {
            "▶"
        }
    }
    /// Position to display: the scrub target while dragging, else the worker's.
    fn shown_pos(&self) -> f64 {
        if self.scrubbing {
            self.scrub_pos
        } else {
            self.position
        }
    }
    fn commit_seek(&mut self, pos: f64) {
        self.scrubbing = false;
        self.position = pos.clamp(0.0, self.info.duration.max(0.0));
        self.ended = false;
        self.send(MediaCmd::Seek(self.position));
    }
}

/// A full-width seek/progress bar: track + elapsed fill + handle. Click or drag
/// anywhere on it to seek; dragging updates the shown position live and commits
/// only on release.
fn seek_bar(ui: &mut egui::Ui, view: &mut MediaView, width: f32) {
    let dur = view.info.duration.max(0.0);
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(width.max(40.0), 18.0), egui::Sense::click_and_drag());
    let frac = if dur > 0.0 {
        (view.shown_pos() / dur).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };

    let track_h = 6.0;
    let track = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), track_h));
    let round = track_h * 0.5;
    let p = ui.painter();
    p.rect_filled(track, round, Color32::from_gray(70));
    let mut filled = track;
    filled.set_width(track.width() * frac);
    p.rect_filled(filled, round, ACCENT);
    let hx = track.left() + track.width() * frac;
    let hr = if resp.hovered() || resp.dragged() { 7.0 } else { 5.0 };
    p.circle_filled(egui::pos2(hx, rect.center().y), hr, Color32::WHITE);

    if dur > 0.0 {
        let frac_at = |x: f32| ((x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0) as f64;
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                view.scrubbing = true;
                view.scrub_pos = frac_at(pos.x) * dur;
            }
        }
        if resp.drag_stopped() {
            view.commit_seek(view.scrub_pos);
        }
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                view.commit_seek(frac_at(pos.x) * dur);
            }
        }
    }
}

/// The transport row — play · elapsed/total · seek bar — laid out left-to-right
/// inside whatever `ui` it is given. Shared by the audio bar and the video
/// overlay so both read and behave identically.
fn transport_row(ui: &mut egui::Ui, view: &mut MediaView) {
    if ui
        .button(view.play_label())
        .on_hover_text("Play/Pausa (Spazio)")
        .clicked()
    {
        view.toggle_play();
    }
    ui.label(format!(
        "{} / {}",
        fmt_time(view.shown_pos()),
        fmt_time(view.info.duration.max(0.0))
    ));
    if view.info.duration > 0.0 {
        let w = ui.available_width();
        seek_bar(ui, view, w);
    }
}

/// Paint a control bar filling `bar`: a translucent backdrop plus the transport
/// row. Used for both the always-visible audio bar and the auto-hiding video
/// overlay, so the seek bar always sits at the bottom of the window.
fn transport_bar(ui: &mut egui::Ui, view: &mut MediaView, bar: egui::Rect) {
    ui.painter()
        .rect_filled(bar, 0.0, Color32::from_rgba_unmultiplied(18, 19, 24, 190));
    let inner = bar.shrink2(egui::vec2(12.0, 8.0));
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.horizontal_centered(|ui| transport_row(ui, view));
    });
}

/// Draw the audio oscilloscope — the latest played-samples window as a waveform —
/// filling `rect`, with a faint baseline so silence still reads as a flat line.
fn oscilloscope(ui: &mut egui::Ui, view: &mut MediaView, rect: egui::Rect) {
    view.scope.snapshot(&mut view.scope_buf);
    let p = ui.painter();
    let mid = rect.center().y;
    p.line_segment(
        [egui::pos2(rect.left(), mid), egui::pos2(rect.right(), mid)],
        egui::Stroke::new(1.0, Color32::from_gray(60)),
    );
    let n = view.scope_buf.len();
    if n < 2 {
        return;
    }
    // Leave a little headroom so peaks at full scale don't clip the band edge.
    let amp = rect.height() * 0.45;
    let denom = (n - 1) as f32;
    let points: Vec<Pos2> = view
        .scope_buf
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let x = rect.left() + rect.width() * (i as f32 / denom);
            egui::pos2(x, mid - s.clamp(-1.0, 1.0) * amp)
        })
        .collect();
    p.add(egui::Shape::line(points, egui::Stroke::new(1.5, ACCENT)));
}

/// Central video surface (or an audio placeholder), updated each frame.
pub(crate) fn media_view(ui: &mut egui::Ui, view: &mut MediaView, file_name: &str) {
    view.pump(ui.ctx());
    // Keep the clock ticking smoothly while playing even without new frames.
    if view.playing {
        ui.ctx().request_repaint();
    }

    if let Some(e) = &view.error {
        ui.centered_and_justified(|ui| {
            ui.colored_label(Color32::from_rgb(220, 80, 80), e);
        });
        return;
    }

    if let (Some(tex), true) = (&view.texture, view.info.has_video) {
        let area = ui.max_rect();
        let avail = area.size();
        let s = (avail.x / view.tex_size.x.max(1.0))
            .min(avail.y / view.tex_size.y.max(1.0))
            .max(0.0);
        let size = view.tex_size * s;
        let rect = egui::Rect::from_center_size(area.center(), size);
        egui::Image::new((tex.id(), size)).paint_at(ui, rect);

        // Show the overlay controls while paused or shortly after pointer motion.
        let pointer = ui.input(|i| i.pointer.latest_pos());
        if pointer != view.last_pointer {
            view.last_pointer = pointer;
            view.last_activity = Instant::now();
        }
        let show = !view.playing || view.last_activity.elapsed().as_secs_f32() < 2.5;

        // Click anywhere on the video (above the bar) toggles play; double-click
        // toggles fullscreen (handled by the app, which owns that state).
        let bar_h = 46.0;
        let click_area = if show {
            egui::Rect::from_min_max(area.min, egui::pos2(area.right(), area.bottom() - bar_h))
        } else {
            area
        };
        let resp = ui.interact(click_area, ui.id().with("video_click"), egui::Sense::click());
        if resp.double_clicked() {
            view.pending_fullscreen = true;
        } else if resp.clicked() {
            view.toggle_play();
        }

        if show {
            video_overlay(ui, view, area, bar_h);
            // Re-evaluate the auto-hide a moment later even if nothing else moves.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(400));
        }
    } else {
        // Audio-only / MIDI (or video not yet decoded): a small oscilloscope card
        // above an always-visible transport bar pinned to the bottom of the window.
        let area = ui.max_rect();
        let bar_h = 46.0;
        let content =
            egui::Rect::from_min_max(area.min, egui::pos2(area.right(), area.bottom() - bar_h));
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(content), |ui| {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.heading(if view.info.has_video { "▶" } else { "♪" });
                    ui.add_space(10.0);
                    // A small scope band: most of the width, capped so it stays
                    // compact in a maximised window.
                    let w = (ui.available_width() - 24.0).clamp(120.0, 560.0);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(w, 80.0), egui::Sense::hover());
                    oscilloscope(ui, view, rect);
                    ui.add_space(10.0);
                    ui.label(file_name);
                });
            });
        });
        let bar =
            egui::Rect::from_min_max(egui::pos2(area.left(), area.bottom() - bar_h), area.max);
        transport_bar(ui, view, bar);
    }
}

/// Auto-hiding control bar drawn over the bottom of the video.
fn video_overlay(ui: &mut egui::Ui, view: &mut MediaView, area: egui::Rect, bar_h: f32) {
    let bar = egui::Rect::from_min_max(egui::pos2(area.left(), area.bottom() - bar_h), area.max);
    transport_bar(ui, view, bar);
}
