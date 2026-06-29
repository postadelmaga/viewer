//! Audio/video playback engine (FFmpeg + cpal), behind the `media` feature.
//!
//! Unlike the other formats, media is *streamed*, not decoded into one blob: a
//! file can be gigabytes and runs in time, needing an audio device and a clock.
//! So this module mirrors the `pdf` worker pattern but further: [`media_worker`]
//! runs the demux/decode loop on its own thread, plays audio through cpal, paces
//! video frames to the audio clock, and answers [`MediaCmd`]s (play/pause/seek).
//! The UI owns the player and just paints the frames it receives.
//!
//! The `MediaInfo` type is always compiled (so `Decoded::Media` exists), but the
//! engine itself needs the feature; a build without it reports "not compiled".

use crate::{Decoded, Family, Format, Input};

/// Media formats. Registered unconditionally so the extensions are recognised
/// even without the `media` feature (they then decode to a "not compiled"
/// error). `Family::Media` makes `decode` skip the read-all step — FFmpeg
/// streams straight from the path, so the file is never loaded whole into RAM.
pub(crate) const FORMATS: &[Format] = &[Format {
    exts: &[
        // Video containers
        "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "ts", "ogv", "3gp",
        // Audio
        "mp3", "flac", "wav", "ogg", "oga", "opus", "m4a", "aac", "wma",
    ],
    family: Family::Media,
    decode: media_entry,
}];

fn media_entry(input: Input) -> Decoded {
    probe(&input.path)
}

/// Lightweight probe result handed to the UI so it can size the window and show
/// the transport before any frame is decoded.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    /// Total duration in seconds (0 if unknown).
    pub duration: f64,
    pub has_video: bool,
    pub has_audio: bool,
}

/// Control-plane message from the playback worker to the UI: small status events,
/// fine to send over the std channel. Decoded video frames do *not* travel here —
/// they ride the `micro-media` data plane (a latest-wins `Frame` channel), so a
/// momentary UI stall drops stale frames instead of queueing megabytes of pixels.
pub enum MediaMsg {
    /// Current playback position, seconds.
    Position(f64),
    /// Reached end of stream (playback paused at the end).
    Eof,
    Error(String),
}

/// Control command from the UI to the playback worker.
pub enum MediaCmd {
    TogglePlay,
    Play,
    Pause,
    /// Seek to an absolute position in seconds.
    Seek(f64),
    /// Stop and tear down the worker.
    Stop,
}

#[cfg(not(feature = "media"))]
pub fn probe(_path: &std::path::Path) -> Decoded {
    Decoded::Error(
        "Supporto audio/video non compilato.\nAbilita la feature `media` di viewer-core.".into(),
    )
}

#[cfg(feature = "media")]
pub use engine::{media_worker, probe};

#[cfg(feature = "media")]
mod engine {
    use super::*;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, Sender, TryRecvError};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use ffmpeg_the_third as ff;
    use micro_media::{Frame, LatestSender, PixelFormat};

    const AV_TIME_BASE: f64 = 1_000_000.0;
    /// Cap the decoded frame width so texture uploads stay cheap; the UI scales
    /// the texture to the window anyway.
    const MAX_W: u32 = 1280;

    /// Probe container metadata without decoding (reads headers only).
    pub fn probe(path: &Path) -> Decoded {
        match probe_inner(path) {
            Ok(info) => Decoded::Media(info),
            Err(e) => Decoded::Error(format!("Media non leggibile:\n{e}")),
        }
    }

    fn probe_inner(path: &Path) -> Result<MediaInfo, ff::Error> {
        ff::init()?;
        let ictx = ff::format::input(path)?;
        let video = ictx.streams().best(ff::media::Type::Video);
        let audio = ictx.streams().best(ff::media::Type::Audio);
        let (mut width, mut height) = (0u32, 0u32);
        if let Some(s) = &video {
            let ctx = ff::codec::context::Context::from_parameters(s.parameters())?;
            if let Ok(dec) = ctx.decoder().video() {
                width = dec.width();
                height = dec.height();
            }
        }
        let duration = (ictx.duration() as f64 / AV_TIME_BASE).max(0.0);
        Ok(MediaInfo {
            width,
            height,
            duration,
            has_video: video.is_some(),
            has_audio: audio.is_some(),
        })
    }

    /// Run the whole playback engine until told to stop (or the channel drops).
    ///
    /// Two output channels, by intent: `msg_tx` is the **control plane** (status
    /// events over the std channel) and `frame_tx` is the **data plane** (a
    /// latest-wins [`Frame`] mailbox from `micro-media`). Audio-only sources never
    /// touch `frame_tx`; it simply stays empty.
    pub fn media_worker<F: Fn() + Send + 'static>(
        path: PathBuf,
        cmd_rx: Receiver<MediaCmd>,
        msg_tx: Sender<MediaMsg>,
        frame_tx: LatestSender<Frame>,
        wake: F,
    ) {
        if let Err(e) = run(&path, &cmd_rx, &msg_tx, &frame_tx, &wake) {
            let _ = msg_tx.send(MediaMsg::Error(e));
            wake();
        }
    }

    /// cpal output plus the shared state the audio callback and the clock read.
    struct AudioOut {
        _stream: cpal::Stream,
        ring: Arc<Mutex<VecDeque<f32>>>,
        /// Per-channel frames pulled by the device — the audio master clock.
        consumed: Arc<AtomicU64>,
        playing: Arc<AtomicBool>,
        rate: u32,
        channels: usize,
    }

    fn build_audio() -> Option<AudioOut> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let device = cpal::default_host().default_output_device()?;
        let config = device.default_output_config().ok()?;
        let rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let ring: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let consumed = Arc::new(AtomicU64::new(0));
        let playing = Arc::new(AtomicBool::new(true));

        let (r, c, p) = (ring.clone(), consumed.clone(), playing.clone());
        let stream = device
            .build_output_stream(
                &config.config(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !p.load(Ordering::Relaxed) {
                        data.iter_mut().for_each(|s| *s = 0.0);
                        return;
                    }
                    let mut ring = r.lock().unwrap();
                    let mut got = 0usize;
                    for s in data.iter_mut() {
                        match ring.pop_front() {
                            Some(v) => {
                                *s = v;
                                got += 1;
                            }
                            None => *s = 0.0,
                        }
                    }
                    // Only real samples advance the clock; an underrun (silence)
                    // doesn't, so video stays paced to actual audio output.
                    c.fetch_add((got / channels.max(1)) as u64, Ordering::Relaxed);
                },
                |e| eprintln!("audio stream error: {e}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(AudioOut {
            _stream: stream,
            ring,
            consumed,
            playing,
            rate,
            channels,
        })
    }

    /// Master playback clock: audio-driven when there's sound, else wall-clock.
    enum Clock {
        Audio {
            consumed: Arc<AtomicU64>,
            rate: u32,
            base: f64,
        },
        Wall {
            base: f64,
            anchor: Instant,
            playing: bool,
        },
    }

    impl Clock {
        fn now(&self) -> f64 {
            match self {
                Clock::Audio {
                    consumed,
                    rate,
                    base,
                } => base + consumed.load(Ordering::Relaxed) as f64 / *rate as f64,
                Clock::Wall {
                    base,
                    anchor,
                    playing,
                } => {
                    if *playing {
                        base + anchor.elapsed().as_secs_f64()
                    } else {
                        *base
                    }
                }
            }
        }
        /// Re-anchor to `t` seconds (after a seek; the audio counter is reset by
        /// the caller so only `base` needs to move).
        fn seek(&mut self, t: f64) {
            match self {
                Clock::Audio { base, .. } => *base = t,
                Clock::Wall { base, anchor, .. } => {
                    *base = t;
                    *anchor = Instant::now();
                }
            }
        }
        fn set_playing(&mut self, play: bool) {
            if let Clock::Wall {
                base,
                anchor,
                playing,
            } = self
            {
                if *playing && !play {
                    *base += anchor.elapsed().as_secs_f64();
                } else if !*playing && play {
                    *anchor = Instant::now();
                }
                *playing = play;
            }
        }
    }

    fn run<F: Fn()>(
        path: &Path,
        cmd_rx: &Receiver<MediaCmd>,
        msg_tx: &Sender<MediaMsg>,
        frame_tx: &LatestSender<Frame>,
        wake: &F,
    ) -> Result<(), String> {
        ff::init().map_err(|e| e.to_string())?;
        let mut ictx = ff::format::input(path).map_err(|e| e.to_string())?;

        let v_idx = ictx.streams().best(ff::media::Type::Video).map(|s| s.index());
        let a_idx = ictx.streams().best(ff::media::Type::Audio).map(|s| s.index());
        if v_idx.is_none() && a_idx.is_none() {
            return Err("Nessuna traccia audio o video".into());
        }

        // Video decoder + RGBA scaler.
        let mut video = match v_idx {
            Some(i) => Some(setup_video(&ictx, i)?),
            None => None,
        };
        // Audio output + decoder + resampler.
        let audio_out = build_audio();
        let mut audio = match (a_idx, &audio_out) {
            (Some(i), Some(out)) => Some(setup_audio(&ictx, i, out)?),
            _ => None,
        };

        let mut clock = match &audio_out {
            Some(out) => Clock::Audio {
                consumed: out.consumed.clone(),
                rate: out.rate,
                base: 0.0,
            },
            None => Clock::Wall {
                base: 0.0,
                anchor: Instant::now(),
                playing: true,
            },
        };

        let mut playing = true;
        let mut vframe = ff::frame::Video::empty();
        let mut aframe = ff::frame::Audio::empty();

        loop {
            let mut pending_seek: Option<f64> = None;
            let mut stop = false;
            let mut eof = false;

            'packets: {
                let mut packets = ictx.packets();
                loop {
                    // Drain controls (non-blocking).
                    match poll_cmds(cmd_rx, &mut playing, &audio_out, &mut clock) {
                        Control::None => {}
                        Control::Seek(t) => {
                            pending_seek = Some(t);
                            break 'packets;
                        }
                        Control::Stop => {
                            stop = true;
                            break 'packets;
                        }
                    }
                    // Paused: block until something changes (no decoding ahead).
                    if !playing {
                        match wait_cmd(cmd_rx, &mut playing, &audio_out, &mut clock) {
                            Control::Seek(t) => {
                                pending_seek = Some(t);
                                break 'packets;
                            }
                            Control::Stop => {
                                stop = true;
                                break 'packets;
                            }
                            Control::None => continue,
                        }
                    }

                    let Some(item) = packets.next() else {
                        eof = true;
                        break 'packets;
                    };
                    let (stream, packet) = match item {
                        Ok(sp) => sp,
                        Err(_) => continue,
                    };
                    let idx = stream.index();

                    if Some(idx) == v_idx {
                        if let Some(v) = &mut video {
                            let _ = v.decoder.send_packet(&packet);
                            while v.decoder.receive_frame(&mut vframe).is_ok() {
                                let pts = frame_seconds(vframe.pts(), v.time_base)
                                    .unwrap_or_else(|| clock.now());
                                // Pace to the master clock; abort the wait on a
                                // seek/stop so controls stay responsive.
                                match present_wait(
                                    pts, cmd_rx, &mut playing, &audio_out, &mut clock,
                                ) {
                                    Control::Seek(t) => {
                                        pending_seek = Some(t);
                                        break 'packets;
                                    }
                                    Control::Stop => {
                                        stop = true;
                                        break 'packets;
                                    }
                                    Control::None => {}
                                }
                                let rgba = scale_to_rgba(&mut v.scaler, &vframe, v.out_w, v.out_h);
                                // Hand the pixels to the data plane: latest-wins, so if the
                                // UI is mid-stall the previous undrawn frame is dropped rather
                                // than queued. A `Disconnected` means the view is gone — stop.
                                match Frame::new(v.out_w, v.out_h, PixelFormat::Rgba8, rgba) {
                                    Ok(frame) => {
                                        if frame_tx.send(frame).is_err() {
                                            return Ok(());
                                        }
                                    }
                                    Err(e) => eprintln!("frame build error: {e}"),
                                }
                                let _ = msg_tx.send(MediaMsg::Position(pts));
                                wake();
                            }
                        }
                    } else if Some(idx) == a_idx {
                        if let (Some(a), Some(out)) = (&mut audio, &audio_out) {
                            let _ = a.decoder.send_packet(&packet);
                            while a.decoder.receive_frame(&mut aframe).is_ok() {
                                push_audio(a, out, &aframe);
                            }
                            let _ = msg_tx.send(MediaMsg::Position(clock.now()));
                            wake();
                        }
                    }

                    // Backpressure for audio-only streams (video is paced by
                    // present_wait): if the ring holds >~2s, pause a beat. Commands
                    // stay queued and are polled at the top of the next iteration.
                    if let Some(out) = &audio_out {
                        if playing && over_buffered(out) {
                            std::thread::sleep(Duration::from_millis(15));
                        }
                    }
                }
            }

            if stop {
                return Ok(());
            }
            if let Some(t) = pending_seek {
                seek(&mut ictx, t);
                if let Some(v) = &mut video {
                    v.decoder.flush();
                }
                if let Some(a) = &mut audio {
                    a.decoder.flush();
                }
                if let Some(out) = &audio_out {
                    out.ring.lock().unwrap().clear();
                    out.consumed.store(0, Ordering::Relaxed);
                }
                clock.seek(t);
                let _ = msg_tx.send(MediaMsg::Position(t));
                wake();
                continue;
            }
            if eof {
                playing = false;
                if let Some(out) = &audio_out {
                    out.playing.store(false, Ordering::Relaxed);
                }
                clock.set_playing(false);
                let _ = msg_tx.send(MediaMsg::Eof);
                wake();
                // Idle until a seek or stop.
                match wait_cmd(cmd_rx, &mut playing, &audio_out, &mut clock) {
                    Control::Seek(t) => {
                        seek(&mut ictx, t);
                        if let Some(v) = &mut video {
                            v.decoder.flush();
                        }
                        if let Some(a) = &mut audio {
                            a.decoder.flush();
                        }
                        if let Some(out) = &audio_out {
                            out.ring.lock().unwrap().clear();
                            out.consumed.store(0, Ordering::Relaxed);
                        }
                        clock.seek(t);
                        continue;
                    }
                    Control::Stop => return Ok(()),
                    Control::None => return Ok(()),
                }
            }
        }
    }

    enum Control {
        None,
        Seek(f64),
        Stop,
    }

    fn apply_play(playing: &mut bool, on: bool, audio: &Option<AudioOut>, clock: &mut Clock) {
        *playing = on;
        if let Some(out) = audio {
            out.playing.store(on, Ordering::Relaxed);
        }
        clock.set_playing(on);
    }

    fn poll_cmds(
        cmd_rx: &Receiver<MediaCmd>,
        playing: &mut bool,
        audio: &Option<AudioOut>,
        clock: &mut Clock,
    ) -> Control {
        loop {
            match cmd_rx.try_recv() {
                Ok(MediaCmd::Play) => apply_play(playing, true, audio, clock),
                Ok(MediaCmd::Pause) => apply_play(playing, false, audio, clock),
                Ok(MediaCmd::TogglePlay) => apply_play(playing, !*playing, audio, clock),
                Ok(MediaCmd::Seek(t)) => return Control::Seek(t.max(0.0)),
                Ok(MediaCmd::Stop) => return Control::Stop,
                Err(TryRecvError::Empty) => return Control::None,
                Err(TryRecvError::Disconnected) => return Control::Stop,
            }
        }
    }

    /// Block for one command (used while paused / at EOF).
    fn wait_cmd(
        cmd_rx: &Receiver<MediaCmd>,
        playing: &mut bool,
        audio: &Option<AudioOut>,
        clock: &mut Clock,
    ) -> Control {
        match cmd_rx.recv() {
            Ok(MediaCmd::Play) => {
                apply_play(playing, true, audio, clock);
                Control::None
            }
            Ok(MediaCmd::Pause) => {
                apply_play(playing, false, audio, clock);
                Control::None
            }
            Ok(MediaCmd::TogglePlay) => {
                apply_play(playing, !*playing, audio, clock);
                Control::None
            }
            Ok(MediaCmd::Seek(t)) => Control::Seek(t.max(0.0)),
            Ok(MediaCmd::Stop) | Err(_) => Control::Stop,
        }
    }

    /// Wait until the master clock reaches `pts`, staying responsive to controls.
    fn present_wait(
        pts: f64,
        cmd_rx: &Receiver<MediaCmd>,
        playing: &mut bool,
        audio: &Option<AudioOut>,
        clock: &mut Clock,
    ) -> Control {
        loop {
            match poll_cmds(cmd_rx, playing, audio, clock) {
                Control::None => {}
                other => return other,
            }
            if !*playing {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let dt = pts - clock.now();
            if dt <= 0.0 {
                return Control::None;
            }
            std::thread::sleep(Duration::from_secs_f64(dt.min(0.05)));
        }
    }

    /// Whether the audio ring already holds more than ~2 seconds of samples.
    fn over_buffered(out: &AudioOut) -> bool {
        out.ring.lock().unwrap().len() as f64 / (out.rate as f64 * out.channels as f64) > 2.0
    }

    struct VideoDec {
        decoder: ff::decoder::Video,
        scaler: ff::software::scaling::Context,
        out_w: u32,
        out_h: u32,
        time_base: f64,
    }

    fn setup_video(ictx: &ff::format::context::Input, idx: usize) -> Result<VideoDec, String> {
        let stream = ictx.stream(idx).ok_or("stream video assente")?;
        let time_base = f64::from(stream.time_base());
        let ctx = ff::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| e.to_string())?;
        let decoder = ctx.decoder().video().map_err(|e| e.to_string())?;
        let (iw, ih) = (decoder.width(), decoder.height());
        let out_w = iw.clamp(2, MAX_W) & !1;
        let out_h = ((ih as u64 * out_w as u64 / iw.max(1) as u64) as u32).max(2) & !1;
        let scaler = ff::software::scaling::Context::get(
            decoder.format(),
            iw,
            ih,
            ff::format::Pixel::RGBA,
            out_w,
            out_h,
            ff::software::scaling::Flags::BILINEAR,
        )
        .map_err(|e| e.to_string())?;
        Ok(VideoDec {
            decoder,
            scaler,
            out_w,
            out_h,
            time_base,
        })
    }

    struct AudioDec {
        decoder: ff::decoder::Audio,
        resampler: ff::software::resampling::Context,
    }

    fn setup_audio(
        ictx: &ff::format::context::Input,
        idx: usize,
        out: &AudioOut,
    ) -> Result<AudioDec, String> {
        let stream = ictx.stream(idx).ok_or("stream audio assente")?;
        let ctx = ff::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| e.to_string())?;
        let decoder = ctx.decoder().audio().map_err(|e| e.to_string())?;
        // Resample the decoder's native format to interleaved f32 at the device's
        // layout/rate. `resampler2` uses the new (FFmpeg 7+) channel-layout API.
        let out_layout = ff::ChannelLayout::default_for_channels(out.channels as u32);
        let resampler = decoder
            .resampler2(
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                out_layout,
                out.rate,
            )
            .map_err(|e| e.to_string())?;
        Ok(AudioDec { decoder, resampler })
    }

    fn push_audio(a: &mut AudioDec, out: &AudioOut, aframe: &ff::frame::Audio) {
        let mut resampled = ff::frame::Audio::empty();
        if a.resampler.run(aframe, &mut resampled).is_err() {
            return;
        }
        let n = resampled.samples() * out.channels;
        let bytes = resampled.data(0);
        let avail = bytes.len() / std::mem::size_of::<f32>();
        let count = n.min(avail);
        let floats =
            unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const f32, count) };
        out.ring.lock().unwrap().extend(floats.iter().copied());
    }

    /// Convert the decoded frame to tightly-packed RGBA, dropping scaler padding.
    fn scale_to_rgba(
        scaler: &mut ff::software::scaling::Context,
        src: &ff::frame::Video,
        w: u32,
        h: u32,
    ) -> Vec<u8> {
        let mut dst = ff::frame::Video::empty();
        if scaler.run(src, &mut dst).is_err() {
            return vec![0; (w * h * 4) as usize];
        }
        let stride = dst.stride(0);
        let row = (w * 4) as usize;
        let data = dst.data(0);
        let mut out = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            let start = y * stride;
            out.extend_from_slice(&data[start..start + row]);
        }
        out
    }

    fn frame_seconds(pts: Option<i64>, time_base: f64) -> Option<f64> {
        pts.map(|p| p as f64 * time_base)
    }

    fn seek(ictx: &mut ff::format::context::Input, t: f64) {
        let ts = (t * AV_TIME_BASE) as i64;
        let _ = ictx.seek(ts, ..ts);
    }
}
