# viewer

Viewer nativo velocissimo (Rust + egui) per **CSV/TSV**, **immagini** (PNG, JPEG, GIF, BMP, WebP, TIFF, ICO), **SVG**, **documenti Office**, **PDF** e **modelli 3D** (OBJ, glTF/GLB).

- Avvio istantaneo (~65 ms al primo frame, backend OpenGL/glow — misurato 3× più veloce di Vulkan/wgpu su questo carico).
- **Pipeline parallela** (default): il file viene decodificato su un thread mentre il thread principale inizializza la finestra → la UI è interattiva in ~90 ms **a prescindere dalla dimensione del file** (un CSV da 45 MB mostra la finestra a ~120 ms invece di ~900 ms). `VIEWER_PIPELINE=sync` forza la vecchia decodifica inline.
- **PDF renderizzato off-thread**: la finestra appare subito (~70 ms, come gli altri tipi) e la pagina si riempie appena pronta.
- **Singola istanza + residente**: aprire un altro file riusa la finestra (handoff via socket Unix ~5 ms); in Quick Look il processo resta vivo nascosto, così le riaperture sono istantanee (auto-uscita dopo 5 min di inattività).
- **Quick Look** (`--quicklook`): finestra overlay sempre in primo piano, si chiude con **Spazio/Esc** o alla perdita di focus. Pensata per essere legata a **Spazio** in Dolphin.
- GPU-accelerato, binario singolo.
- **Interfaccia** dark curata (accento, angoli arrotondati) con **finestra trasparente** e **toolbar frosted**: su KWin, con l'effetto *Blur* attivo, diventa vetro smerigliato vero — costo a carico del compositore, ~zero per l'app.
- CSV/fogli con **rendering virtualizzato**: regge file enormi (mostra solo le righe visibili), colonne ridimensionabili, filtro live.
- Immagini/SVG con **zoom** (rotella/pinch, ancorato al cursore) e **pan** (trascina). SVG rasterizzato con resvg.
- **Fogli di calcolo** (`.xlsx .xlsm .xlsb .xls .ods`) via calamine → tabella con selettore di foglio.
- **Word / PowerPoint / OpenDocument** (`.docx .pptx .odt .odp`) → anteprima testo (estrazione contenuto).
- **Markdown** (`.md .markdown`) renderizzato (egui_commonmark).
- **PDF** (`.pdf`) con rendering pagine, zoom/pan e navigazione pagine (◀/▶, PgUp/PgDn). Richiede `libpdfium.so` (vedi sotto).
- **Modelli 3D** (`.obj .gltf .glb`) renderizzati su GPU (OpenGL/glow) con **camera orbitale**: trascina per ruotare, rotella per lo zoom, **Adatta** per inquadrare. Materiali/texture ignorati (solo geometria, illuminazione Lambert a faro). OBJ via **parser multi-thread interno** (~3-4× più veloce su file grandi: il parsing testuale è quasi tutto il costo di un OBJ); glTF 2.0 via `gltf` (scena di default appiattita in world-space). Decodifica **off-thread** con **spinner** di caricamento, così i file grandi non bloccano la finestra. Modulo opzionale: feature `mesh` di `viewer-core` (vedi sotto).
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
- **← / →** = file precedente/successivo nella **stessa cartella** (scorre solo i formati supportati, ordine alfabetico, con wrap agli estremi).
- **Ctrl+C** = copia negli appunti: le **immagini** come immagine (incolli in editor/chat), gli **altri file** come `text/uri-list` (incolli in Dolphin); fallback al percorso come testo. Usa `wl-copy` (Wayland).
- **F** o **⛶** = schermo intero; **Esc** esce dal fullscreen (o chiude la finestra).
- Finestra frameless: trascina la **toolbar** per spostarla, doppio-clic per massimizzare.
- Immagine/PDF/SVG/3D: rotella = zoom, trascina = pan/ruota, **Adatta** = reset; il contenuto si adatta automaticamente alla finestra.
- **PDF**: **PgUp/PgDn** o i pulsanti ◀/▶ cambiano pagina (le frecce ← → sono riservate alla navigazione tra file).
- CSV: scrivi nel box **🔍 filtra** per filtrare le righe.

### Integrazione col file manager
Punta l'apertura del file a `target/release/viewer "%f"` (il path passato come primo argomento viene aperto al lancio).

## Struttura: libreria riutilizzabile

Il progetto è un workspace Cargo a due crate, così la capacità di **decodificare velocemente molti formati** è riusabile da altri programmi, indipendentemente dall'interfaccia grafica:

- **`crates/viewer-core`** — libreria **senza dipendenze grafiche** (niente egui/GPU). Espone:
  - `decode(path) -> Decoded`: legge e interpreta un file in un valore `Send` con dati grezzi (`Decoded::Image { rgba, w, h, … }`, `Csv`, `Sheets`, `Text`, `Markdown`, `Pdf(bytes)`, `Mesh(MeshData)`, `Error`).
  - `decode_with_limits(path, SizeLimits)` / `SizeLimits`: stesso decode ma con **size-guard configurabile** per famiglia di formato (testo 512 MB / immagini 64 MB / **3D 512 MB** / altro 128 MB). I file oltre il limite vengono rifiutati *prima* di leggerli in RAM, con messaggio chiaro. `SizeLimits::unlimited()` toglie il guard; `decode()` usa `SizeLimits::from_env()`, che parte dai default e li sovrascrive tutti con la variabile d'ambiente `VIEWER_MAX_MB` se impostata (così si alza il tetto senza ricompilare, es. `VIEWER_MAX_MB=2048` per modelli enormi). Il 3D ha una famiglia dedicata con tetto più alto perché i mesh sono spesso grossi ma si espandono poco in RAM, e comunque la decodifica è off-thread.
  - `spawn_decode(path, on_ready) -> Receiver<Decoded>`: esegue la decodifica su un thread worker e richiama `on_ready` quando il risultato è pronto. È il primitivo dietro l'«apertura istantanea»: avvii la decodifica, costruisci la finestra in parallelo, e raccogli il risultato quando arriva.
  - modulo `mesh` (feature `mesh`, **non** attiva di default): `decode_obj(bytes)` / `decode_gltf(path)` restituiscono `Decoded::Mesh(MeshData)`, geometria triangolare **agnostica rispetto alla GPU** — buffer di vertici interlacciati `[x,y,z, nx,ny,nz]` + indici `u32`, AABB per inquadrare la camera, normali calcolate se assenti. L'OBJ usa un **parser multi-thread interno** (nessuna dipendenza extra): il file viene diviso in blocchi allineati alle righe e parsato in parallelo (posizioni/normali, poi facce), saltando la deduplica con hashmap — il parsing è ~96% del costo di caricamento di un OBJ, quindi parallelizzarlo dà ~3-4× sui modelli grandi. Nessuna dipendenza grafica nel core: il rendering è responsabilità della UI (l'app `viewer` lo fa con una paint-callback glow + camera orbitale). Senza la feature, aprire OBJ/glTF restituisce un errore chiaro «supporto 3D non compilato». Abilitala solo nelle build che mostrano modelli 3D — è il significato di «modulare e caricato solo se necessario»: il costo (`gltf`, shader e buffer GPU) si paga solo quando serve.
  - modulo `pdf` (feature `pdf`, attiva di default): `bind_pdfium()` e `pdf_worker(...)` servono il PDF off-thread, agnostico rispetto alla UI (segnala l'avanzamento con una callback `wake` qualsiasi). Il worker risponde a `PdfReq::Render(pagina)` con la pagina rasterizzata e a `PdfReq::Text(pagina)` con il **layer di testo**: `PdfMsg::Text { words, page_w_pt, page_h_pt }`, dove ogni `PdfWord` porta testo + box in **coordinate normalizzate** (`0..1`, origine in alto a sinistra, indipendenti dallo zoom) — la base per evidenziare/localizzare un campo (prezzo, EAN) e il click-to-locate. L'estrazione testo è molto più economica del rendering ed è on-demand per pagina. Senza la feature, `decode()` restituisce comunque i byte del PDF (`Decoded::Pdf`).
- **`crates/viewer`** — l'applicazione (egui/eframe): finestra, IPC a istanza singola, Quick Look, e i widget di rendering.

#### Architettura modulare dei formati (registry)

Il dispatch non è un `match` centrale che cresce a ogni formato: ogni modulo decoder **si descrive da sé** esponendo una slice `FORMATS: &[Format]`, e `decode_with_limits` le aggrega in un unico registro.

```rust
pub struct Format {
    pub exts: &'static [&'static str], // estensioni rivendicate
    pub family: Family,                // budget di memoria (Text/Image/Other)
    pub decode: fn(Input) -> Decoded,  // Input { path, bytes }
}
// es. nel modulo mesh:
pub(crate) const FORMATS: &[Format] = &[
    Format { exts: &["obj"],          family: Family::Other, decode: obj_entry },
    Format { exts: &["gltf", "glb"],  family: Family::Other, decode: gltf_entry },
];
```

Aggiungere un formato = **un modulo nuovo + una riga** (più la sua `#[cfg(feature = …)]` se opzionale); il core `decode()` non si tocca. Il flusso resta **performante**: si risolve il formato dall'estensione → si applica il size-guard per `Family` *prima* di leggere il file → si legge una sola volta e si passa per valore al decoder (es. il PDF inoltra il buffer senza copiarlo). I formati gated (3D) registrano comunque le loro estensioni: senza la feature vengono *riconosciute* ma decodificano in un errore «non compilato», anziché cadere nel fallback testo.

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
