//! Single-instance support over a Unix domain socket, plus minimal diagnostics.

use eframe::egui;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

/// Minimal diagnostics to stderr (a full logging framework would be overkill).
pub(crate) fn warn(msg: &str) {
    eprintln!("viewer: {msg}");
}

pub(crate) fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("viewer.sock")
}

/// If a viewer is already running, hand the file off to it. Returns `true` when
/// the file was delivered (the caller should then exit). A failed connect is the
/// normal "no server yet" case and is not reported.
pub(crate) fn try_handoff(sock: &Path, path: &Path) -> bool {
    if let Ok(mut stream) = UnixStream::connect(sock) {
        match writeln!(stream, "{}", path.display()) {
            Ok(()) => return true,
            Err(e) => warn(&format!("handoff all'istanza esistente fallito: {e}")),
        }
    }
    false
}

/// Become the single-instance server: clear any stale socket and bind. Returns
/// `None` (multi-instance fallback) if binding fails, after logging why.
pub(crate) fn bind_listener(sock: &Path) -> Option<UnixListener> {
    if let Err(e) = std::fs::remove_file(sock) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn(&format!("socket stale non rimovibile {}: {e}", sock.display()));
        }
    }
    match UnixListener::bind(sock) {
        Ok(l) => Some(l),
        Err(e) => {
            warn(&format!(
                "istanza singola disattivata ({}): bind fallito: {e}",
                sock.display()
            ));
            None
        }
    }
}

/// Spawn the accept loop: each later invocation sends a file path, which we
/// forward to `tx` and wake the UI to pick up.
pub(crate) fn serve(listener: UnixListener, tx: Sender<PathBuf>, ctx: egui::Context) {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_ok() {
                let p = PathBuf::from(line.trim());
                if !p.as_os_str().is_empty() {
                    let _ = tx.send(p);
                    ctx.request_repaint();
                }
            }
        }
    });
}
