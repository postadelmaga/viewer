//! MIDI playback: parse + SoundFont synthesis (rustysynth), behind `midi`.
//!
//! MIDI isn't audio — it's a score of note events — so unlike the other media it
//! needs a synthesiser plus a SoundFont bank to make sound. We render rustysynth
//! straight inside the cpal callback (the synth is cheap, no decode-ahead), and
//! reuse the [`crate::media`] control/message protocol and the app's MediaView,
//! so a `.mid` is just another audio source with the same transport.
//!
//! The SoundFont is a runtime file (see [`soundfont_path`]); [`ensure_soundfont_background`]
//! fetches it once on first launch so playback "just works" without bundling MBs.

use crate::{Decoded, Family, Format, Input};
use std::path::PathBuf;

/// Default General MIDI SoundFont (~5.7 MB) fetched on first run. Override the
/// URL with `VIEWER_SOUNDFONT_URL`, or point `VIEWER_SOUNDFONT` at a local .sf2.
const DEFAULT_SOUNDFONT_URL: &str =
    "https://archive.org/download/free-soundfonts-sf2-2019-04/TimGM6mb.sf2";

pub(crate) const FORMATS: &[Format] = &[Format {
    exts: &["mid", "midi"],
    family: Family::Media,
    decode: midi_entry,
}];

fn midi_entry(input: Input) -> Decoded {
    probe(&input.path)
}

/// Where the SoundFont lives: `$VIEWER_SOUNDFONT`, else
/// `$XDG_DATA_HOME/viewer/soundfont.sf2`, else `~/.local/share/viewer/soundfont.sf2`.
pub fn soundfont_path() -> PathBuf {
    if let Some(p) = std::env::var_os("VIEWER_SOUNDFONT") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("viewer").join("soundfont.sf2")
}

/// On first launch, fetch the SoundFont in the background if it's missing, so a
/// later `.mid` open finds it ready. Best-effort and silent: uses `curl` then
/// `wget`, downloads to a `.part` file and renames on success. No-op if present.
pub fn ensure_soundfont_background() {
    let path = soundfont_path();
    if path.exists() {
        return;
    }
    std::thread::spawn(move || {
        let url =
            std::env::var("VIEWER_SOUNDFONT_URL").unwrap_or_else(|_| DEFAULT_SOUNDFONT_URL.into());
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("part");
        let tmp_s = tmp.to_string_lossy().to_string();
        let ok = run(&["curl", "-fLso", &tmp_s, &url]) || run(&["wget", "-qO", &tmp_s, &url]);
        // Guard against truncated/empty downloads before publishing the file.
        let big = std::fs::metadata(&tmp).map(|m| m.len() > 100_000).unwrap_or(false);
        if ok && big {
            let _ = std::fs::rename(&tmp, &path);
        } else {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("viewer: download SoundFont fallito ({url})");
        }
    });
}

fn run(args: &[&str]) -> bool {
    use std::process::{Command, Stdio};
    Command::new(args[0])
        .args(&args[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(feature = "midi"))]
pub fn probe(_path: &std::path::Path) -> Decoded {
    Decoded::Error("Supporto MIDI non compilato.\nAbilita la feature `midi`.".into())
}

#[cfg(feature = "midi")]
pub use engine::{midi_worker, probe};

#[cfg(feature = "midi")]
mod engine {
    use super::*;
    use crate::media::{MediaCmd, MediaInfo, MediaMsg};
    use std::fs::File;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};

    /// Probe duration by parsing the MIDI (no SoundFont needed). MIDI files are
    /// tiny, so reading from the path here is fine.
    pub fn probe(path: &Path) -> Decoded {
        match File::open(path).map_err(|e| e.to_string()).and_then(|mut f| {
            MidiFile::new(&mut f).map_err(|e| e.to_string())
        }) {
            Ok(midi) => Decoded::Media(MediaInfo {
                width: 0,
                height: 0,
                duration: midi.get_length().max(0.0),
                has_video: false,
                has_audio: true,
            }),
            Err(e) => Decoded::Error(format!("MIDI non valido:\n{e}")),
        }
    }

    /// Play a MIDI file: synthesise rustysynth into the cpal output, driven by
    /// the same [`MediaCmd`]/[`MediaMsg`] protocol as the A/V worker.
    pub fn midi_worker<F: Fn() + Send + 'static>(
        path: PathBuf,
        cmd_rx: Receiver<MediaCmd>,
        msg_tx: Sender<MediaMsg>,
        wake: F,
    ) {
        if let Err(e) = run_midi(&path, &cmd_rx, &msg_tx, &wake) {
            let _ = msg_tx.send(MediaMsg::Error(e));
            wake();
        }
    }

    fn run_midi<F: Fn()>(
        path: &Path,
        cmd_rx: &Receiver<MediaCmd>,
        msg_tx: &Sender<MediaMsg>,
        wake: &F,
    ) -> Result<(), String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let sf_path = soundfont_path();
        if !sf_path.exists() {
            return Err(format!(
                "SoundFont non disponibile.\nÈ in download in background, oppure mettine uno in:\n{}",
                sf_path.display()
            ));
        }

        let device = cpal::default_host()
            .default_output_device()
            .ok_or("Nessun dispositivo audio")?;
        let config = device
            .default_output_config()
            .map_err(|e| e.to_string())?;
        let rate = config.sample_rate().0;
        let channels = config.channels().max(1) as usize;

        let soundfont = Arc::new(
            SoundFont::new(&mut File::open(&sf_path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?,
        );
        let midi = Arc::new(
            MidiFile::new(&mut File::open(path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?,
        );
        let duration = midi.get_length().max(0.0);

        let settings = SynthesizerSettings::new(rate as i32);
        let synth = Synthesizer::new(&soundfont, &settings).map_err(|e| e.to_string())?;
        let mut sequencer = MidiFileSequencer::new(synth);
        sequencer.play(&midi, false);

        let sequencer = Arc::new(Mutex::new(sequencer));
        let playing = Arc::new(AtomicBool::new(true));
        let rendered = Arc::new(AtomicU64::new(0)); // per-channel frames synthesised

        let (seq_cb, play_cb, rend_cb) = (sequencer.clone(), playing.clone(), rendered.clone());
        let mut lbuf: Vec<f32> = Vec::new();
        let mut rbuf: Vec<f32> = Vec::new();
        let stream = device
            .build_output_stream(
                &config.config(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !play_cb.load(Ordering::Relaxed) {
                        data.iter_mut().for_each(|s| *s = 0.0);
                        return;
                    }
                    let frames = data.len() / channels;
                    lbuf.resize(frames, 0.0);
                    rbuf.resize(frames, 0.0);
                    seq_cb.lock().unwrap().render(&mut lbuf, &mut rbuf);
                    for (i, frame) in data.chunks_mut(channels).enumerate() {
                        if channels >= 2 {
                            frame[0] = lbuf[i];
                            frame[1] = rbuf[i];
                            frame[2..].iter_mut().for_each(|s| *s = 0.0);
                        } else {
                            frame[0] = 0.5 * (lbuf[i] + rbuf[i]);
                        }
                    }
                    rend_cb.fetch_add(frames as u64, Ordering::Relaxed);
                },
                |e| eprintln!("audio stream error: {e}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;

        let pos = || rendered.load(Ordering::Relaxed) as f64 / rate as f64;
        let mut ended = false;

        loop {
            match cmd_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(MediaCmd::Play) => playing.store(true, Ordering::Relaxed),
                Ok(MediaCmd::Pause) => playing.store(false, Ordering::Relaxed),
                Ok(MediaCmd::TogglePlay) => {
                    let p = !playing.load(Ordering::Relaxed);
                    playing.store(p, Ordering::Relaxed);
                }
                Ok(MediaCmd::Seek(t)) => {
                    let t = t.clamp(0.0, duration);
                    seek(&sequencer, &midi, rate, channels, t);
                    rendered.store((t * rate as f64) as u64, Ordering::Relaxed);
                    ended = false;
                    playing.store(true, Ordering::Relaxed);
                    let _ = msg_tx.send(MediaMsg::Position(t));
                    wake();
                }
                Ok(MediaCmd::Stop) => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
            }

            // Tick: report position and detect end of sequence.
            if !ended && sequencer.lock().unwrap().end_of_sequence() {
                ended = true;
                playing.store(false, Ordering::Relaxed);
                let _ = msg_tx.send(MediaMsg::Eof);
                wake();
            } else if playing.load(Ordering::Relaxed) {
                let _ = msg_tx.send(MediaMsg::Position(pos().min(duration)));
                wake();
            }
        }
    }

    /// rustysynth's sequencer has no seek, so restart and fast-forward by
    /// rendering (and discarding) up to the target. Render is far faster than
    /// real time, so this is quick for typical clips.
    fn seek(
        sequencer: &Arc<Mutex<MidiFileSequencer>>,
        midi: &Arc<MidiFile>,
        rate: u32,
        _channels: usize,
        target: f64,
    ) {
        let mut seq = sequencer.lock().unwrap();
        seq.play(midi, false);
        let mut remaining = (target * rate as f64) as usize;
        let chunk = 4096;
        let (mut l, mut r) = (vec![0.0f32; chunk], vec![0.0f32; chunk]);
        while remaining > 0 {
            let n = remaining.min(chunk);
            seq.render(&mut l[..n], &mut r[..n]);
            remaining -= n;
        }
    }
}
