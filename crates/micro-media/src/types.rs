//! The two canonical media payloads. Both wrap their buffer in an [`Arc`] so moving one
//! across a channel (or cloning a handle for a second sink) is a pointer bump, not a copy —
//! the whole point of the data plane.

use std::sync::Arc;

/// How the bytes of a [`Frame`] are laid out. Kept minimal on purpose; an app adds variants
/// in its own renderer if it needs more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8 bits per channel, red-green-blue-alpha, 4 bytes per pixel.
    Rgba8,
    /// 8 bits per channel, blue-green-red-alpha (common GPU swapchain order).
    Bgra8,
}

impl PixelFormat {
    /// Bytes per pixel for this format.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
        }
    }
}

/// A single decoded video frame: a shared pixel buffer plus its geometry. Cheap to move and
/// to clone (the `Arc` is shared, the pixels are not copied).
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Tightly packed `width * height * bytes_per_pixel` bytes.
    pub pixels: Arc<[u8]>,
}

impl Frame {
    /// Build a frame, checking the buffer length matches the geometry.
    pub fn new(
        width: u32,
        height: u32,
        format: PixelFormat,
        pixels: impl Into<Arc<[u8]>>,
    ) -> Result<Self, String> {
        let pixels = pixels.into();
        let expected = width as usize * height as usize * format.bytes_per_pixel();
        if pixels.len() != expected {
            return Err(format!(
                "frame buffer is {} bytes, expected {expected} for {width}x{height} {format:?}",
                pixels.len()
            ));
        }
        Ok(Self {
            width,
            height,
            format,
            pixels,
        })
    }
}

/// A block of rendered audio: interleaved `f32` samples plus the format needed to play them.
/// Shared via `Arc` so a block can fan out to, say, a player and a meter without a copy.
#[derive(Clone, Debug)]
pub struct AudioBlock {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved samples: `frames * channels` values, each in `-1.0..=1.0`.
    pub samples: Arc<[f32]>,
}

impl AudioBlock {
    pub fn new(sample_rate: u32, channels: u16, samples: impl Into<Arc<[f32]>>) -> Self {
        Self {
            sample_rate,
            channels,
            samples: samples.into(),
        }
    }

    /// Number of sample frames (samples per channel) in this block.
    pub fn frames(&self) -> usize {
        let ch = self.channels.max(1) as usize;
        self.samples.len() / ch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rejects_a_mismatched_buffer() {
        let ok = Frame::new(2, 2, PixelFormat::Rgba8, vec![0u8; 16]);
        assert!(ok.is_ok());
        let bad = Frame::new(2, 2, PixelFormat::Rgba8, vec![0u8; 15]);
        assert!(bad.is_err());
    }

    #[test]
    fn audio_block_counts_frames() {
        let b = AudioBlock::new(48_000, 2, vec![0.0f32; 256]);
        assert_eq!(b.frames(), 128);
    }
}
