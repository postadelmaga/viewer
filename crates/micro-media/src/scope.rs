//! A lock-light audio tap for visualisation — the data behind the player's
//! oscilloscope.
//!
//! The playback engines (FFmpeg audio, MIDI synth) push the very samples they
//! hand to the device into one of these; the UI snapshots the most recent window
//! each repaint and paints it. It is deliberately tiny and lossy: a fixed ring of
//! mono samples, overwritten in place, no backpressure. A visualisation that
//! misses a few samples is fine — stalling the real-time audio callback is not,
//! so the lock is only ever held to copy a handful of floats.

use std::sync::{Arc, Mutex};

/// Number of mono samples retained. At 44.1 kHz this is ~23 ms — a short, lively
/// window that reads as an oscilloscope rather than a sluggish level meter.
pub const SCOPE_LEN: usize = 1024;

/// A shared, cloneable tap of recently-played audio. Cloning shares one ring, so
/// the audio callback and the UI talk through the same buffer.
#[derive(Clone)]
pub struct Scope(Arc<Mutex<Ring>>);

struct Ring {
    buf: [f32; SCOPE_LEN],
    /// Index the next sample is written to (the ring's oldest slot).
    head: usize,
}

impl Scope {
    pub fn new() -> Self {
        Scope(Arc::new(Mutex::new(Ring {
            buf: [0.0; SCOPE_LEN],
            head: 0,
        })))
    }

    /// Push a block of interleaved samples, mono-downmixing each frame. Called
    /// from the audio callback, so it copies floats and nothing else.
    pub fn push_interleaved(&self, data: &[f32], channels: usize) {
        if channels == 0 {
            return;
        }
        let mut r = self.0.lock().unwrap();
        for frame in data.chunks_exact(channels) {
            let m = frame.iter().copied().sum::<f32>() / channels as f32;
            let h = r.head;
            r.buf[h] = m;
            r.head = (h + 1) % SCOPE_LEN;
        }
    }

    /// Copy the retained window, oldest→newest, into `out` (cleared first).
    pub fn snapshot(&self, out: &mut Vec<f32>) {
        let r = self.0.lock().unwrap();
        out.clear();
        out.reserve(SCOPE_LEN);
        let h = r.head;
        for i in 0..SCOPE_LEN {
            out.push(r.buf[(h + i) % SCOPE_LEN]);
        }
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}
