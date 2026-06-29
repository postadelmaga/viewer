#!/usr/bin/env bash
# install.sh — build viewer (release) and register it with the Linux desktop:
# installs the binary to ~/.local/bin, writes the .desktop launcher + a KDE
# service menu (Quick Look on Space / Open with viewer), and registers MIME
# types for every format the program opens (including a custom one for farbfeld,
# which has no standard MIME type). Re-run after adding formats.
#
# Usage:  ./install.sh            # build + install + register
#         ./install.sh --no-build # skip cargo build (use the existing binary)
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
BIN="$HOME/.local/bin/viewer"
APPS="$HOME/.local/share/applications"
SVCMENU="$HOME/.local/share/kio/servicemenus"
MIMEPKG="$HOME/.local/share/mime/packages"

# Every MIME type the viewer handles (kept in sync with the decoder registry).
MIMES="text/plain;text/csv;text/tab-separated-values;text/markdown;application/json;application/xml;text/xml;application/toml;application/yaml;application/x-yaml;text/rust;text/x-python;text/x-csrc;text/x-c++src;text/x-chdr;text/x-c++hdr;text/x-go;text/javascript;application/javascript;text/css;text/html;application/xhtml+xml;application/x-shellscript;text/x-java;application/x-ruby;text/x-sql;text/x-lua;application/x-php;image/png;image/jpeg;image/gif;image/bmp;image/webp;image/tiff;image/vnd.microsoft.icon;image/x-icon;image/svg+xml;image/svg+xml-compressed;image/x-portable-pixmap;image/x-portable-graymap;image/x-portable-bitmap;image/x-portable-anymap;image/x-tga;image/qoi;image/x-dds;image/x-hdr;image/vnd.radiance;image/x-farbfeld;model/gltf-binary;model/gltf+json;model/obj;model/stl;application/pdf;application/vnd.openxmlformats-officedocument.spreadsheetml.sheet;application/vnd.ms-excel;application/vnd.oasis.opendocument.spreadsheet;application/vnd.openxmlformats-officedocument.wordprocessingml.document;application/vnd.openxmlformats-officedocument.presentationml.presentation;application/vnd.oasis.opendocument.text;application/vnd.oasis.opendocument.presentation;audio/mpeg;audio/flac;audio/ogg;audio/x-vorbis+ogg;audio/x-opus+ogg;audio/vnd.wave;audio/x-wav;audio/wav;audio/mp4;audio/aac;audio/x-aac;audio/x-ms-wma;audio/midi;audio/x-midi;video/mp4;video/x-matroska;video/quicktime;video/webm;video/x-msvideo;video/x-ms-wmv;video/x-flv;video/mpeg;video/mp2t;video/ogg;video/3gpp;video/x-m4v;"

if [[ "${1:-}" != "--no-build" ]]; then
  echo ">> build release"
  ( cd "$REPO" && cargo build --release )
fi

echo ">> install binary -> $BIN"
mkdir -p "$HOME/.local/bin"
install -m755 "$REPO/target/release/viewer" "$BIN"

echo ">> custom MIME (farbfeld)"
mkdir -p "$MIMEPKG"
cat > "$MIMEPKG/viewer.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="image/x-farbfeld">
    <comment>Farbfeld image</comment>
    <glob pattern="*.ff"/>
    <glob pattern="*.farbfeld"/>
    <magic priority="50"><match type="string" value="farbfeld" offset="0"/></magic>
  </mime-type>
</mime-info>
XML

echo ">> desktop launcher"
mkdir -p "$APPS"
cat > "$APPS/viewer-app.desktop" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=viewer
GenericName=Visualizzatore file
Comment=Visualizzatore veloce per tabelle, immagini, SVG, Office, PDF, 3D, audio/video, MIDI e testo/codice
Exec=$BIN %f
Icon=applications-graphics
Terminal=false
Categories=Graphics;Viewer;AudioVideo;
MimeType=$MIMES
EOF

echo ">> KDE service menu (Quick Look / Open with)"
mkdir -p "$SVCMENU"
cat > "$SVCMENU/viewer.desktop" <<EOF
[Desktop Entry]
Type=Service
ServiceTypes=KonqPopupMenu/Plugin
MimeType=$MIMES
Actions=QuickLook;OpenInViewer;
X-KDE-Priority=TopLevel

[Desktop Action QuickLook]
Name=Anteprima rapida (Quick Look)
Icon=quickview
Exec=$BIN --quicklook %f

[Desktop Action OpenInViewer]
Name=Apri con viewer
Icon=document-preview
Exec=$BIN %f
EOF

echo ">> refresh databases"
update-mime-database "$HOME/.local/share/mime" || true
update-desktop-database "$APPS" || true
kbuildsycoca6 2>/dev/null || kbuildsycoca5 2>/dev/null || true

echo ">> done. Restart Dolphin if a menu doesn't appear yet."
