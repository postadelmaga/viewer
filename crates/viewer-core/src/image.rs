//! Raster image and SVG decoding into raw RGBA (no GPU texture yet).

use super::{Decoded, Family, Format, Input};
use std::io::Cursor;

/// Formats this module handles (see [`crate::Format`]). SVG is `Other` family —
/// it's vector source, not a w·h·4 raster — while rasters use the image budget.
pub(crate) const FORMATS: &[Format] = &[
    Format {
        exts: &[
            "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico", "tga", "dds", "hdr",
            "pnm", "pbm", "pgm", "ppm", "pam", "qoi", "ff", "farbfeld",
        ],
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
    // Pass the extension so formats with no magic signature (notably TGA) still
    // decode when content-sniffing can't identify them.
    let ext = input
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    decode_image_ext(&input.bytes, &ext)
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

/// Decode raster bytes whose format is sniffed from content. Use when the
/// extension is unknown (e.g. the extension-less fallback path).
pub fn decode_image(bytes: &[u8]) -> Decoded {
    decode_image_ext(bytes, "")
}

/// Like [`decode_image`], but with the file extension as a fallback format hint
/// for formats that carry no magic signature (TGA above all). Content sniffing
/// still wins when it succeeds; `ext` only fills the gap when it doesn't.
pub fn decode_image_ext(bytes: &[u8], ext: &str) -> Decoded {
    // A reader with the format resolved by content, then by extension. Built
    // twice (dimension probe, then decode) since `ImageReader` isn't reusable.
    let reader = || -> Option<image::ImageReader<Cursor<&[u8]>>> {
        let mut r = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        if r.format().is_none() {
            r.set_format(image::ImageFormat::from_extension(ext)?);
        }
        Some(r)
    };

    // Probe dimensions from the header (cheap, no pixel decode) and refuse
    // anything past the decode ceiling with a precise message.
    let dims = reader().and_then(|r| r.into_dimensions().ok());
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
    let mut reader = match reader() {
        Some(r) => r,
        None => return Decoded::Error("Immagine non leggibile".into()),
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

pub fn decode_svg(bytes: &[u8]) -> Decoded {
    use resvg::tiny_skia;
    use resvg::usvg;

    // Text shaping is compiled out (see Cargo.toml): we render shapes/paths only,
    // so no font database is needed. `<text>` elements are skipped.
    let opt = usvg::Options::default();

    let tree = match usvg::Tree::from_data(bytes, &opt) {
        Ok(t) => t,
        Err(e) => return Decoded::Error(format!("SVG non valido:\n{e}")),
    };

    let size = tree.size();
    // Rasterise to a fixed *pixel-area* budget, not a fixed longest side: render
    // cost is ~linear in area, and the old "scale the long side to 2048" rule made
    // a 256² icon a 2048²=16 MP (64 MB) pixmap that takes ~0.9 s to fill (vs ~0.12 s
    // at 1024²), while leaving genuinely large SVGs at full native size. Capping by
    // area upscales small SVGs for crispness yet keeps the worst case bounded; the
    // per-side clamp keeps the result a valid GPU texture.
    const MAX_AREA: f32 = 1_500_000.0; // ~1.5 MP, e.g. ~1224²
    let w = size.width().max(1.0);
    let h = size.height().max(1.0);
    let by_area = (MAX_AREA / (w * h)).sqrt();
    let by_side = GPU_MAX_SIDE as f32 / w.max(h);
    let scale = by_area.min(by_side).clamp(0.1, 8.0);
    let pw = (w * scale).ceil().max(1.0) as u32;
    let ph = (h * scale).ceil().max(1.0) as u32;

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
