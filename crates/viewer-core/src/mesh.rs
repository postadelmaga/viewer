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
];

#[cfg(feature = "mesh")]
fn obj_entry(input: Input) -> Decoded {
    decode_obj(&input.bytes)
}
#[cfg(feature = "mesh")]
fn gltf_entry(input: Input) -> Decoded {
    decode_gltf(&input.path)
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
        if positions.is_empty() || indices.len() < 3 {
            return Decoded::Error("Modello 3D senza geometria triangolare.".into());
        }

        let normals = match normals {
            Some(n) if n.len() == positions.len() => n,
            _ => compute_normals(&positions, &indices),
        };

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
/// mesh; materials and `.mtl` files are ignored (geometry only). Triangulated
/// and de-indexed to a single shared index buffer.
#[cfg(feature = "mesh")]
pub fn decode_obj(bytes: &[u8]) -> Decoded {
    use std::io::Cursor;

    let opts = tobj::LoadOptions {
        triangulate: true,
        single_index: true,
        ignore_points: true,
        ignore_lines: true,
    };
    // We render geometry only: satisfy any `mtllib` reference with empty
    // materials rather than failing the whole load on a missing `.mtl`.
    let mut reader = Cursor::new(bytes);
    let (models, _) = match tobj::load_obj_buf(&mut reader, &opts, |_| {
        Ok((Vec::new(), Default::default()))
    }) {
        Ok(r) => r,
        Err(e) => return Decoded::Error(format!("OBJ non leggibile:\n{e}")),
    };

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut all_have_normals = true;

    for model in &models {
        let mesh = &model.mesh;
        let base = positions.len() as u32;
        for p in mesh.positions.chunks_exact(3) {
            positions.push([p[0], p[1], p[2]]);
        }
        if mesh.normals.len() == mesh.positions.len() {
            for n in mesh.normals.chunks_exact(3) {
                normals.push([n[0], n[1], n[2]]);
            }
        } else {
            all_have_normals = false;
        }
        indices.extend(mesh.indices.iter().map(|i| base + i));
    }

    let normals = if all_have_normals && normals.len() == positions.len() {
        Some(normals)
    } else {
        None
    };
    let kind = format!("OBJ · {}", tri_label(indices.len() / 3));
    MeshData::build(positions, normals, indices, kind)
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
