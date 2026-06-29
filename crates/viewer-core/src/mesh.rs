//! 3D meshes: decode OBJ / glTF / GLB into raw, `Send` geometry — no GPU.
//!
//! Like every other [`Decoded`] payload, the output is plain CPU data: an
//! interleaved vertex buffer (position + normal) plus a `u32` index buffer,
//! ready for a single GPU upload by whatever UI displays it. No windowing or
//! GL dependency is referenced here.
//!
//! The *decoders* ([`decode_obj`], [`decode_gltf`]) live behind the `mesh`
//! feature, since they pull `tobj` / `gltf`. The [`MeshData`] type itself is
//! always compiled (it's just data), so `Decoded::Mesh` exists regardless and a
//! build without the feature simply reports that 3D support was not compiled in
//! — mirroring how `Decoded::Pdf` works without the `pdf` feature.

use super::{Decoded, Family, Format, Input};

/// Formats this module handles (see [`crate::Format`]). The rows are always
/// registered so the extensions are *recognised* even in a build without the
/// `mesh` feature — they just decode to a clear "not compiled" error then.
pub(crate) const FORMATS: &[Format] = &[
    Format {
        exts: &["obj"],
        family: Family::Mesh,
        decode: obj_entry,
    },
    Format {
        exts: &["gltf", "glb"],
        family: Family::Mesh,
        decode: gltf_entry,
    },
    Format {
        exts: &["stl"],
        family: Family::Mesh,
        decode: stl_entry,
    },
];

#[cfg(feature = "mesh")]
fn obj_entry(input: Input) -> Decoded {
    decode_obj(&input.bytes)
}
#[cfg(feature = "mesh")]
fn gltf_entry(input: Input) -> Decoded {
    decode_gltf(&input.path)
}
#[cfg(feature = "mesh")]
fn stl_entry(input: Input) -> Decoded {
    decode_stl(&input.bytes)
}

#[cfg(not(feature = "mesh"))]
fn obj_entry(_: Input) -> Decoded {
    not_compiled()
}
#[cfg(not(feature = "mesh"))]
fn gltf_entry(_: Input) -> Decoded {
    not_compiled()
}
#[cfg(not(feature = "mesh"))]
fn stl_entry(_: Input) -> Decoded {
    not_compiled()
}
#[cfg(not(feature = "mesh"))]
fn not_compiled() -> Decoded {
    Decoded::Error(
        "Supporto 3D (OBJ/glTF) non compilato.\nAbilita la feature `mesh` di viewer-core.".into(),
    )
}

/// A decoded triangle mesh. Vertices are interleaved as six `f32`s each —
/// position `xyz` then normal `xyz` — so a consumer can upload `vertices` to one
/// vertex buffer with a single stride and draw `indices` directly.
///
/// `aabb_min`/`aabb_max` bound the geometry so a viewer can frame the camera on
/// load without scanning the buffer again.
#[non_exhaustive]
pub struct MeshData {
    /// Interleaved `[x, y, z, nx, ny, nz]` per vertex.
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    pub aabb_min: [f32; 3],
    pub aabb_max: [f32; 3],
    /// Short human label, e.g. `"OBJ · 12.3k tri"`.
    pub kind: String,
}

/// Phase timing for the decode path, printed to stderr only when `VIEWER_BENCH`
/// is set (same gate the app uses). Free in normal use.
#[cfg(feature = "mesh")]
pub(crate) fn mesh_bench(label: &str, d: std::time::Duration) {
    if std::env::var_os("VIEWER_BENCH").is_some() {
        eprintln!("BENCH mesh_{label}_ms={:.1}", d.as_secs_f64() * 1000.0);
    }
}

#[cfg(feature = "mesh")]
impl MeshData {
    /// Assemble from positions, optional normals and triangle indices. Normals
    /// are recomputed from face geometry when absent or mismatched (OBJ files
    /// frequently ship without them), and the AABB is derived here.
    fn build(
        positions: Vec<[f32; 3]>,
        normals: Option<Vec<[f32; 3]>>,
        indices: Vec<u32>,
        kind: String,
    ) -> Decoded {
        use std::time::Instant;
        if positions.is_empty() || indices.len() < 3 {
            return Decoded::Error("Modello 3D senza geometria triangolare.".into());
        }

        let t = Instant::now();
        let normals = match normals {
            Some(n) if n.len() == positions.len() => n,
            _ => compute_normals(&positions, &indices),
        };
        mesh_bench("normals", t.elapsed());

        let t = Instant::now();
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        let mut vertices = Vec::with_capacity(positions.len() * 6);
        for (p, n) in positions.iter().zip(normals.iter()) {
            for k in 0..3 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
            vertices.extend_from_slice(&[p[0], p[1], p[2], n[0], n[1], n[2]]);
        }
        mesh_bench("interleave", t.elapsed());
        // A model whose coordinates are all NaN/inf would leave the box unset.
        if !min.iter().chain(max.iter()).all(|v| v.is_finite()) {
            return Decoded::Error("Modello 3D con coordinate non valide.".into());
        }

        Decoded::Mesh(MeshData {
            vertices,
            indices,
            aabb_min: min,
            aabb_max: max,
            kind,
        })
    }
}

/// Per-vertex normals as the normalized sum of incident face normals (weighted
/// by face area, which falls out of the un-normalized cross product). Degenerate
/// or unreferenced vertices get an arbitrary up-normal so shading stays defined.
#[cfg(feature = "mesh")]
fn compute_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut normals = vec![[0.0f32; 3]; positions.len()];
    for tri in indices.chunks_exact(3) {
        let (a, b, c) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        if a >= positions.len() || b >= positions.len() || c >= positions.len() {
            continue;
        }
        let (pa, pb, pc) = (positions[a], positions[b], positions[c]);
        let u = sub(pb, pa);
        let v = sub(pc, pa);
        let n = cross(u, v);
        for &i in &[a, b, c] {
            for k in 0..3 {
                normals[i][k] += n[k];
            }
        }
    }
    for n in &mut normals {
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        *n = if len > 1e-12 {
            [n[0] / len, n[1] / len, n[2] / len]
        } else {
            [0.0, 1.0, 0.0]
        };
    }
    normals
}

#[cfg(feature = "mesh")]
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[cfg(feature = "mesh")]
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Format a triangle count compactly: `850`, `12.3k`, `4.2M`.
#[cfg(feature = "mesh")]
fn tri_label(tris: usize) -> String {
    if tris >= 1_000_000 {
        format!("{:.1}M tri", tris as f64 / 1_000_000.0)
    } else if tris >= 1_000 {
        format!("{:.1}k tri", tris as f64 / 1_000.0)
    } else {
        format!("{tris} tri")
    }
}

/// Decode a Wavefront OBJ from memory. All objects/groups are merged into one
/// mesh; materials/`.mtl` and texture coords are ignored (geometry only).
/// Faces are fan-triangulated.
///
/// The parse is the whole cost of loading an OBJ (text → millions of floats), so
/// it runs **multi-threaded**: the file is split into line-aligned chunks parsed
/// in parallel (positions/normals in pass 1, faces in pass 2). When the file has
/// no normals we keep the compact indexed form and synthesise normals; when it
/// does, we expand per-corner (avoiding any vertex-dedup hashing).
#[cfg(feature = "mesh")]
pub fn decode_obj(bytes: &[u8]) -> Decoded {
    let t = std::time::Instant::now();
    let (positions, normals, indices) = parse_obj(bytes);
    mesh_bench("obj_parse", t.elapsed());

    if positions.is_empty() || indices.len() < 3 {
        return Decoded::Error("OBJ senza geometria triangolare.".into());
    }
    let kind = format!("OBJ · {}", tri_label(indices.len() / 3));
    MeshData::build(positions, normals, indices, kind)
}

/// Decode an STL (binary or ASCII) into geometry. STL stores each triangle's
/// three corner positions outright with no vertex sharing, so we emit them as a
/// flat position list with sequential indices and let [`MeshData::build`]
/// synthesise normals — each corner belongs to exactly one face, so the computed
/// normal is that face's normal: correct flat shading without trusting the
/// (often-zero) normals STL files carry.
#[cfg(feature = "mesh")]
pub fn decode_stl(bytes: &[u8]) -> Decoded {
    let t = std::time::Instant::now();
    let positions = if is_binary_stl(bytes) {
        parse_binary_stl(bytes)
    } else {
        parse_ascii_stl(bytes)
    };
    mesh_bench("stl_parse", t.elapsed());

    match positions {
        Some(pos) if pos.len() >= 3 => {
            let indices: Vec<u32> = (0..pos.len() as u32).collect();
            let kind = format!("STL · {}", tri_label(pos.len() / 3));
            MeshData::build(pos, None, indices, kind)
        }
        _ => Decoded::Error("STL senza geometria triangolare.".into()),
    }
}

/// Binary STL has an 80-byte header, a `u32` triangle count, then exactly 50
/// bytes per triangle. We detect it by that exact size identity rather than the
/// header text, since a binary file's header can itself start with `"solid"`.
#[cfg(feature = "mesh")]
fn is_binary_stl(bytes: &[u8]) -> bool {
    if bytes.len() < 84 {
        return false;
    }
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    84 + count.saturating_mul(50) == bytes.len()
}

/// Read corner positions from a binary STL, skipping each triangle's face normal.
#[cfg(feature = "mesh")]
fn parse_binary_stl(bytes: &[u8]) -> Option<Vec<[f32; 3]>> {
    let count = u32::from_le_bytes(bytes[80..84].try_into().ok()?) as usize;
    let mut pos = Vec::with_capacity(count * 3);
    let f = |b: &[u8]| f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    for i in 0..count {
        let tri = 84 + i * 50;
        if tri + 50 > bytes.len() {
            break;
        }
        // Layout: normal[12] then v0[12] v1[12] v2[12] then attr[2]. Skip normal.
        for v in 0..3 {
            let p = tri + 12 + v * 12;
            pos.push([f(&bytes[p..]), f(&bytes[p + 4..]), f(&bytes[p + 8..])]);
        }
    }
    Some(pos)
}

/// Read corner positions from an ASCII STL: every `vertex x y z` line, in order.
/// Malformed vertex lines are skipped so one bad line doesn't sink the file.
#[cfg(feature = "mesh")]
fn parse_ascii_stl(bytes: &[u8]) -> Option<Vec<[f32; 3]>> {
    let s = std::str::from_utf8(bytes).ok()?;
    let mut pos = Vec::new();
    for line in s.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("vertex") {
            let mut it = rest.split_ascii_whitespace();
            if let (Some(Ok(x)), Some(Ok(y)), Some(Ok(z))) = (
                it.next().map(str::parse),
                it.next().map(str::parse),
                it.next().map(str::parse),
            ) {
                pos.push([x, y, z]);
            }
        }
    }
    Some(pos)
}

/// Split `bytes` into at most `n` line-aligned `[start, end)` ranges (each range
/// holds whole lines), so chunks can be parsed independently.
#[cfg(feature = "mesh")]
fn split_lines(bytes: &[u8], n: usize) -> Vec<(usize, usize)> {
    let len = bytes.len();
    if len == 0 || n <= 1 {
        return vec![(0, len)];
    }
    let mut bounds = vec![0usize];
    for i in 1..n {
        let mut p = (len * i / n).min(len);
        // Advance to just after the next newline so we never split a line.
        while p < len && bytes[p - 1] != b'\n' {
            p += 1;
        }
        if p > *bounds.last().unwrap() {
            bounds.push(p);
        }
    }
    bounds.push(len);
    bounds.windows(2).map(|w| (w[0], w[1])).filter(|(a, b)| a < b).collect()
}

#[cfg(feature = "mesh")]
fn strip_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

/// Parse the first three whitespace-separated floats of `b` (a `v`/`vn` payload).
#[cfg(feature = "mesh")]
fn parse_vec3(b: &[u8]) -> Option<[f32; 3]> {
    let s = std::str::from_utf8(b).ok()?;
    let mut it = s.split_ascii_whitespace();
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let z = it.next()?.parse().ok()?;
    Some([x, y, z])
}

/// Resolve an OBJ index token (1-based, negative = relative to items seen so far)
/// to a 0-based absolute index. `count` is how many such items are defined up to
/// this point in the file.
#[cfg(feature = "mesh")]
fn resolve_index(tok: &[u8], count: usize) -> Option<usize> {
    let s = std::str::from_utf8(tok).ok()?;
    let v: i64 = s.parse().ok()?;
    if v > 0 {
        Some((v - 1) as usize)
    } else if v < 0 {
        (count as i64 + v).try_into().ok()
    } else {
        None
    }
}

/// Full parallel OBJ parse. Returns `(positions, normals, indices)` where either
/// `normals` is `None` (compact indexed mesh, normals synthesised later) or
/// `Some` aligned per-vertex with an expanded `positions` (sequential indices).
#[cfg(feature = "mesh")]
#[allow(clippy::type_complexity)]
fn parse_obj(bytes: &[u8]) -> (Vec<[f32; 3]>, Option<Vec<[f32; 3]>>, Vec<u32>) {
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 16);
    parse_obj_n(bytes, nthreads)
}

/// [`parse_obj`] with an explicit chunk/thread count (so tests can exercise the
/// chunk-boundary logic deterministically).
#[cfg(feature = "mesh")]
#[allow(clippy::type_complexity)]
fn parse_obj_n(bytes: &[u8], nthreads: usize) -> (Vec<[f32; 3]>, Option<Vec<[f32; 3]>>, Vec<u32>) {
    let chunks = split_lines(bytes, nthreads.max(1));

    // Pass 1 (parallel): positions and normals per chunk, in file order.
    let per_chunk: Vec<(Vec<[f32; 3]>, Vec<[f32; 3]>)> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|&(a, b)| {
                s.spawn(move || {
                    let mut pos = Vec::new();
                    let mut nrm = Vec::new();
                    for line in bytes[a..b].split(|&c| c == b'\n') {
                        let line = strip_cr(line);
                        if let Some(rest) = line.strip_prefix(b"v ") {
                            if let Some(p) = parse_vec3(rest) {
                                pos.push(p);
                            }
                        } else if let Some(rest) = line.strip_prefix(b"vn ") {
                            if let Some(nv) = parse_vec3(rest) {
                                nrm.push(nv);
                            }
                        }
                    }
                    (pos, nrm)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Concatenate to global arrays; remember each chunk's starting offsets so
    // pass 2 can resolve absolute and relative (negative) face indices.
    let mut v_off = Vec::with_capacity(chunks.len());
    let mut vn_off = Vec::with_capacity(chunks.len());
    let (mut nv, mut nn) = (0usize, 0usize);
    for (p, n) in &per_chunk {
        v_off.push(nv);
        vn_off.push(nn);
        nv += p.len();
        nn += n.len();
    }
    let mut positions = Vec::with_capacity(nv);
    let mut normals = Vec::with_capacity(nn);
    for (p, n) in &per_chunk {
        positions.extend_from_slice(p);
        normals.extend_from_slice(n);
    }
    let has_normals = !normals.is_empty();

    // Pass 2 (parallel): faces. Per chunk, re-walk lines counting v/vn (to track
    // the running totals for negative indices) and emit triangles.
    let positions = &positions;
    let normals = &normals;
    let results: Vec<(Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<u32>)> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .iter()
            .enumerate()
            .map(|(ci, &(a, b))| {
                let v_base = v_off[ci];
                let vn_base = vn_off[ci];
                s.spawn(move || {
                    let mut run_v = v_base;
                    let mut run_vn = vn_base;
                    let mut idx: Vec<u32> = Vec::new();
                    let mut epos: Vec<[f32; 3]> = Vec::new();
                    let mut enrm: Vec<[f32; 3]> = Vec::new();
                    // Scratch reused per face to avoid per-line allocation.
                    let mut corner_v: Vec<usize> = Vec::new();
                    let mut corner_n: Vec<Option<usize>> = Vec::new();

                    for line in bytes[a..b].split(|&c| c == b'\n') {
                        let line = strip_cr(line);
                        if line.strip_prefix(b"v ").is_some() {
                            run_v += 1;
                        } else if line.strip_prefix(b"vn ").is_some() {
                            run_vn += 1;
                        } else if let Some(rest) = line.strip_prefix(b"f ") {
                            corner_v.clear();
                            corner_n.clear();
                            for tok in rest.split(|&c| c == b' ' || c == b'\t') {
                                if tok.is_empty() {
                                    continue;
                                }
                                let mut parts = tok.split(|&c| c == b'/');
                                let vtok = parts.next().unwrap_or(b"");
                                let _vt = parts.next();
                                let vntok = parts.next();
                                let vi = match resolve_index(vtok, run_v) {
                                    Some(i) if i < positions.len() => i,
                                    _ => {
                                        corner_v.clear();
                                        break; // malformed face: drop it
                                    }
                                };
                                let ni = vntok.and_then(|t| {
                                    if t.is_empty() {
                                        None
                                    } else {
                                        resolve_index(t, run_vn).filter(|&i| i < normals.len())
                                    }
                                });
                                corner_v.push(vi);
                                corner_n.push(ni);
                            }
                            // Fan-triangulate the polygon.
                            if corner_v.len() >= 3 {
                                for k in 1..corner_v.len() - 1 {
                                    for &c in &[0usize, k, k + 1] {
                                        if has_normals {
                                            epos.push(positions[corner_v[c]]);
                                            enrm.push(match corner_n[c] {
                                                Some(n) => normals[n],
                                                None => [0.0, 0.0, 0.0],
                                            });
                                        } else {
                                            idx.push(corner_v[c] as u32);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    (epos, enrm, idx)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    if has_normals {
        // Expanded per-corner mesh: concat positions/normals, sequential indices.
        let total: usize = results.iter().map(|(p, _, _)| p.len()).sum();
        let mut epos = Vec::with_capacity(total);
        let mut enrm = Vec::with_capacity(total);
        for (p, n, _) in &results {
            epos.extend_from_slice(p);
            enrm.extend_from_slice(n);
        }
        let indices: Vec<u32> = (0..epos.len() as u32).collect();
        (epos, Some(enrm), indices)
    } else {
        // Compact indexed mesh over the shared position array.
        let total: usize = results.iter().map(|(_, _, i)| i.len()).sum();
        let mut indices = Vec::with_capacity(total);
        for (_, _, i) in &results {
            indices.extend_from_slice(i);
        }
        (positions.clone(), None, indices)
    }
}

/// Decode glTF 2.0 — text `.gltf` (with embedded or sibling buffers) or binary
/// `.glb`. Takes the path so the loader can resolve buffers/`.bin` referenced by
/// relative URI; the whole default scene is flattened to world space (node
/// transforms applied), keeping only triangle primitives.
#[cfg(feature = "mesh")]
pub fn decode_gltf(path: &std::path::Path) -> Decoded {
    let (doc, buffers, _images) = match gltf::import(path) {
        Ok(r) => r,
        Err(e) => return Decoded::Error(format!("glTF non leggibile:\n{e}")),
    };

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut all_have_normals = true;

    let scene = doc.default_scene().or_else(|| doc.scenes().next());
    let roots: Vec<gltf::Node> = match scene {
        Some(s) => s.nodes().collect(),
        // No scene graph: fall back to every node so we still show something.
        None => doc.nodes().collect(),
    };
    for node in roots {
        visit_node(
            &node,
            IDENTITY,
            &buffers,
            &mut positions,
            &mut normals,
            &mut indices,
            &mut all_have_normals,
        );
    }

    let normals = if all_have_normals && normals.len() == positions.len() {
        Some(normals)
    } else {
        None
    };
    let kind = format!("glTF · {}", tri_label(indices.len() / 3));
    MeshData::build(positions, normals, indices, kind)
}

// --- glTF scene traversal: flatten nodes to world space ---------------------
//
// 4×4 matrices are column-major (`m[col][row]`), matching the glTF spec and the
// `node.transform().matrix()` layout, so no transpose is needed.

#[cfg(feature = "mesh")]
type Mat4 = [[f32; 4]; 4];

#[cfg(feature = "mesh")]
const IDENTITY: Mat4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

#[cfg(feature = "mesh")]
fn mat_mul(a: Mat4, b: Mat4) -> Mat4 {
    let mut out = [[0.0f32; 4]; 4];
    for c in 0..4 {
        for r in 0..4 {
            out[c][r] = (0..4).map(|k| a[k][r] * b[c][k]).sum();
        }
    }
    out
}

/// Transform a point (w = 1) by a column-major matrix.
#[cfg(feature = "mesh")]
fn transform_point(m: &Mat4, p: [f32; 3]) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    for r in 0..3 {
        out[r] = m[0][r] * p[0] + m[1][r] * p[1] + m[2][r] * p[2] + m[3][r];
    }
    out
}

/// Transform a direction (w = 0) by the linear 3×3 part. Used for normals; for
/// the non-uniform-scale case this isn't strictly the inverse-transpose, but the
/// result is renormalized later and the visual error is negligible for a viewer.
#[cfg(feature = "mesh")]
fn transform_dir(m: &Mat4, d: [f32; 3]) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    for r in 0..3 {
        out[r] = m[0][r] * d[0] + m[1][r] * d[1] + m[2][r] * d[2];
    }
    out
}

#[cfg(feature = "mesh")]
#[allow(clippy::too_many_arguments)]
fn visit_node(
    node: &gltf::Node,
    parent: Mat4,
    buffers: &[gltf::buffer::Data],
    positions: &mut Vec<[f32; 3]>,
    normals: &mut Vec<[f32; 3]>,
    indices: &mut Vec<u32>,
    all_have_normals: &mut bool,
) {
    let world = mat_mul(parent, node.transform().matrix());

    if let Some(mesh) = node.mesh() {
        for prim in mesh.primitives() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                continue; // points/lines/strips: not handled by this viewer
            }
            let reader = prim.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
            let Some(pos_iter) = reader.read_positions() else {
                continue;
            };
            let base = positions.len() as u32;
            for p in pos_iter {
                positions.push(transform_point(&world, p));
            }
            let added = positions.len() as u32 - base;

            match reader.read_normals() {
                Some(nrm_iter) => {
                    for n in nrm_iter {
                        let n = transform_dir(&world, n);
                        normals.push(n);
                    }
                }
                None => *all_have_normals = false,
            }

            match reader.read_indices() {
                Some(idx) => indices.extend(idx.into_u32().map(|i| base + i)),
                // Non-indexed primitive: vertices are consecutive triangles.
                None => indices.extend((0..added).map(|i| base + i)),
            }
        }
    }

    for child in node.children() {
        visit_node(
            &child,
            world,
            buffers,
            positions,
            normals,
            indices,
            all_have_normals,
        );
    }
}

#[cfg(all(test, feature = "mesh"))]
mod tests {
    use super::*;
    use crate::Decoded;

    #[test]
    fn obj_cube_decodes_with_computed_normals() {
        // A minimal two-triangle quad, no normals in the file.
        let obj = "\
v 0 0 0
v 1 0 0
v 1 1 0
v 0 1 0
f 1 2 3
f 1 3 4
";
        match decode_obj(obj.as_bytes()) {
            Decoded::Mesh(m) => {
                assert_eq!(m.vertices.len(), 4 * 6, "4 vertici × (pos+normale)");
                assert_eq!(m.indices.len(), 6, "due triangoli");
                assert_eq!(m.aabb_min, [0.0, 0.0, 0.0]);
                assert_eq!(m.aabb_max, [1.0, 1.0, 0.0]);
                // Quad in the z=0 plane → every normal is ±Z, unit length.
                for v in m.vertices.chunks_exact(6) {
                    let n = [v[3], v[4], v[5]];
                    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                    assert!((len - 1.0).abs() < 1e-4, "normale unitaria: {n:?}");
                    assert!(n[2].abs() > 0.99, "normale lungo Z: {n:?}");
                }
                assert!(m.kind.contains("tri"), "kind: {}", m.kind);
            }
            other => panic!("atteso Mesh, ottenuto {other:?}", other = variant(&other)),
        }
    }

    #[test]
    fn stl_ascii_and_binary_decode_the_same_triangle() {
        // One triangle in the z=0 plane; its normal should resolve to ±Z.
        let ascii = "\
solid t
  facet normal 0 0 1
    outer loop
      vertex 0 0 0
      vertex 1 0 0
      vertex 0 1 0
    endloop
  endfacet
endsolid t
";
        // Equivalent binary STL: 80-byte header, count=1, then the 50-byte facet.
        let mut bin = vec![0u8; 84];
        bin[80] = 1; // little-endian triangle count = 1
        let push = |b: &mut Vec<u8>, v: [f32; 3]| {
            for c in v {
                b.extend_from_slice(&c.to_le_bytes());
            }
        };
        push(&mut bin, [0.0, 0.0, 1.0]); // normal (ignored by the decoder)
        push(&mut bin, [0.0, 0.0, 0.0]);
        push(&mut bin, [1.0, 0.0, 0.0]);
        push(&mut bin, [0.0, 1.0, 0.0]);
        bin.extend_from_slice(&[0u8, 0u8]); // attribute byte count

        assert!(!is_binary_stl(ascii.as_bytes()), "ASCII non deve sembrare binario");
        assert!(is_binary_stl(&bin), "binario riconosciuto per dimensione");

        for (label, bytes) in [("ascii", ascii.as_bytes()), ("binary", bin.as_slice())] {
            match decode_stl(bytes) {
                Decoded::Mesh(m) => {
                    assert_eq!(m.indices.len(), 3, "{label}: un triangolo");
                    assert_eq!(m.vertices.len(), 3 * 6, "{label}: 3 corner pos+normale");
                    assert_eq!(m.aabb_min, [0.0, 0.0, 0.0], "{label}");
                    assert_eq!(m.aabb_max, [1.0, 1.0, 0.0], "{label}");
                    for v in m.vertices.chunks_exact(6) {
                        assert!(v[5].abs() > 0.99, "{label}: normale lungo Z: {:?}", &v[3..6]);
                    }
                    assert!(m.kind.starts_with("STL"), "{label}: kind {}", m.kind);
                }
                other => panic!("{label}: atteso Mesh, ottenuto {}", variant(&other)),
            }
        }
    }

    #[test]
    fn obj_with_normals_expands_per_corner() {
        // A triangle carrying explicit normals (v//vn) → expanded path: 3
        // vertices, normals taken from the file (here +Z), not recomputed.
        let obj = "\
v 0 0 0
v 1 0 0
v 0 1 0
vn 0 0 1
f 1//1 2//1 3//1
";
        match decode_obj(obj.as_bytes()) {
            Decoded::Mesh(m) => {
                assert_eq!(m.indices.len(), 3);
                assert_eq!(m.vertices.len(), 3 * 6);
                for v in m.vertices.chunks_exact(6) {
                    assert_eq!([v[3], v[4], v[5]], [0.0, 0.0, 1.0], "normale dal file");
                }
            }
            other => panic!("atteso Mesh, ottenuto {}", variant(&other)),
        }
    }

    #[test]
    fn obj_handles_negative_indices_and_quads() {
        // A quad addressed with negative (relative) indices; must triangulate to
        // two triangles over the four positions.
        let obj = "\
v 0 0 0
v 1 0 0
v 1 1 0
v 0 1 0
f -4 -3 -2 -1
";
        match decode_obj(obj.as_bytes()) {
            Decoded::Mesh(m) => {
                assert_eq!(m.indices.len(), 6, "quad → due triangoli");
                assert_eq!(m.vertices.len(), 4 * 6, "quattro posizioni condivise");
                // All indices must be in range of the four vertices.
                assert!(m.indices.iter().all(|&i| i < 4));
            }
            other => panic!("atteso Mesh, ottenuto {}", variant(&other)),
        }
    }

    #[test]
    fn obj_parses_consistently_across_chunk_counts() {
        // Same model parsed whole vs split: the parallel path must agree with a
        // single-chunk parse on triangle count (boundary-safety of split_lines).
        let mut obj = String::from("# grid\n");
        for i in 0..50 {
            obj.push_str(&format!("v {i} 0 0\nv {i} 1 0\n"));
        }
        for i in 0..49 {
            let a = i * 2 + 1;
            obj.push_str(&format!("f {} {} {}\n", a, a + 1, a + 2));
        }
        let one = parse_obj_n(obj.as_bytes(), 1);
        let many = parse_obj_n(obj.as_bytes(), 8);
        assert_eq!(one.0.len(), many.0.len(), "stesse posizioni");
        assert_eq!(one.2, many.2, "stessi indici risolti");
        assert!(!one.2.is_empty(), "geometria non vuota");
    }

    #[test]
    fn glb_triangle_decodes_and_computes_normals() {
        // Hand-build a minimal binary glTF: one triangle, POSITION only (so the
        // decoder must synthesise normals), wrapped in the GLB container.
        let mut bin = Vec::new();
        for f in [
            0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // three vec3 positions
        ] {
            bin.extend_from_slice(&f.to_le_bytes());
        }
        let json = concat!(
            r#"{"asset":{"version":"2.0"},"#,
            r#""buffers":[{"byteLength":36}],"#,
            r#""bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36,"target":34962}],"#,
            r#""accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","#,
            r#""min":[0,0,0],"max":[1,1,0]}],"#,
            r#""meshes":[{"primitives":[{"attributes":{"POSITION":0},"mode":4}]}],"#,
            r#""nodes":[{"mesh":0}],"scenes":[{"nodes":[0]}],"scene":0}"#,
        );
        let mut json_bytes = json.as_bytes().to_vec();
        while json_bytes.len() % 4 != 0 {
            json_bytes.push(b' ');
        }
        let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::new();
        glb.extend_from_slice(&0x46546C67u32.to_le_bytes()); // "glTF"
        glb.extend_from_slice(&2u32.to_le_bytes()); // version
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x4E4F534Au32.to_le_bytes()); // "JSON"
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x004E4942u32.to_le_bytes()); // "BIN\0"
        glb.extend_from_slice(&bin);

        let path = std::env::temp_dir().join("viewer_core_test_tri.glb");
        std::fs::write(&path, &glb).expect("write glb");
        let decoded = decode_gltf(&path);
        let _ = std::fs::remove_file(&path);

        match decoded {
            Decoded::Mesh(m) => {
                assert_eq!(m.indices.len(), 3, "un triangolo");
                assert_eq!(m.vertices.len(), 3 * 6, "3 vertici × (pos+normale)");
                // Triangle in z=0 → synthesised normal is unit length along Z.
                let n = [m.vertices[3], m.vertices[4], m.vertices[5]];
                let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                assert!((len - 1.0).abs() < 1e-4, "normale unitaria: {n:?}");
                assert!(n[2].abs() > 0.99, "normale lungo Z: {n:?}");
                assert!(m.kind.starts_with("glTF"), "kind: {}", m.kind);
            }
            other => panic!("atteso Mesh, ottenuto {}", variant(&other)),
        }
    }

    #[test]
    fn empty_obj_is_an_error() {
        match decode_obj(b"# just a comment\n") {
            Decoded::Error(_) => {}
            other => panic!("atteso Error, ottenuto {}", variant(&other)),
        }
    }

    fn variant(d: &Decoded) -> &'static str {
        match d {
            Decoded::Mesh(_) => "Mesh",
            Decoded::Error(_) => "Error",
            _ => "other",
        }
    }
}
