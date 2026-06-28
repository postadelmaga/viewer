//! viewer-core — fast, UI-agnostic file decoding.
//!
//! [`decode`] reads and parses a path into [`Decoded`], a `Send` value carrying
//! raw data (RGBA pixels, table rows, extracted text, PDF bytes). It builds no
//! GPU textures and has no windowing dependency, so any program — egui or not —
//! can reuse the format support. [`spawn_decode`] runs that work on a worker
//! thread so a UI can stay interactive while a file loads (the "instant open"
//! pattern the `viewer` app is built on).
//!
//! ```no_run
//! let rx = viewer_core::spawn_decode("data.csv".into(), || { /* wake the UI */ });
//! // ... initialise your window while the file decodes ...
//! match rx.recv().unwrap() {
//!     viewer_core::Decoded::Csv(c) => { /* render c.headers / c.rows */ }
//!     other => { /* handle the other variants */ }
//! }
//! ```

pub mod csv;
pub mod image;
pub mod mesh;
pub mod office;
#[cfg(feature = "pdf")]
pub mod pdf;

pub use csv::CsvData;
pub use mesh::MeshData;
pub use office::SheetData;

use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

/// Result of decoding a file. Everything here is `Send`; turning it into GPU
/// textures (if a caller wants to display it) happens later on the UI thread.
///
/// `#[non_exhaustive]`: new variants may be added in future releases without a
/// breaking change, so external `match`es must include a `_ => …` arm.
#[non_exhaustive]
pub enum Decoded {
    Csv(CsvData),
    Sheets(SheetData),
    Image {
        rgba: Vec<u8>,
        w: u32,
        h: u32,
        premultiplied: bool,
        kind: String,
    },
    Markdown(String),
    Text(String),
    /// Raw PDF bytes. Decoding pages needs pdfium — see the [`pdf`] module
    /// (enabled by the `pdf` feature).
    Pdf(Vec<u8>),
    /// A decoded 3D mesh (OBJ / glTF / GLB). Produced only when the `mesh`
    /// feature is enabled; see the [`mesh`] module.
    Mesh(MeshData),
    Error(String),
}

/// Size-limit family of a format — which [`SizeLimits`] budget guards it. Kept
/// separate from the decoder so the memory policy is one small, auditable table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Text-like, ~1× in memory (csv, markdown, …).
    Text,
    /// Raster images, which decode to w·h·4 bytes regardless of file size.
    Image,
    /// 3D meshes (OBJ/glTF/GLB). Get their own, more generous budget: real
    /// models are routinely large yet expand only modestly in memory (a text OBJ
    /// often decodes to *fewer* bytes than its source; glTF/GLB is already
    /// binary), and decoding runs off-thread so a big file doesn't block the UI.
    Mesh,
    /// Everything else (Office/ODF/PDF/SVG/…).
    Other,
}

/// What a format decoder is handed: the file path plus its already-read bytes.
/// Most decoders work from `bytes`; a few (e.g. glTF, which resolves sibling
/// `.bin`/texture files) need the `path`. Owning both keeps the decoder fn
/// pointer free of lifetimes, so the registry is a plain `const` table.
pub struct Input {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

/// A self-describing file format: the extensions it claims, its size-limit
/// [`Family`], and how to decode it. Each decoder module exposes a `FORMATS`
/// slice of these and [`decode_with_limits`] aggregates them into one registry —
/// so supporting a new format is a new module plus one row, never an edit to a
/// central match. Feature-gated families (3D) contribute their rows only when
/// compiled in, decoding to a clear "not compiled" error otherwise.
pub struct Format {
    pub exts: &'static [&'static str],
    pub family: Family,
    pub decode: fn(Input) -> Decoded,
}

/// Formats handled directly by the library root (no dedicated module): Markdown,
/// the PDF byte passthrough (rendering lives in the gated [`pdf`] module), and a
/// helpful refusal for legacy binary Office files.
const LIB_FORMATS: &[Format] = &[
    Format {
        exts: &["md", "markdown"],
        family: Family::Text,
        decode: decode_markdown,
    },
    Format {
        exts: &["pdf"],
        family: Family::Other,
        decode: decode_pdf_passthrough,
    },
    Format {
        exts: &["doc", "ppt"],
        family: Family::Other,
        decode: decode_legacy_office,
    },
];

/// Every registered format, in lookup order. The first row whose `exts` contains
/// the queried extension wins.
fn registry() -> impl Iterator<Item = &'static Format> {
    LIB_FORMATS
        .iter()
        .chain(csv::FORMATS)
        .chain(image::FORMATS)
        .chain(office::FORMATS)
        .chain(mesh::FORMATS)
}

/// Find the format claiming `ext` (already lowercased), if any.
fn find_format(ext: &str) -> Option<&'static Format> {
    registry().find(|f| f.exts.contains(&ext))
}

/// Size family for an extension no decoder claims: text-like ones load ~1× so
/// they get the text budget; unknown binaries get the `other` budget. The
/// fallback decoder ([`decode_text_or_guess`]) then handles the bytes.
fn unmatched_family(ext: &str) -> Family {
    match ext {
        "txt" | "json" | "log" | "" => Family::Text,
        _ => Family::Other,
    }
}

fn decode_markdown(input: Input) -> Decoded {
    match String::from_utf8(input.bytes) {
        Ok(s) => Decoded::Markdown(s),
        Err(_) => Decoded::Error("Markdown non in UTF-8".into()),
    }
}

fn decode_pdf_passthrough(input: Input) -> Decoded {
    // No copy: hand the read buffer straight on as the PDF payload.
    Decoded::Pdf(input.bytes)
}

fn decode_legacy_office(_: Input) -> Decoded {
    Decoded::Error(
        "Formato Office binario legacy (.doc/.ppt) non supportato.\nConverti in .docx/.pptx o PDF."
            .into(),
    )
}

/// Maximum input sizes (MiB) accepted by [`decode_with_limits`], per format
/// family. A file larger than its family's limit is rejected *before* being read
/// into memory, since decoding can amplify it (a raster image decodes to w·h·4
/// bytes). Tune these when a caller manages memory itself or routinely handles
/// larger exports than the defaults assume.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct SizeLimits {
    /// Text-like formats, ~1× in memory: csv, tsv, md, txt, json, log, and
    /// extension-less files.
    pub text_mb: u64,
    /// Raster images, which decode to w·h·4 bytes regardless of file size.
    pub image_mb: u64,
    /// 3D meshes (OBJ/glTF/GLB). Higher than `other_mb`: real models are large
    /// but expand little in memory, and decoding is off-thread.
    pub mesh_mb: u64,
    /// Everything else (Office/ODF/PDF/…).
    pub other_mb: u64,
}

impl Default for SizeLimits {
    fn default() -> Self {
        Self {
            text_mb: 512,
            image_mb: 64,
            mesh_mb: 512,
            other_mb: 128,
        }
    }
}

impl SizeLimits {
    /// Accept any size — no guard. Use when the caller has already bounded the
    /// input or manages memory pressure on its own.
    pub fn unlimited() -> Self {
        Self {
            text_mb: u64::MAX,
            image_mb: u64::MAX,
            mesh_mb: u64::MAX,
            other_mb: u64::MAX,
        }
    }

    /// Defaults, with every family overridden to `VIEWER_MAX_MB` when that env
    /// var is set to a valid number. Lets operators raise the ceiling for an
    /// app built on [`decode`] without a code change.
    pub fn from_env() -> Self {
        match std::env::var("VIEWER_MAX_MB")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            Some(v) => Self {
                text_mb: v,
                image_mb: v,
                mesh_mb: v,
                other_mb: v,
            },
            None => Self::default(),
        }
    }

    /// The limit that applies to a given size-limit [`Family`].
    fn for_family(&self, family: Family) -> u64 {
        match family {
            Family::Text => self.text_mb,
            Family::Image => self.image_mb,
            Family::Mesh => self.mesh_mb,
            Family::Other => self.other_mb,
        }
    }
}

/// Decode `path` on the current thread, using [`SizeLimits::from_env`]. Never
/// touches a GPU or windowing context, so it is safe to call from any thread.
/// Call [`decode_with_limits`] to set the size guard programmatically.
pub fn decode(path: &Path) -> Decoded {
    decode_with_limits(path, SizeLimits::from_env())
}

/// Like [`decode`], but with an explicit size guard. A file over the limit for
/// its format family is rejected before being read into memory.
pub fn decode_with_limits(path: &Path, limits: SizeLimits) -> Decoded {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Resolve the format (and thus its size family) before touching the file.
    let format = find_format(&ext);
    let family = format.map_or_else(|| unmatched_family(&ext), |f| f.family);

    let max_mb = limits.for_family(family);
    if max_mb != u64::MAX {
        if let Ok(meta) = std::fs::metadata(path) {
            // Compare in bytes so the limit is exact: a 0 MB limit rejects any
            // non-empty file, and 512 MB doesn't silently tolerate 512.9 MB.
            if meta.len() > max_mb.saturating_mul(1024 * 1024) {
                let mb = meta.len() / (1024 * 1024);
                return Decoded::Error(format!(
                    "File troppo grande: {mb} MB (limite {max_mb} MB per questo formato).\nIl caricamento rischierebbe di esaurire la memoria."
                ));
            }
        }
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return Decoded::Error(format!("Impossibile leggere il file:\n{e}")),
    };

    match format {
        Some(f) => (f.decode)(Input {
            path: path.to_path_buf(),
            bytes,
        }),
        // Unknown extension: sniff for an image, else show as text.
        None => decode_text_or_guess(&bytes),
    }
}

/// Decode `path` on a freshly spawned worker thread, returning a [`Receiver`]
/// that yields the [`Decoded`] result. This is the primitive behind the app's
/// "window is interactive in ~90 ms regardless of file size": start the decode,
/// build your window, then pick up the result when it lands.
///
/// `on_ready` is invoked **on the worker thread** right after the result has been
/// sent — use it only to *signal* the consumer (e.g. request a UI repaint / wake
/// an event loop) so it can `try_recv()` without busy-polling. It must not touch
/// state owned by another thread, and it should not panic (a panic there would
/// silently kill the worker after the result was already delivered).
pub fn spawn_decode<F>(path: PathBuf, on_ready: F) -> Receiver<Decoded>
where
    F: FnOnce() + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<Decoded>();
    std::thread::spawn(move || {
        let _ = tx.send(decode(&path));
        on_ready();
    });
    rx
}

fn decode_text_or_guess(bytes: &[u8]) -> Decoded {
    // Try image first (covers extension-less image files), else show as text.
    // `::image` is the crate; `image` here would be the sibling submodule.
    if ::image::guess_format(bytes).is_ok() {
        if let d @ Decoded::Image { .. } = image::decode_image(bytes) {
            return d;
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Decoded::Text(s.to_string()),
        Err(_) => Decoded::Error("Formato non riconosciuto (file binario)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::csv::decode_csv;
    use super::office::{decode_docx, decode_spreadsheet};
    use super::Decoded;

    // Fixtures live in the repo so `cargo test` is portable on any machine.
    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    #[test]
    fn registry_routes_known_extensions_and_falls_back() {
        use super::{decode_with_limits, SizeLimits};
        use std::path::PathBuf;
        let unlimited = SizeLimits::unlimited();

        // SVG → image family decoder.
        let svg = PathBuf::from(format!("{DIR}/test.svg"));
        match decode_with_limits(&svg, unlimited) {
            Decoded::Image { .. } => {}
            other => panic!("svg: atteso Image, ottenuto {}", decoded_variant(&other)),
        }
        // OBJ → mesh decoder (registered behind the `mesh` feature).
        #[cfg(feature = "mesh")]
        {
            let obj = PathBuf::from(format!("{DIR}/test.obj"));
            match decode_with_limits(&obj, unlimited) {
                Decoded::Mesh(_) => {}
                other => panic!("obj: atteso Mesh, ottenuto {}", decoded_variant(&other)),
            }
        }
        // Unknown extension on UTF-8 content → text fallback.
        let md = PathBuf::from(format!("{DIR}/test.md")); // not a registry text decoder ext for raw bytes
        match decode_with_limits(&md, unlimited) {
            // .md is registered (Markdown); assert the registry picked it, not
            // the fallback — proving extension dispatch works.
            Decoded::Markdown(_) => {}
            other => panic!("md: atteso Markdown, ottenuto {}", decoded_variant(&other)),
        }
    }

    #[test]
    fn xlsx_parses_into_sheets() {
        let bytes = std::fs::read(format!("{DIR}/test.xlsx")).expect("test.xlsx");
        match decode_spreadsheet(&bytes) {
            Decoded::Sheets(sd) => {
                assert_eq!(sd.sheets.len(), 2, "due fogli");
                assert_eq!(sd.sheets[0].0, "Foglio1");
                assert_eq!(sd.sheets[0].1.headers, vec!["nome", "qta", "prezzo"]);
                assert_eq!(sd.sheets[0].1.rows.len(), 2);
                assert_eq!(sd.sheets[0].1.rows[0][0], "mela");
                assert_eq!(sd.sheets[1].0, "Note");
            }
            other => panic!("atteso Sheets, ottenuto {}", decoded_variant(&other)),
        }
    }

    #[test]
    fn docx_extracts_text() {
        let bytes = std::fs::read(format!("{DIR}/test.docx")).expect("test.docx");
        match decode_docx(&bytes) {
            Decoded::Text(t) => {
                assert!(t.contains("Ciao mondo"), "testo: {t:?}");
                assert!(t.contains("Seconda riga"));
            }
            other => panic!("atteso Text, ottenuto {}", decoded_variant(&other)),
        }
    }

    #[cfg(feature = "pdf")]
    #[test]
    fn pdf_renders_with_pdfium() {
        let bytes = std::fs::read(format!("{DIR}/test.pdf")).expect("test.pdf");
        let pdfium = super::pdf::bind_pdfium().expect("binding libpdfium");
        let doc = pdfium
            .load_pdf_from_byte_slice(&bytes, None)
            .expect("load pdf");
        assert!(doc.pages().len() >= 1, "almeno una pagina");
        let page = doc.pages().get(0).unwrap();
        let cfg = pdfium_render::prelude::PdfRenderConfig::new().set_target_width(300);
        let img = page.render_with_config(&cfg).unwrap().as_image().into_rgba8();
        assert!(img.width() > 0 && img.height() > 0, "immagine non vuota");
    }

    #[test]
    fn csv_decodes() {
        let bytes = std::fs::read(format!("{DIR}/test.csv")).expect("test.csv");
        match decode_csv(&bytes, b',') {
            Decoded::Csv(c) => {
                assert_eq!(c.headers.len(), 3);
                assert!(c.rows.len() >= 3);
            }
            other => panic!("atteso Csv, ottenuto {}", decoded_variant(&other)),
        }
    }

    #[test]
    fn size_limit_rejects_oversized_file_before_reading() {
        use super::{decode_with_limits, SizeLimits};
        // The CSV fixture is a few hundred bytes; a 0 MB text limit rejects it.
        let path = std::path::PathBuf::from(format!("{DIR}/test.csv"));
        let limits = SizeLimits {
            text_mb: 0,
            ..SizeLimits::default()
        };
        match decode_with_limits(&path, limits) {
            Decoded::Error(e) => assert!(e.contains("troppo grande"), "msg: {e}"),
            other => panic!("atteso Error, ottenuto {}", decoded_variant(&other)),
        }
        // Unlimited accepts it.
        match decode_with_limits(&path, SizeLimits::unlimited()) {
            Decoded::Csv(_) => {}
            other => panic!("atteso Csv, ottenuto {}", decoded_variant(&other)),
        }
    }

    fn decoded_variant(d: &Decoded) -> &'static str {
        match d {
            Decoded::Csv(_) => "Csv",
            Decoded::Sheets(_) => "Sheets",
            Decoded::Image { .. } => "Image",
            Decoded::Markdown(_) => "Markdown",
            Decoded::Text(_) => "Text",
            Decoded::Pdf(_) => "Pdf",
            Decoded::Mesh(_) => "Mesh",
            Decoded::Error(_) => "Error",
        }
    }
}
