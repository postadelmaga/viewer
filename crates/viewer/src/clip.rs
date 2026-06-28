//! Clipboard copy, picking the representation that's most useful per file type.
//!
//! Raster images are placed as image data (so they paste into image editors and
//! chats); every other type is placed as a `text/uri-list` (so it pastes into a
//! file manager like Dolphin). On Wayland this goes through `wl-copy`; callers
//! fall back to copying the path as plain text when that isn't available.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Copy `path` to the clipboard. Returns a short status string for the UI, or an
/// error (e.g. `wl-copy` missing) so the caller can fall back to a text copy.
pub(crate) fn copy_file(path: &Path) -> Result<String, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let image_mime = match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "bmp" => Some("image/bmp"),
        "webp" => Some("image/webp"),
        "tif" | "tiff" => Some("image/tiff"),
        "ico" => Some("image/x-icon"),
        _ => None,
    };

    if let Some(mime) = image_mime {
        let bytes = std::fs::read(path).map_err(|e| format!("lettura immagine: {e}"))?;
        wl_copy(mime, &bytes)?;
        Ok("Immagine copiata".into())
    } else {
        // text/uri-list entries are file:// URIs separated by CRLF.
        let uri = format!("file://{}\r\n", uri_encode(&path.to_string_lossy()));
        wl_copy("text/uri-list", uri.as_bytes())?;
        Ok("File copiato".into())
    }
}

/// Feed `data` to `wl-copy --type <mime>` over stdin. `wl-copy` reads stdin fully
/// then forks a background process to serve the selection, so the foreground
/// child exits immediately and we can reap it without blocking.
fn wl_copy(mime: &str, data: &[u8]) -> Result<(), String> {
    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg(mime)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("wl-copy non disponibile: {e}"))?;
    {
        let mut stdin = child.stdin.take().ok_or("stdin non disponibile")?;
        stdin
            .write_all(data)
            .map_err(|e| format!("scrittura clipboard: {e}"))?;
        // stdin dropped here → EOF, so wl-copy can finish reading and daemonize.
    }
    let _ = child.wait();
    Ok(())
}

/// Percent-encode a path for a `file://` URI: keep the unreserved set and `/`,
/// escape everything else (spaces, accents, `#`, `?`, …) as `%XX`.
fn uri_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
