//! The windows the glow is drawn in: one per edge, each flush against it.
//!
//! Windows are made here and nowhere else, whichever renderer draws into
//! them, so where they sit, what they stack above, that they ignore the
//! pointer and when they are shown or hidden exist once. A renderer only
//! presents pixels into the window it is given.

use std::num::NonZeroU32;

use glow::HasContext;
use glutin::{
    config::{Config, ConfigTemplateBuilder, GlConfig},
    context::{
        ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext,
        PossiblyCurrentGlContext, Version,
    },
    display::{GetGlDisplay, GlDisplay},
    prelude::GlSurface,
    surface::{Surface, SwapInterval, WindowSurface},
};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use syntra_plugin_api::Edge;
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event_loop::ActiveEventLoop,
    platform::x11::{WindowAttributesExtX11, WindowType},
    window::{Window, WindowAttributes, WindowLevel},
};

use crate::{
    paint::Paint,
    render::{Draw, RenderError},
    x11,
};

/// The X11 window id behind a window handle, if it has one.
///
/// `Xlib` and `Xcb` name the same id; Wayland, Windows and macOS windows have
/// none, and the CPU renderer for X11 cannot draw into them.
pub fn x11_window_id(handle: RawWindowHandle) -> Option<u32> {
    match handle {
        RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
        RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok(),
        _ => None,
    }
}

/// The attributes every glow window shares.
fn attributes(
    edge: Edge,
    origin: (i32, i32),
    screen: (u32, u32),
    thickness: u32,
) -> WindowAttributes {
    let (x, y, width, height) = x11::geometry(edge, origin, screen, thickness);
    Window::default_attributes()
        .with_title(format!("Syntra edge glow ({edge:?})"))
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_inner_size(PhysicalSize::new(width, height))
        .with_position(PhysicalPosition::new(x, y))
        .with_window_level(WindowLevel::AlwaysOnTop)
        // A notification, so the window manager neither decorates nor
        // focuses it. Only X11 reads this; elsewhere it is ignored.
        .with_x11_window_type(vec![WindowType::Notification])
        .with_visible(false)
}

/// One edge's window, hidden until something lights it.
pub struct EdgeWindow {
    /// Which edge it lies along.
    pub edge: Edge,
    window: Window,
    size: (u32, u32),
    visible: bool,
    click_through: bool,
}

impl EdgeWindow {
    /// Creates the window for `edge` without a GL context, for the CPU
    /// renderer and for a window that is only ever shown.
    pub fn plain(
        event_loop: &ActiveEventLoop,
        edge: Edge,
        origin: (i32, i32),
        screen: (u32, u32),
        thickness: u32,
    ) -> Result<Self, RenderError> {
        let window = event_loop
            .create_window(attributes(edge, origin, screen, thickness))
            .map_err(|error| RenderError(format!("cannot create the {edge:?} window: {error}")))?;
        let (_, _, width, height) = x11::geometry(edge, origin, screen, thickness);
        Ok(Self::wrap(edge, window, (width, height)))
    }

    fn wrap(edge: Edge, window: Window, size: (u32, u32)) -> Self {
        Self {
            edge,
            window,
            size,
            visible: false,
            click_through: false,
        }
    }

    /// The window's size in pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The identifier winit uses for this window in the events it delivers.
    pub fn id(&self) -> winit::window::WindowId {
        self.window.id()
    }

    /// The X11 id of the window, when it is one.
    pub fn x11_id(&self) -> Option<u32> {
        self.window
            .window_handle()
            .ok()
            .and_then(|handle| x11_window_id(handle.as_raw()))
    }

    /// Shows the window, raised above the others.
    ///
    /// Click-through is set after the window exists and has been shown once:
    /// a window that is not mapped yet may ignore the request.
    pub fn show(&mut self) {
        if !self.visible {
            self.window.set_visible(true);
            self.visible = true;
            crate::trace::line(&format!("{:?} window shown", self.edge));
        }
        if !self.click_through {
            self.click_through = self.window.set_cursor_hittest(false).is_ok();
        }
    }

    /// Hides the window. A hidden window costs the compositor nothing.
    pub fn hide(&mut self) {
        if self.visible {
            self.window.set_visible(false);
            self.visible = false;
            crate::trace::line(&format!("{:?} window hidden", self.edge));
        }
    }

    /// Whether the window is shown.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Asks for a redraw on the next turn of the event loop.
    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }
}

/// An OpenGL context bound to one edge window.
///
/// The order of the fields is a requirement, not a style: Rust drops fields in
/// the order they are declared, and the GLX surface and context refer to the
/// X window. If the window went first, the server would answer the later
/// release of the surface with `GLXBadWindow`, which `winit` turns into a
/// panic. So the window comes last.
pub struct GlStrip {
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    gl: glow::Context,
    program: glow::Program,
    vertex_array: glow::VertexArray,
    uniforms: Uniforms,
    /// Reads back a pixel after each draw and reports it, for diagnosing the
    /// GPU path. Off unless `SYNTRA_GLOW_PROBE` is set.
    probe: bool,
    /// Declared last on purpose: see the type's documentation.
    window: EdgeWindow,
}

impl Drop for GlStrip {
    fn drop(&mut self) {
        // The GL objects belong to this strip's context, which is not
        // necessarily the current one: make it current first, or the deletes
        // would land in another strip's context. Failure is ignored, as the
        // process may be shutting down and the server frees them anyway.
        let _ = self.context.make_current(&self.surface);
        // The GL objects must be freed while their context is current.
        unsafe {
            self.gl.use_program(None);
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vertex_array);
        }
    }
}

struct Uniforms {
    edge: Option<glow::UniformLocation>,
    kind: Option<glow::UniformLocation>,
    along: Option<glow::UniformLocation>,
    level: Option<glow::UniformLocation>,
    progress: Option<glow::UniformLocation>,
    colour: Option<glow::UniformLocation>,
    rainbow: Option<glow::UniformLocation>,
    clock: Option<glow::UniformLocation>,
}

const VERTEX_SHADER: &str = r#"
attribute vec2 a_position;
varying vec2 v_uv;
void main() {
    // A triangle strip covering the clip space; uv has its origin top-left.
    v_uv = vec2(a_position.x * 0.5 + 0.5, 0.5 - a_position.y * 0.5);
    gl_Position = vec4(a_position, 0.0, 1.0);
}
"#;

impl GlStrip {
    /// Creates the window and an OpenGL (or GLES) context for `edge`.
    ///
    /// Any failure is returned, never panicked on, so the plugin can fall
    /// back to the CPU when the GPU is unusable.
    pub fn create(
        event_loop: &ActiveEventLoop,
        edge: Edge,
        origin: (i32, i32),
        screen: (u32, u32),
        thickness: u32,
    ) -> Result<Self, RenderError> {
        let fail = |context: &str, error: &dyn std::fmt::Display| {
            RenderError(format!("{context}: {error}"))
        };
        let template = ConfigTemplateBuilder::new().with_alpha_size(8);
        let (window, config): (Option<Window>, Config) = DisplayBuilder::new()
            .with_window_attributes(Some(attributes(edge, origin, screen, thickness)))
            .build(event_loop, template, |configs| {
                configs
                    .reduce(|best, next| {
                        let better = next.supports_transparency().unwrap_or(false)
                            && !best.supports_transparency().unwrap_or(false);
                        if better { next } else { best }
                    })
                    .expect("the display offered no configuration")
            })
            .map_err(|error| fail("no usable graphics configuration", &error))?;
        let window = window.ok_or_else(|| RenderError("no window was created".into()))?;
        if !config.supports_transparency().unwrap_or(false) {
            return Err(RenderError(
                "the graphics configuration has no alpha channel".into(),
            ));
        }
        let display = config.display();
        let handle = window
            .window_handle()
            .map_err(|error| fail("no window handle", &error))?
            .as_raw();
        let not_current = unsafe {
            display
                .create_context(
                    &config,
                    &ContextAttributesBuilder::new()
                        .with_context_api(ContextApi::OpenGl(Some(Version::new(2, 1))))
                        .build(Some(handle)),
                )
                .or_else(|_| {
                    display.create_context(
                        &config,
                        &ContextAttributesBuilder::new()
                            .with_context_api(ContextApi::Gles(None))
                            .build(Some(handle)),
                    )
                })
                .map_err(|error| fail("cannot create a graphics context", &error))?
        };
        let surface_attributes = window
            .build_surface_attributes(Default::default())
            .map_err(|error| fail("cannot describe the surface", &error))?;
        let surface = unsafe {
            display
                .create_window_surface(&config, &surface_attributes)
                .map_err(|error| fail("cannot create a surface", &error))?
        };
        let context = not_current
            .make_current(&surface)
            .map_err(|error| fail("cannot use the graphics context", &error))?;
        // A hidden strip draws once per state change, not once per vsync.
        let _ = surface.set_swap_interval(&context, SwapInterval::DontWait);
        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|name| display.get_proc_address(name))
        };
        let (program, vertex_array, uniforms) = unsafe { build_program(&gl)? };
        let (_, _, width, height) = x11::geometry(edge, origin, screen, thickness);
        Ok(Self {
            window: EdgeWindow::wrap(edge, window, (width, height)),
            surface,
            context,
            gl,
            program,
            vertex_array,
            uniforms,
            probe: std::env::var_os("SYNTRA_GLOW_PROBE").is_some(),
        })
    }

    /// The window this strip draws in.
    pub fn window(&mut self) -> &mut EdgeWindow {
        &mut self.window
    }
}

/// Compiles the glow shader and a full-strip quad.
///
/// # Safety
/// `gl` must be the current context.
unsafe fn build_program(
    gl: &glow::Context,
) -> Result<(glow::Program, glow::VertexArray, Uniforms), RenderError> {
    let fail = |context: &str, log: String| RenderError(format!("{context}: {log}"));
    unsafe {
        let program = gl
            .create_program()
            .map_err(|error| fail("cannot create a program", error))?;
        let mut shaders = Vec::new();
        for (kind, source) in [
            (glow::VERTEX_SHADER, VERTEX_SHADER.to_owned()),
            (glow::FRAGMENT_SHADER, crate::paint::fragment_shader()),
        ] {
            let shader = gl
                .create_shader(kind)
                .map_err(|error| fail("cannot create a shader", error))?;
            gl.shader_source(shader, &source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(fail(
                    "the shader did not compile",
                    gl.get_shader_info_log(shader),
                ));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        gl.bind_attrib_location(program, 0, "a_position");
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(fail(
                "the shader did not link",
                gl.get_program_info_log(program),
            ));
        }
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        let location = |name: &str| gl.get_uniform_location(program, name);
        let uniforms = Uniforms {
            edge: location("u_edge"),
            kind: location("u_kind"),
            along: location("u_along"),
            level: location("u_level"),
            progress: location("u_progress"),
            colour: location("u_colour"),
            rainbow: location("u_rainbow"),
            clock: location("u_clock"),
        };
        let vertex_array = gl
            .create_vertex_array()
            .map_err(|error| fail("cannot create a vertex array", error))?;
        gl.bind_vertex_array(Some(vertex_array));
        let buffer = gl
            .create_buffer()
            .map_err(|error| fail("cannot create a buffer", error))?;
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
        let quad: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        let bytes =
            std::slice::from_raw_parts(quad.as_ptr().cast::<u8>(), std::mem::size_of_val(&quad));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
        Ok((program, vertex_array, uniforms))
    }
}

impl Draw for GlStrip {
    fn name(&self) -> &'static str {
        "OpenGL"
    }

    fn draw(&mut self, paint: &Paint, width: u32, height: u32) -> Result<(), RenderError> {
        use crate::glow::Light;
        let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return Ok(());
        };
        self.surface.resize(&self.context, w, h);
        let (kind, along, level, progress) = match paint.light {
            Light::Entry { along, age } => (
                0,
                along,
                0.0,
                (age.as_secs_f32() / crate::glow::ENTRY_DURATION.as_secs_f32()).clamp(0.0, 1.0),
            ),
            Light::Push { along, level } => (1, along, level, 0.0),
            Light::Fading {
                along,
                level,
                since,
            } => (
                2,
                along,
                level,
                (since.as_secs_f32() / crate::glow::RELEASE_DURATION.as_secs_f32()).clamp(0.0, 1.0),
            ),
        };
        // Each strip has a context of its own, and only one is current at a
        // time: after four strips were created it is the last one's. Without
        // this every GL call below landed in that other context, and the swap
        // presented this strip's buffer, which had never been drawn into.
        self.context
            .make_current(&self.surface)
            .map_err(|error| RenderError(format!("cannot use the graphics context: {error}")))?;
        // Mapped before anything is drawn into it: a frame presented to a
        // window that is not on screen is not kept for when it is.
        let was_visible = self.window.is_visible();
        self.window.show();
        let gl = &self.gl;
        unsafe {
            gl.viewport(0, 0, width as i32, height as i32);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vertex_array));
            let edge = match paint.edge {
                Edge::Left => 0,
                Edge::Right => 1,
                Edge::Top => 2,
                Edge::Bottom => 3,
            };
            gl.uniform_1_i32(self.uniforms.edge.as_ref(), edge);
            gl.uniform_1_i32(self.uniforms.kind.as_ref(), kind);
            gl.uniform_1_f32(self.uniforms.along.as_ref(), along);
            gl.uniform_1_f32(self.uniforms.level.as_ref(), level);
            gl.uniform_1_f32(self.uniforms.progress.as_ref(), progress);
            gl.uniform_3_f32(
                self.uniforms.colour.as_ref(),
                paint.colour.0,
                paint.colour.1,
                paint.colour.2,
            );
            gl.uniform_1_i32(self.uniforms.rainbow.as_ref(), i32::from(paint.rainbow));
            gl.uniform_1_f32(self.uniforms.clock.as_ref(), paint.clock);
            // The shader writes premultiplied colour, so it is composed as is.
            gl.disable(glow::BLEND);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            if self.probe {
                // Read back from this strip's own framebuffer, before it is
                // presented: whatever a compositor does later, this says
                // whether the shader and the geometry produced light.
                // Next to the screen edge and half way along it, which is where
                // the glow is brightest on every one of the four edges. Any
                // other pixel is dark by design and would read as a failure.
                // `read_pixels` counts rows from the bottom, while the shader's
                // coordinates start at the top: the top strip's screen edge is
                // the last row here, and the bottom strip's is the first.
                let (px, py) = match paint.edge {
                    Edge::Left => (1, (height / 2) as i32),
                    Edge::Right => (width as i32 - 2, (height / 2) as i32),
                    Edge::Top => ((width / 2) as i32, height as i32 - 2),
                    Edge::Bottom => ((width / 2) as i32, 1),
                };
                let mut pixel = [0_u8; 4];
                gl.read_pixels(
                    px,
                    py,
                    1,
                    1,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut pixel)),
                );
                eprintln!(
                    "syntra-plugin-edge-glow: gl probe {:?} pixel({px},{py}) rgba={pixel:?} error={:#x}",
                    paint.edge,
                    gl.get_error()
                );
            }
        }
        self.surface
            .swap_buffers(&self.context)
            .map_err(|error| RenderError(format!("cannot present: {error}")))?;
        if !was_visible {
            // A window that has only just been mapped shows nothing until the
            // next frame is presented into it, so ask for one.
            self.window.request_redraw();
        }
        Ok(())
    }

    fn clear(&mut self, _width: u32, _height: u32) -> Result<(), RenderError> {
        self.window.hide();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    /// Both X11 handle kinds name the same window id; any other kind has none,
    /// so the CPU renderer for X11 is never handed a window it cannot draw in.
    #[test]
    fn x11_ids_come_from_xcb_and_xlib_handles() {
        let xcb = raw_window_handle::XcbWindowHandle::new(NonZeroU32::new(77).unwrap());
        assert_eq!(x11_window_id(RawWindowHandle::Xcb(xcb)), Some(77));
        let xlib = raw_window_handle::XlibWindowHandle::new(88);
        assert_eq!(x11_window_id(RawWindowHandle::Xlib(xlib)), Some(88));
        let wayland = raw_window_handle::WaylandWindowHandle::new(std::ptr::NonNull::dangling());
        assert_eq!(x11_window_id(RawWindowHandle::Wayland(wayland)), None);
    }

    /// An id that does not fit an X11 window id is refused, not truncated into
    /// a different window's id.
    #[test]
    fn an_oversized_xlib_id_is_refused() {
        let xlib = raw_window_handle::XlibWindowHandle::new(u64::MAX);
        assert_eq!(x11_window_id(RawWindowHandle::Xlib(xlib)), None);
    }
}
