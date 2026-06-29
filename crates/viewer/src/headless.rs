//! Headless screenshot rendering (`--headless`).
//!
//! Renders a file to a PNG with no window and no display server, so it works on a
//! bare server or in CI. We create a *surfaceless* OpenGL context via EGL
//! (`EGL_MESA_platform_surfaceless` + `EGL_KHR_surfaceless_context`), draw the same
//! egui UI as the windowed app into an offscreen framebuffer with `egui_glow`
//! (the very painter eframe uses), then read the pixels back and encode a PNG.
//!
//! This is the display-independent counterpart to `App::maybe_capture_screenshot`,
//! which captures a real window. Both reuse `App::ui` so the drawing is identical.

use crate::app::{configure_style, file_label, App};
use eframe::egui;
use eframe::glow::{self, HasContext};
use khronos_egl as egl;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use viewer_core::decode;

/// EGL platform enum for Mesa's surfaceless platform (`EGL_MESA_platform_surfaceless`).
const PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31DD;

/// Render `path` to `out` (a PNG) at `width`×`height` pixels, headlessly.
/// Returns a human-readable error string on any failure.
pub fn capture(path: &Path, out: &Path, width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("screenshot dimensions must be non-zero".into());
    }
    let gl = Arc::new(create_surfaceless_gl()?);

    // Offscreen target: an RGBA8 colour buffer plus a depth/stencil buffer (the 3D
    // mesh view needs depth to occlude correctly, exactly like the windowed app's
    // `depth_buffer: 24`).
    let fbo = unsafe { setup_framebuffer(&gl, width, height)? };

    let mut painter = egui_glow::Painter::new(gl.clone(), "", None, false)
        .map_err(|e| format!("egui_glow painter init failed: {e}"))?;

    let egui_ctx = egui::Context::default();
    configure_style(&egui_ctx);
    egui_ctx.set_pixels_per_point(1.0);

    // Decode synchronously (no UI thread to keep responsive here) and hand the
    // result to a normal `App` in its non-parallel mode.
    let decoded = decode(path);
    let mut app = App::new(None, false, None);
    app.parallel = false;
    app.file_name = file_label(path);
    app.file_path = Some(path.to_path_buf());
    app.finalize(&egui_ctx, path, decoded);

    // Drive frames until the content is actually on screen (PDF/mesh upload and the
    // PDF worker resolve over a few frames), then a few extra to let it settle.
    let start = Instant::now();
    let deadline = Duration::from_secs(15);
    let mut t = 0.0f64;
    loop {
        render_frame(&egui_ctx, &mut painter, &mut app, &gl, fbo, width, height, t);
        t += 1.0 / 60.0;
        if app.content_ready() || start.elapsed() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    if !app.content_ready() {
        eprintln!("viewer: content did not finish loading; capturing the current frame");
    }
    // Settle: a few more frames so the freshly-ready content paints fully.
    for _ in 0..3 {
        render_frame(&egui_ctx, &mut painter, &mut app, &gl, fbo, width, height, t);
        t += 1.0 / 60.0;
    }

    let pixels = unsafe { read_pixels(&gl, fbo, width, height) };
    painter.destroy();

    let img = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or("captured buffer had unexpected size")?;
    img.save(out)
        .map_err(|e| format!("failed to write {}: {e}", out.display()))?;
    eprintln!("viewer: screenshot saved to {}", out.display());
    Ok(())
}

/// Run one egui frame and paint it into `fbo`.
#[allow(clippy::too_many_arguments)]
fn render_frame(
    egui_ctx: &egui::Context,
    painter: &mut egui_glow::Painter,
    app: &mut App,
    gl: &Arc<glow::Context>,
    fbo: glow::Framebuffer,
    width: u32,
    height: u32,
    time: f64,
) {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(width as f32, height as f32),
        )),
        time: Some(time),
        focused: true,
        ..Default::default()
    };
    let output = egui_ctx.run(raw, |ctx| app.ui(ctx, Some(gl)));
    let ppp = output.pixels_per_point;
    let primitives = egui_ctx.tessellate(output.shapes, ppp);
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.disable(glow::SCISSOR_TEST);
        gl.viewport(0, 0, width as i32, height as i32);
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT | glow::STENCIL_BUFFER_BIT);
    }
    painter.paint_and_update_textures([width, height], ppp, &primitives, &output.textures_delta);
}

/// Create and bind an offscreen framebuffer with RGBA8 colour + depth/stencil.
unsafe fn setup_framebuffer(
    gl: &glow::Context,
    width: u32,
    height: u32,
) -> Result<glow::Framebuffer, String> {
    let fbo = gl
        .create_framebuffer()
        .map_err(|e| format!("create_framebuffer: {e}"))?;
    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));

    let color = gl
        .create_renderbuffer()
        .map_err(|e| format!("create_renderbuffer (color): {e}"))?;
    gl.bind_renderbuffer(glow::RENDERBUFFER, Some(color));
    gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, width as i32, height as i32);
    gl.framebuffer_renderbuffer(
        glow::FRAMEBUFFER,
        glow::COLOR_ATTACHMENT0,
        glow::RENDERBUFFER,
        Some(color),
    );

    let depth = gl
        .create_renderbuffer()
        .map_err(|e| format!("create_renderbuffer (depth): {e}"))?;
    gl.bind_renderbuffer(glow::RENDERBUFFER, Some(depth));
    gl.renderbuffer_storage(
        glow::RENDERBUFFER,
        glow::DEPTH24_STENCIL8,
        width as i32,
        height as i32,
    );
    gl.framebuffer_renderbuffer(
        glow::FRAMEBUFFER,
        glow::DEPTH_STENCIL_ATTACHMENT,
        glow::RENDERBUFFER,
        Some(depth),
    );

    let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
    if status != glow::FRAMEBUFFER_COMPLETE {
        return Err(format!("framebuffer incomplete (status {status:#x})"));
    }
    Ok(fbo)
}

/// Read the colour buffer back as top-down RGBA bytes (OpenGL reads bottom-up, so
/// the rows are flipped here).
unsafe fn read_pixels(gl: &glow::Context, fbo: glow::Framebuffer, width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut buf = vec![0u8; w * h * 4];
    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
    gl.read_buffer(glow::COLOR_ATTACHMENT0);
    gl.read_pixels(
        0,
        0,
        width as i32,
        height as i32,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        glow::PixelPackData::Slice(&mut buf),
    );
    // Flip vertically: GL's origin is bottom-left, PNG's is top-left.
    let row = w * 4;
    let mut flipped = vec![0u8; buf.len()];
    for y in 0..h {
        let src = (h - 1 - y) * row;
        let dst = y * row;
        flipped[dst..dst + row].copy_from_slice(&buf[src..src + row]);
    }
    flipped
}

/// Bring up a surfaceless EGL OpenGL context and load it into glow.
fn create_surfaceless_gl() -> Result<glow::Context, String> {
    let egl = egl::Instance::new(egl::Static);

    // Prefer Mesa's surfaceless platform; fall back to the default display, which
    // also supports `EGL_KHR_surfaceless_context` on most drivers.
    let display = unsafe {
        egl.get_platform_display(
            PLATFORM_SURFACELESS_MESA,
            egl::DEFAULT_DISPLAY,
            &[egl::ATTRIB_NONE],
        )
    }
    .or_else(|_| unsafe { egl.get_display(egl::DEFAULT_DISPLAY).ok_or(egl::Error::BadDisplay) })
    .map_err(|e| format!("no EGL display (is libEGL/Mesa installed?): {e}"))?;

    egl.initialize(display)
        .map_err(|e| format!("eglInitialize failed: {e}"))?;
    egl.bind_api(egl::OPENGL_API)
        .map_err(|e| format!("eglBindAPI(OpenGL) failed: {e}"))?;

    let config_attribs = [
        egl::SURFACE_TYPE,
        egl::PBUFFER_BIT,
        egl::RENDERABLE_TYPE,
        egl::OPENGL_BIT,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        egl::DEPTH_SIZE,
        24,
        egl::NONE,
    ];
    let config = egl
        .choose_first_config(display, &config_attribs)
        .map_err(|e| format!("eglChooseConfig failed: {e}"))?
        .ok_or("no suitable EGL config")?;

    let context_attribs = [
        egl::CONTEXT_MAJOR_VERSION,
        3,
        egl::CONTEXT_MINOR_VERSION,
        3,
        egl::CONTEXT_OPENGL_PROFILE_MASK,
        egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
        egl::NONE,
    ];
    let context = egl
        .create_context(display, config, None, &context_attribs)
        .map_err(|e| format!("eglCreateContext failed: {e}"))?;

    // Surfaceless: no draw/read surface, relying on EGL_KHR_surfaceless_context.
    egl.make_current(display, None, None, Some(context))
        .map_err(|e| format!("eglMakeCurrent (surfaceless) failed: {e}"))?;

    let gl = unsafe {
        glow::Context::from_loader_function(|s| {
            egl.get_proc_address(s)
                .map(|p| p as *const c_void)
                .unwrap_or(std::ptr::null())
        })
    };
    Ok(gl)
}
