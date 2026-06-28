# viewer

Viewer nativo velocissimo (Rust + egui) per **CSV/TSV**, **immagini** (PNG, JPEG, GIF, BMP, WebP, TIFF, ICO), **SVG** e **documenti Office**.

- Avvio istantaneo (~65 ms al primo frame, backend OpenGL/glow — misurato 3× più veloce di Vulkan/wgpu su questo carico).
- **Pipeline parallela** (default): il file viene decodificato su un thread mentre il thread principale inizializza la finestra → la UI è interattiva in ~90 ms **a prescindere dalla dimensione del file** (un CSV da 45 MB mostra la finestra a ~120 ms invece di ~900 ms). `VIEWER_PIPELINE=sync` forza la vecchia decodifica inline.
- **PDF renderizzato off-thread**: la finestra appare subito (~70 ms, come gli altri tipi) e la pagina si riempie appena pronta.
- **Singola istanza + residente**: aprire un altro file riusa la finestra (handoff via socket Unix ~5 ms); in Quick Look il processo resta vivo nascosto, così le riaperture sono istantanee (auto-uscita dopo 5 min di inattività).
- **Quick Look** (`--quicklook`): finestra overlay sempre in primo piano, si chiude con **Spazio/Esc** o alla perdita di focus. Pensata per essere legata a **Spazio** in Dolphin.
- GPU-accelerato, binario singolo.
- CSV/fogli con **rendering virtualizzato**: regge file enormi (mostra solo le righe visibili), colonne ridimensionabili, filtro live.
- Immagini/SVG con **zoom** (rotella/pinch, ancorato al cursore) e **pan** (trascina). SVG rasterizzato con resvg.
- **Fogli di calcolo** (`.xlsx .xlsm .xlsb .xls .ods`) via calamine → tabella con selettore di foglio.
- **Word / PowerPoint / OpenDocument** (`.docx .pptx .odt .odp`) → anteprima testo (estrazione contenuto).
- **Markdown** (`.md .markdown`) renderizzato (egui_commonmark).
- **PDF** (`.pdf`) con rendering pagine, zoom/pan e navigazione (◀/▶, frecce, PgUp/PgDn). Richiede `libpdfium.so` (vedi sotto).
- Formati binari legacy `.doc/.ppt` non supportati (converti in .docx/.pptx).

## Dipendenza per il PDF

Il rendering PDF usa **pdfium**. Metti `libpdfium.so` in uno di questi percorsi (cercati in ordine):
`$PDFIUM_LIB` · accanto al binario · `~/.local/lib/libpdfium.so` · `/usr/lib` · `/usr/local/lib`.

Prebuilt (no root):
```bash
curl -fsSL https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-x64.tgz \
  | tar -xz -C /tmp && mkdir -p ~/.local/lib && cp /tmp/lib/libpdfium.so ~/.local/lib/
```
Oppure da AUR: `paru -S pdfium-binaries`.

## Uso

```bash
cargo build --release
./target/release/viewer [FILE]
```

- Trascina un file nella finestra, oppure premi **📂 Apri** / **Ctrl+O**.
- **F** o **⛶** = schermo intero; **Esc** esce dal fullscreen (o chiude la finestra).
- Finestra frameless: trascina la **toolbar** per spostarla, doppio-clic per massimizzare.
- Immagine/PDF/SVG: rotella = zoom, trascina = pan, **Adatta** = reset; il contenuto si adatta automaticamente alla finestra.
- CSV: scrivi nel box **🔍 filtra** per filtrare le righe.

### Integrazione col file manager
Punta l'apertura del file a `target/release/viewer "%f"` (il path passato come primo argomento viene aperto al lancio).

## Struttura: libreria riutilizzabile

Il progetto è un workspace Cargo a due crate, così la capacità di **decodificare velocemente molti formati** è riusabile da altri programmi, indipendentemente dall'interfaccia grafica:

- **`crates/viewer-core`** — libreria **senza dipendenze grafiche** (niente egui/GPU). Espone:
  - `decode(path) -> Decoded`: legge e interpreta un file in un valore `Send` con dati grezzi (`Decoded::Image { rgba, w, h, … }`, `Csv`, `Sheets`, `Text`, `Markdown`, `Pdf(bytes)`, `Error`).
  - `decode_with_limits(path, SizeLimits)` / `SizeLimits`: stesso decode ma con **size-guard configurabile** per famiglia di formato (testo/immagini/altro). I file oltre il limite vengono rifiutati *prima* di leggerli in RAM. `SizeLimits::unlimited()` toglie il guard; `decode()` usa `SizeLimits::from_env()`, che parte dai default e li sovrascrive tutti con la variabile d'ambiente `VIEWER_MAX_MB` se impostata (così si alza il tetto senza ricompilare).
  - `spawn_decode(path, on_ready) -> Receiver<Decoded>`: esegue la decodifica su un thread worker e richiama `on_ready` quando il risultato è pronto. È il primitivo dietro l'«apertura istantanea»: avvii la decodifica, costruisci la finestra in parallelo, e raccogli il risultato quando arriva.
  - modulo `pdf` (feature `pdf`, attiva di default): `bind_pdfium()` e `pdf_worker(...)` servono il PDF off-thread, agnostico rispetto alla UI (segnala l'avanzamento con una callback `wake` qualsiasi). Il worker risponde a `PdfReq::Render(pagina)` con la pagina rasterizzata e a `PdfReq::Text(pagina)` con il **layer di testo**: `PdfMsg::Text { words, page_w_pt, page_h_pt }`, dove ogni `PdfWord` porta testo + box in **coordinate normalizzate** (`0..1`, origine in alto a sinistra, indipendenti dallo zoom) — la base per evidenziare/localizzare un campo (prezzo, EAN) e il click-to-locate. L'estrazione testo è molto più economica del rendering ed è on-demand per pagina. Senza la feature, `decode()` restituisce comunque i byte del PDF (`Decoded::Pdf`).
- **`crates/viewer`** — l'applicazione (egui/eframe): finestra, IPC a istanza singola, Quick Look, e i widget di rendering.

Esempio minimo (qualsiasi app Rust):
```rust
let rx = viewer_core::spawn_decode("data.csv".into(), || { /* sveglia la tua UI */ });
// ... inizializza la finestra mentre il file viene decodificato ...
match rx.recv().unwrap() {
    viewer_core::Decoded::Csv(c) => { /* usa c.headers / c.rows */ }
    other => { /* gestisci gli altri tipi */ }
}
```

Per usarla senza pdfium: `viewer-core = { path = "…", default-features = false }`.
