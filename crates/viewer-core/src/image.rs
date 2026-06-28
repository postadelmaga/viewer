//! Raster image and SVG decoding into raw RGBA (no GPU texture yet).

use super::{Decoded, Family, Format, Input};
use std::io::Cursor;
use std::sync::OnceLock;

/// Formats this module handles (see [`crate::Format`]). SVG is `Other` family —
/// it's vector source, not a w·h·4 raster — while rasters use the image budget.
pub(crate) const FORMATS: &[Format] = &[
    Format {
        exts: &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico"],
        family: Family::Image,
        decode: raster_entry,
    },
    Format {
        exts: &["svg", "svgz"],
        family: Family::Other,
        decode: svg_entry,
    },
];

fn raster_entry(input: Input) -> Decoded {
    decode_image(&input.bytes)
}

fn svg_entry(input: Input) -> Decoded {
    decode_svg(&input.bytes)
}

/// Max texture side most GL drivers accept, and a sane upper bound on area so a
/// huge image doesn't keep tens of MB of RGBA around just to be shown shrunk.
const GPU_MAX_SIDE: u32 = 8192;
const MAX_PIXELS: u64 = 60_000_000; // 60 MP target after downscale
/// Hard ceiling on what we're willing to fully decode into RAM (~100 MP ≈ 400 MB
/// RGBA peak). Beyond this we refuse with a clear message instead of OOMing.
const MAX_DECODE_PIXELS: u64 = 100_000_000;

pub fn decode_image(bytes: &[u8]) -> Decoded {
    // Probe dimensions from the header (cheap, no pixel decode) and refuse
    // anything past the decode ceiling with a precise message.
    let dims = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok());
    if let Some((w, h)) = dims {
        if w as u64 * h as u64 > MAX_DECODE_PIXELS {
            return Decoded::Error(format!(
                "Immagine troppo grande: {w}×{h} ({} MP).\nLimite di decodifica: {} MP.",
                (w as u64 * h as u64) / 1_000_000,
                MAX_DECODE_PIXELS / 1_000_000
            ));
        }
    }

    // Decode, sizing the allocation budget to the actual image (the default
    // limit would reject large-but-acceptable images before we can downscale).
    // ~8 bytes/px covers the RGBA buffer plus typical decoder scratch; bounded
    // because the dimension probe above already rejects anything > the ceiling.
    let mut reader = match image::ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(r) => r,
        Err(e) => return Decoded::Error(format!("Immagine non leggibile:\n{e}")),
    };
    let px = dims.map(|(w, h)| w as u64 * h as u64).unwrap_or(MAX_DECODE_PIXELS);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(px.saturating_mul(8).saturating_add(64 << 20));
    reader.limits(limits);
    let mut img = match reader.decode() {
        Ok(i) => i,
        Err(e) => return Decoded::Error(format!("Immagine non supportata:\n{e}")),
    };
    let (ow, oh) = (img.width(), img.height());

    // Downscale if it exceeds the GPU side limit or the area budget, keeping the
    // aspect ratio (uniform scale = min of the side- and area-driven factors).
    let side_scale = (GPU_MAX_SIDE as f64 / ow.max(1) as f64)
        .min(GPU_MAX_SIDE as f64 / oh.max(1) as f64)
        .min(1.0);
    let area_scale = (MAX_PIXELS as f64 / (ow as u64 * oh as u64).max(1) as f64)
        .sqrt()
        .min(1.0);
    let scale = side_scale.min(area_scale);

    let mut kind = format!("{ow}×{oh}");
    if scale < 1.0 {
        let nw = ((ow as f64 * scale) as u32).max(1);
        let nh = ((oh as f64 * scale) as u32).max(1);
        img = img.resize_exact(nw, nh, image::imageops::FilterType::Triangle);
        kind = format!("{ow}×{oh} (ridotta a {nw}×{nh})");
    }

    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    Decoded::Image {
        rgba: rgba.into_raw(),
        w,
        h,
        premultiplied: false,
        kind,
    }
}

/// System fonts are expensive to enumerate (~1s cold), so load them once and
/// only when an SVG actually needs them.
fn svg_fontdb() -> &'static resvg::usvg::fontdb::Database {
    static DB: OnceLock<resvg::usvg::fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

pub fn decode_svg(bytes: &[u8]) -> Decoded {
    use resvg::tiny_skia;
    use resvg::usvg;

    // Only pay for system fonts if the SVG has text (or is gzipped and unscannable).
    let needs_fonts = bytes.starts_with(&[0x1f, 0x8b])
        || contains_sub(bytes, b"<text")
        || contains_sub(bytes, b"<tspan");

    let mut opt = usvg::Options::default();
    if needs_fonts {
        *opt.fontdb_mut() = svg_fontdb().clone();
    }

    let tree = match usvg::Tree::from_data(bytes, &opt) {
        Ok(t) => t,
        Err(e) => return Decoded::Error(format!("SVG non valido:\n{e}")),
    };

    let size = tree.size();
    // Render at a resolution that stays crisp under moderate zoom.
    let max_dim = size.width().max(size.height()).max(1.0);
    let scale = (2048.0 / max_dim).clamp(1.0, 8.0);
    let pw = (size.width() * scale).ceil().max(1.0) as u32;
    let ph = (size.height() * scale).ceil().max(1.0) as u32;

    let mut pixmap = match tiny_skia::Pixmap::new(pw, ph) {
        Some(p) => p,
        None => return Decoded::Error("Dimensione SVG non valida".into()),
    };
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Decoded::Image {
        rgba: pixmap.take(),
        w: pw,
        h: ph,
        premultiplied: true,
        kind: format!("SVG {}×{}", size.width() as i32, size.height() as i32),
    }
}

/// Substring search over raw bytes (std has none for slices).
fn contains_sub(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return needle.is_empty();
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::{decode_image, GPU_MAX_SIDE};
    use crate::Decoded;

    #[test]
    fn oversized_image_is_downscaled() {
        // A 9000×8 PNG exceeds the GPU side limit on width only.
        let big = ::image::RgbaImage::new(9000, 8);
        let mut buf = std::io::Cursor::new(Vec::new());
        ::image::DynamicImage::ImageRgba8(big)
            .write_to(&mut buf, ::image::ImageFormat::Png)
            .expect("encode png");

        match decode_image(&buf.into_inner()) {
            Decoded::Image { w, h, kind, .. } => {
                assert!(w <= GPU_MAX_SIDE, "larghezza {w} oltre il limite GPU");
                assert!((1..=8).contains(&h), "altezza scalata in proporzione: {h}");
                assert!(kind.contains("ridotta"), "kind segnala il downscale: {kind}");
            }
            _ => panic!("atteso Decoded::Image"),
        }
    }
}
