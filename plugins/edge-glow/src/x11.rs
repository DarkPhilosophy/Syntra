//! The CPU renderer for X11.
//!
//! The window is made by the surface code (`winit`), the same one the GPU
//! renderer draws into, so placement, stacking order, click-through and
//! showing or hiding exist once. This module only sends pixels into a window
//! it is handed: `put_image` of a premultiplied ARGB buffer, which a window
//! created with transparency accepts because its visual is 32 bits deep.
//!
//! On a Wayland session this runs through XWayland, which is how the glow
//! gets a position and a stacking order on GNOME, where no layer-shell exists.

use std::rc::Rc;

use x11rb::{
    connection::Connection,
    protocol::xproto::{self, ConnectionExt as _, CreateGCAux, ImageFormat},
    rust_connection::RustConnection,
};

use crate::{
    paint::{self, Paint},
    render::{Draw, RenderError},
};
use syntra_plugin_api::Edge;

fn fail(context: &str, error: impl std::fmt::Display) -> RenderError {
    RenderError(format!("{context}: {error}"))
}

/// Where a strip lies and how big it is, for an edge of a screen whose
/// top-left corner is at `origin`.
pub fn geometry(
    edge: Edge,
    origin: (i32, i32),
    screen: (u32, u32),
    thickness: u32,
) -> (i32, i32, u32, u32) {
    let (width, height) = screen;
    let (x0, y0) = origin;
    let thick_w = thickness.min(width).max(1);
    let thick_h = thickness.min(height).max(1);
    let end = |start: i32, length: u32, thick: u32| {
        start.saturating_add(length.saturating_sub(thick).min(i32::MAX as u32) as i32)
    };
    match edge {
        Edge::Left => (x0, y0, thick_w, height),
        Edge::Right => (end(x0, width, thick_w), y0, thick_w, height),
        Edge::Top => (x0, y0, width, thick_h),
        Edge::Bottom => (x0, end(y0, height, thick_h), width, thick_h),
    }
}

/// A connection to the X server, shared by every strip.
pub struct Display {
    connection: RustConnection,
}

impl Display {
    /// Connects to the X server.
    pub fn open() -> Result<Self, RenderError> {
        let (connection, _screen) =
            x11rb::connect(None).map_err(|error| fail("cannot reach the X server", error))?;
        Ok(Self { connection })
    }

    /// The depth of `window`, which must be 32 for per-pixel transparency.
    ///
    /// Checked rather than assumed: a window with an opaque visual would take
    /// the pixels and show them on a black rectangle.
    pub fn depth_of(&self, window: u32) -> Result<u8, RenderError> {
        self.connection
            .get_geometry(window)
            .map_err(|error| fail("cannot ask about the window", error))?
            .reply()
            .map(|reply| reply.depth)
            .map_err(|error| fail("the window is not known to the X server", error))
    }
}

/// Sends pixels into one window.
pub struct Pixels {
    display: Rc<Display>,
    window: xproto::Window,
    gc: xproto::Gcontext,
    buffer: Vec<u8>,
}

impl Pixels {
    /// Prepares to draw into `window`, which must have a 32-bit visual.
    ///
    /// Takes a share of the connection, so the connection lives exactly as
    /// long as the last strip using it and is closed when that is dropped.
    pub fn new(display: &Rc<Display>, window: xproto::Window) -> Result<Self, RenderError> {
        let depth = display.depth_of(window)?;
        if depth != 32 {
            return Err(RenderError(format!(
                "the window is {depth} bits deep; a glow needs a 32-bit ARGB window"
            )));
        }
        let gc = display
            .connection
            .generate_id()
            .map_err(|error| fail("cannot allocate a context id", error))?;
        display
            .connection
            .create_gc(gc, window, &CreateGCAux::new())
            .map_err(|error| fail("cannot create a graphics context", error))?
            .check()
            .map_err(|error| fail("the graphics context was refused", error))?;
        Ok(Self {
            display: Rc::clone(display),
            window,
            gc,
            buffer: Vec::new(),
        })
    }

    fn put(&self, width: u32, height: u32) -> Result<(), RenderError> {
        // `put_image` is limited by the server's maximum request size, so a
        // tall strip is sent in bands that each fit well under it.
        let row = width as usize * 4;
        let rows_per_band = (65_536 / row.max(1)).clamp(1, height as usize);
        let mut y = 0usize;
        while y < height as usize {
            let rows = rows_per_band.min(height as usize - y);
            self.display
                .connection
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    self.window,
                    self.gc,
                    width.min(u32::from(u16::MAX)) as u16,
                    rows as u16,
                    0,
                    y.min(i16::MAX as usize) as i16,
                    0,
                    32,
                    &self.buffer[y * row..(y + rows) * row],
                )
                .map_err(|error| fail("cannot send pixels", error))?;
            y += rows;
        }
        self.display
            .connection
            .flush()
            .map_err(|error| fail("cannot flush", error))
    }
}

impl Draw for Pixels {
    fn name(&self) -> &'static str {
        "software (X11)"
    }

    fn draw(&mut self, paint: &Paint, width: u32, height: u32) -> Result<(), RenderError> {
        self.buffer.resize(width as usize * height as usize * 4, 0);
        paint::fill_argb8888(paint, width, height, &mut self.buffer);
        self.put(width, height)
    }

    fn clear(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        self.buffer.clear();
        self.buffer.resize(width as usize * height as usize * 4, 0);
        self.put(width, height)
    }
}

impl Drop for Pixels {
    fn drop(&mut self) {
        let _ = self.display.connection.free_gc(self.gc);
        let _ = self.display.connection.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each strip must lie flush against its own edge and span that edge, or
    /// the glow would float in the middle of the screen.
    #[test]
    fn strips_lie_flush_along_their_edges() {
        let screen = (3840, 2160);
        assert_eq!(geometry(Edge::Left, (0, 0), screen, 56), (0, 0, 56, 2160));
        assert_eq!(
            geometry(Edge::Right, (0, 0), screen, 56),
            (3784, 0, 56, 2160)
        );
        assert_eq!(geometry(Edge::Top, (0, 0), screen, 56), (0, 0, 3840, 56));
        assert_eq!(
            geometry(Edge::Bottom, (0, 0), screen, 56),
            (0, 2104, 3840, 56)
        );
    }

    /// A second monitor sits at an offset, and its strips follow it.
    #[test]
    fn a_monitor_not_at_the_origin_moves_its_strips() {
        assert_eq!(
            geometry(Edge::Right, (1920, 100), (1280, 1024), 40),
            (1920 + 1240, 100, 40, 1024)
        );
        assert_eq!(
            geometry(Edge::Bottom, (-1280, 0), (1280, 1024), 40),
            (-1280, 984, 1280, 40)
        );
    }

    /// A thickness larger than the screen, or zero, never produces a size the
    /// server would reject or a strip that starts before the monitor.
    #[test]
    fn thickness_is_kept_inside_the_screen() {
        let (_, _, width, _) = geometry(Edge::Left, (0, 0), (100, 100), 9999);
        assert_eq!(width, 100);
        let (_, _, width, _) = geometry(Edge::Left, (0, 0), (100, 100), 0);
        assert_eq!(width, 1, "never a zero-size window");
        let (x, ..) = geometry(Edge::Right, (0, 0), (100, 100), 9999);
        assert_eq!(x, 0, "starts at the monitor's own edge, not before it");
    }

    /// Coordinates that would overflow are saturated, not wrapped.
    #[test]
    fn extreme_positions_do_not_overflow() {
        let (x, ..) = geometry(Edge::Right, (i32::MAX, 0), (u32::MAX, 10), 5);
        assert_eq!(x, i32::MAX);
    }
}
