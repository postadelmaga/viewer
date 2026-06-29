//! Audio/video player UI: paints frames from the [`media_worker`] and drives it.
//!
//! The heavy lifting (demux/decode/audio/clock) lives in `viewer_core::media`;
//! this just drains decoded frames into a texture, draws it fit-to-window, and
//! turns the transport controls into [`MediaCmd`]s. Mirrors how `pdf_view` talks
//! to `pdf_worker`.

use eframe::egui;
use egui::{Color32, TextureHandle, TextureOptions, Vec2};
use micro_media::LatestReceiver;
use micro_media::Frame;
use std::sync::mpsc::{Receiver, Sender};
use viewer_core::media::{MediaCmd, MediaMsg};
use viewer_core::MediaInfo;

pub(crate) struct MediaView {
    pub(crate) info: MediaInfo,
    cmd_tx: Sender<MediaCmd>,
    msg_rx: Receiver<MediaMsg>,
    /// Data plane: decoded video frames, latest-wins. Empty for audio-only sources.
    frame_rx: LatestReceiver<Frame>,
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
}

impl MediaView {
    pub(crate) fn new(
        info: MediaInfo,
        cmd_tx: Sender<MediaCmd>,
        msg_rx: Receiver<MediaMsg>,
        frame_rx: LatestReceiver<Frame>,
    ) -> Self {
        Self {
            info,
            cmd_tx,
            msg_rx,
            frame_rx,
            texture: None,
            tex_size: Vec2::ZERO,
            position: 0.0,
            playing: true,
            ended: false,
            error: None,
            scrubbing: false,
            scrub_pos: 0.0,
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
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.pixels,
            );
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

/// Transport controls, drawn in the toolbar.
pub(crate) fn media_toolbar(ui: &mut egui::Ui, view: &mut MediaView) {
    let label = if view.ended {
        "↻"
    } else if view.playing {
        "⏸"
    } else {
        "▶"
    };
    if ui.button(label).on_hover_text("Play/Pausa (Spazio)").clicked() {
        view.toggle_play();
    }

    let dur = view.info.duration.max(0.0);
    let shown = if view.scrubbing {
        view.scrub_pos
    } else {
        view.position
    };
    ui.label(format!("{} / {}", fmt_time(shown), fmt_time(dur)));

    if dur > 0.0 {
        let mut pos = shown;
        let resp = ui.add(
            egui::Slider::new(&mut pos, 0.0..=dur)
                .show_value(false)
                .trailing_fill(true),
        );
        if resp.dragged() {
            view.scrubbing = true;
            view.scrub_pos = pos;
        }
        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
            view.scrubbing = false;
            view.position = pos;
            view.ended = false;
            view.send(MediaCmd::Seek(pos));
        }
    }
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

    match (&view.texture, view.info.has_video) {
        (Some(tex), true) => {
            let avail = ui.available_size();
            let s = (avail.x / view.tex_size.x.max(1.0))
                .min(avail.y / view.tex_size.y.max(1.0))
                .max(0.0);
            let size = view.tex_size * s;
            let rect = egui::Rect::from_center_size(ui.max_rect().center(), size);
            egui::Image::new((tex.id(), size)).paint_at(ui, rect);
        }
        _ => {
            // Audio-only (or video not yet decoded): a simple centered card.
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.heading(if view.info.has_video { "▶" } else { "♪" });
                    ui.add_space(6.0);
                    ui.label(file_name);
                });
            });
        }
    }
}
