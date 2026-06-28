//! 3D mesh view: an orbit camera over a glow-rendered triangle mesh.
//!
//! The geometry arrives as CPU data ([`viewer_core::MeshData`]); on first paint
//! we upload it to a GPU buffer and compile a small Lambert shader, then draw it
//! each frame through an egui paint callback. GL resources are created lazily —
//! nothing touches the GPU until a 3D file is actually shown.

use eframe::egui;
use eframe::egui_glow;
use eframe::glow::{self, HasContext};
use egui::mutex::Mutex;
use std::sync::Arc;
use viewer_core::MeshData;

/// Vertical field of view (radians) used both for framing and projection.
const FOVY: f32 = 45.0 * std::f32::consts::PI / 180.0;

pub(crate) struct MeshView {
    pub(crate) kind: String,
    /// Geometry waiting for its one-time GPU upload (taken on first paint).
    pending: Option<MeshData>,
    renderer: Option<Arc<Mutex<MeshRenderer>>>,
    error: Option<String>,

    // Orbit camera around `center`, looking in from distance `dist`.
    center: [f32; 3],
    radius: f32,
    yaw: f32,
    pitch: f32,
    dist: f32,
    /// Framing distance, the reset target for "Adatta".
    base_dist: f32,
}

impl MeshView {
    pub(crate) fn new(data: MeshData) -> Self {
        let center = [
            (data.aabb_min[0] + data.aabb_max[0]) * 0.5,
            (data.aabb_min[1] + data.aabb_max[1]) * 0.5,
            (data.aabb_min[2] + data.aabb_max[2]) * 0.5,
        ];
        let diag = {
            let d = [
                data.aabb_max[0] - data.aabb_min[0],
                data.aabb_max[1] - data.aabb_min[1],
                data.aabb_max[2] - data.aabb_min[2],
            ];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        };
        let radius = (diag * 0.5).max(1e-4);
        // Pull the camera back far enough to frame the bounding sphere, with a
        // little margin so the model doesn't touch the window edges.
        let base_dist = radius / (FOVY * 0.5).sin() * 1.2;
        MeshView {
            kind: data.kind.clone(),
            pending: Some(data),
            renderer: None,
            error: None,
            center,
            radius,
            yaw: 0.6,
            pitch: 0.45,
            dist: base_dist,
            base_dist,
        }
    }

    /// Reset the camera to the framing pose ("Adatta" button).
    pub(crate) fn reset(&mut self) {
        self.yaw = 0.6;
        self.pitch = 0.45;
        self.dist = self.base_dist;
    }
}

/// Render the mesh into the remaining space. `gl` is the live glow context from
/// `eframe::Frame::gl()`; without it (e.g. a non-glow backend) we can only show
/// a message.
pub(crate) fn mesh_view(
    ui: &mut egui::Ui,
    view: &mut MeshView,
    gl: Option<&Arc<glow::Context>>,
) {
    let Some(gl) = gl else {
        ui.centered_and_justified(|ui| {
            ui.label("Backend GPU non disponibile per il rendering 3D.");
        });
        return;
    };

    // One-time upload + shader compile, on the first frame we have geometry.
    if view.renderer.is_none() && view.error.is_none() {
        if let Some(data) = view.pending.take() {
            match MeshRenderer::new(gl, &data) {
                Ok(r) => view.renderer = Some(Arc::new(Mutex::new(r))),
                Err(e) => view.error = Some(e),
            }
        }
    }
    if let Some(e) = &view.error {
        ui.centered_and_justified(|ui| {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e);
        });
        return;
    }

    let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::drag());

    // Orbit on drag; clamp pitch shy of the poles to keep "up" well-defined.
    if response.dragged() {
        let d = response.drag_delta();
        view.yaw -= d.x * 0.01;
        view.pitch = (view.pitch + d.y * 0.01).clamp(-1.54, 1.54);
    }
    // Wheel / pinch dolly, bounded so the model can't be lost or clipped away.
    if response.hovered() {
        let wheel = ui.input(|i| i.raw_scroll_delta.y);
        let pinch = ui.input(|i| i.zoom_delta());
        let scroll = wheel + if pinch != 1.0 { (pinch - 1.0) * 200.0 } else { 0.0 };
        if scroll != 0.0 {
            let lo = view.radius * 0.2;
            let hi = view.base_dist * 8.0;
            view.dist = (view.dist * (-scroll * 0.0015).exp()).clamp(lo, hi);
        }
    }

    // Build the camera for this frame.
    let (cp, sp) = (view.pitch.cos(), view.pitch.sin());
    let (cy, sy) = (view.yaw.cos(), view.yaw.sin());
    let dir = [cp * sy, sp, cp * cy];
    let eye = [
        view.center[0] + dir[0] * view.dist,
        view.center[1] + dir[1] * view.dist,
        view.center[2] + dir[2] * view.dist,
    ];
    let aspect = (rect.width() / rect.height().max(1.0)).max(1e-3);
    let near = (view.dist - view.radius).max(view.radius * 0.01);
    let far = view.dist + view.radius * 2.0;
    let mvp = mat_mul(
        perspective(FOVY, aspect, near, far),
        look_at(eye, view.center, [0.0, 1.0, 0.0]),
    );

    let Some(renderer) = view.renderer.clone() else {
        return;
    };
    let callback = egui_glow::CallbackFn::new(move |_info, painter| {
        renderer.lock().paint(painter.gl(), mvp, eye);
    });
    ui.painter().add(egui::PaintCallback {
        rect,
        callback: Arc::new(callback),
    });
}

/// GPU-side mesh: a compiled program and an interleaved VBO/EBO pair.
struct MeshRenderer {
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    ebo: glow::Buffer,
    index_count: i32,
}

impl MeshRenderer {
    fn new(gl: &glow::Context, data: &MeshData) -> Result<Self, String> {
        use egui_glow::ShaderVersion;
        let header = ShaderVersion::get(gl).version_declaration();

        let vert_src = format!("{header}\n{VERT}");
        let frag_src = format!("{header}\n{FRAG}");

        unsafe {
            let program = gl.create_program().map_err(|e| e.to_string())?;
            let shaders = [
                (glow::VERTEX_SHADER, vert_src),
                (glow::FRAGMENT_SHADER, frag_src),
            ]
            .into_iter()
            .map(|(ty, src)| {
                let s = gl.create_shader(ty)?;
                gl.shader_source(s, &src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    return Err(format!("shader 3D: {}", gl.get_shader_info_log(s)));
                }
                gl.attach_shader(program, s);
                Ok(s)
            })
            .collect::<Result<Vec<_>, String>>()?;

            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("link 3D: {}", gl.get_program_info_log(program)));
            }
            for s in shaders {
                gl.detach_shader(program, s);
                gl.delete_shader(s);
            }

            let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
            let vbo = gl.create_buffer().map_err(|e| e.to_string())?;
            let ebo = gl.create_buffer().map_err(|e| e.to_string())?;

            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(
                glow::ARRAY_BUFFER,
                as_bytes(&data.vertices),
                glow::STATIC_DRAW,
            );
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
            gl.buffer_data_u8_slice(
                glow::ELEMENT_ARRAY_BUFFER,
                as_bytes(&data.indices),
                glow::STATIC_DRAW,
            );

            let stride = 6 * std::mem::size_of::<f32>() as i32;
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 3, glow::FLOAT, false, stride, 3 * 4);

            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);

            Ok(MeshRenderer {
                program,
                vao,
                vbo,
                ebo,
                index_count: data.indices.len() as i32,
            })
        }
    }

    fn paint(&self, gl: &glow::Context, mvp: [f32; 16], eye: [f32; 3]) {
        unsafe {
            // egui draws 2D without depth; give the mesh its own depth test and
            // a fresh depth range (scissored by egui to this callback's rect),
            // then hand the state back the way egui expects it.
            gl.enable(glow::DEPTH_TEST);
            gl.depth_func(glow::LESS);
            gl.clear_depth_f32(1.0);
            gl.clear(glow::DEPTH_BUFFER_BIT);
            gl.disable(glow::CULL_FACE); // meshes vary in winding; shade two-sided

            gl.use_program(Some(self.program));
            if let Some(loc) = gl.get_uniform_location(self.program, "u_mvp") {
                gl.uniform_matrix_4_f32_slice(Some(&loc), false, &mvp);
            }
            if let Some(loc) = gl.get_uniform_location(self.program, "u_eye") {
                gl.uniform_3_f32(Some(&loc), eye[0], eye[1], eye[2]);
            }
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_elements(glow::TRIANGLES, self.index_count, glow::UNSIGNED_INT, 0);
            gl.bind_vertex_array(None);

            gl.disable(glow::DEPTH_TEST);
        }
    }

    fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_program(self.program);
            gl.delete_vertex_array(self.vao);
            gl.delete_buffer(self.vbo);
            gl.delete_buffer(self.ebo);
        }
    }
}

impl MeshView {
    /// Release GPU resources. Called on app exit (the glow context is gone after).
    pub(crate) fn destroy(&self, gl: &glow::Context) {
        if let Some(r) = &self.renderer {
            r.lock().destroy(gl);
        }
    }
}

/// Reinterpret a `Copy` slice (`f32`/`u32`) as raw bytes for `buffer_data`.
fn as_bytes<T: Copy>(s: &[T]) -> &[u8] {
    // Safe: `T` is plain old data and the byte view is read-only and bounded to
    // the slice's own length.
    unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
}

const VERT: &str = r#"
in vec3 a_pos;
in vec3 a_nrm;
uniform mat4 u_mvp;
out vec3 v_nrm;
out vec3 v_pos;
void main() {
    v_nrm = a_nrm;
    v_pos = a_pos;
    gl_Position = u_mvp * vec4(a_pos, 1.0);
}
"#;

const FRAG: &str = r#"
precision mediump float;
in vec3 v_nrm;
in vec3 v_pos;
uniform vec3 u_eye;
out vec4 frag;
void main() {
    vec3 n = normalize(v_nrm);
    vec3 l = normalize(u_eye - v_pos);     // headlight at the camera
    float d = abs(dot(n, l));              // two-sided: light back faces too
    vec3 base = vec3(0.74, 0.76, 0.80);
    frag = vec4(base * (0.25 + 0.75 * d), 1.0);
}
"#;

// --- column-major 4×4 camera math (flat [f32;16] for glUniformMatrix) --------

fn mat_mul(a: [f32; 16], b: [f32; 16]) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            out[col * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[col * 4 + k]).sum();
        }
    }
    out
}

fn look_at(eye: [f32; 3], center: [f32; 3], up: [f32; 3]) -> [f32; 16] {
    let f = norm(sub(center, eye));
    let s = norm(cross(f, up));
    let u = cross(s, f);
    [
        s[0], u[0], -f[0], 0.0,
        s[1], u[1], -f[1], 0.0,
        s[2], u[2], -f[2], 0.0,
        -dot(s, eye), -dot(u, eye), dot(f, eye), 1.0,
    ]
}

fn perspective(fovy: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fovy * 0.5).tan();
    let nf = 1.0 / (near - far);
    [
        f / aspect, 0.0, 0.0, 0.0,
        0.0, f, 0.0, 0.0,
        0.0, 0.0, (far + near) * nf, -1.0,
        0.0, 0.0, 2.0 * far * near * nf, 0.0,
    ]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn norm(a: [f32; 3]) -> [f32; 3] {
    let l = dot(a, a).sqrt();
    if l > 1e-12 {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        [0.0, 0.0, 1.0]
    }
}
