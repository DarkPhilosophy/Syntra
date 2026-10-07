//! The plugin's Linux event loop.

use std::{
    io::{self, BufReader},
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use syntra_plugin_api::{Edge, Message};
use syntra_plugin_edge_glow::{
    paint::Paint,
    protocol,
    render::{Candidate, Draw, RenderError, choose},
    state::{Renderer, State, Strip},
    surface::{EdgeWindow, GlStrip},
    x11,
};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    platform::x11::EventLoopBuilderExtX11,
    window::WindowId,
};

const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

/// How long between animation frames: about sixty a second, which is smooth
/// for a glow and, unlike drawing on every event, leaves the machine idle in
/// between.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// What wakes the event loop from outside.
enum Wake {
    /// The daemon sent a message.
    Message(Message),
    /// The daemon closed the pipe: the plugin has been stopped.
    Closed,
}

/// One report from the plugin: to standard error, where the daemon forwards it
/// to the dashboard's diagnostics view, and to the plugin's own log file.
fn report(text: &str) {
    eprintln!("syntra-plugin-edge-glow: {text}");
    syntra_plugin_edge_glow::trace::line(text);
}

pub fn main() {
    if std::env::args().any(|argument| argument == "--describe") {
        let mut out = io::stdout().lock();
        if protocol::send_hello(&mut out).is_err() {
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run() {
        report(&format!("{error}"));
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    // The handshake goes first: the daemon gives a plugin only a short time to
    // speak, and the graphics below can take longer than that to start.
    protocol::send_hello(&mut io::stdout().lock()).map_err(|error| error.to_string())?;

    // Windows are made on X11, through XWayland on a Wayland session: winit
    // cannot place or raise a Wayland toplevel, and GNOME has no layer-shell.
    let event_loop = EventLoop::<Wake>::with_user_event()
        .with_x11()
        .build()
        .map_err(|error| format!("cannot start the event loop: {error}"))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    spawn_reader(proxy);

    let mut app = App {
        state: State::default(),
        started: Instant::now(),
        windows: None,
        failed: None,
        next_frame: Instant::now(),
    };
    event_loop
        .run_app(&mut app)
        .map_err(|error| format!("the event loop failed: {error}"))?;
    match app.failed {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// Reads the daemon's messages on their own thread, so the drawing loop never
/// waits on stdin.
fn spawn_reader(proxy: EventLoopProxy<Wake>) {
    thread::Builder::new()
        .name("daemon-reader".into())
        .spawn(move || {
            let (tx, rx) = mpsc::channel();
            let forward = {
                let proxy = proxy.clone();
                thread::spawn(move || {
                    for message in rx {
                        if proxy.send_event(Wake::Message(message)).is_err() {
                            return;
                        }
                    }
                })
            };
            protocol::read_messages(BufReader::new(io::stdin()), &tx);
            drop(tx);
            let _ = forward.join();
            let _ = proxy.send_event(Wake::Closed);
        })
        .expect("cannot start the reader thread");
}

/// The windows and the renderer that draws into them.
struct Windows {
    strips: Vec<(Edge, Box<dyn StripDraw>)>,
    /// Which renderer is actually drawing, which may differ from the setting
    /// when the setting is `Auto`.
    renderer: Renderer,
    /// The width the windows were built with, in pixels.
    width: u32,
}

/// A window with its renderer, so the event loop need not know which kind.
trait StripDraw {
    fn draw(&mut self, paint: &Paint) -> Result<(), RenderError>;
    fn clear(&mut self) -> Result<(), RenderError>;
    /// The window this strip draws in, to match redraw requests to it.
    fn window_id(&mut self) -> WindowId;
}

struct GlWindow(GlStrip);

impl StripDraw for GlWindow {
    fn draw(&mut self, paint: &Paint) -> Result<(), RenderError> {
        let (width, height) = self.0.window().size();
        self.0.draw(paint, width, height)
    }
    fn clear(&mut self) -> Result<(), RenderError> {
        let (width, height) = self.0.window().size();
        self.0.clear(width, height)
    }
    fn window_id(&mut self) -> WindowId {
        self.0.window().id()
    }
}

/// A window drawn by the CPU. Its pixel writer shares the X connection with
/// the other three windows, so the connection lives as long as any of them.
struct CpuWindow {
    window: EdgeWindow,
    pixels: x11::Pixels,
}

impl StripDraw for CpuWindow {
    fn draw(&mut self, paint: &Paint) -> Result<(), RenderError> {
        let (width, height) = self.window.size();
        // Mapped first: pixels sent to a window that is not on screen yet are
        // not kept for when it is.
        self.window.show();
        self.pixels.draw(paint, width, height)
    }
    fn clear(&mut self) -> Result<(), RenderError> {
        self.window.hide();
        Ok(())
    }
    fn window_id(&mut self) -> WindowId {
        self.window.id()
    }
}

struct App {
    state: State,
    started: Instant,
    windows: Option<Windows>,
    failed: Option<String>,
    /// When the next animation frame is due. Draws before that are skipped.
    next_frame: Instant,
}

impl App {
    /// The monitor the glow lies along: the primary one, else the first.
    fn monitor(event_loop: &ActiveEventLoop) -> Result<((i32, i32), (u32, u32)), String> {
        let monitor = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
            .ok_or("no monitor is connected")?;
        let position = monitor.position();
        let size = monitor.size();
        Ok(((position.x, position.y), (size.width, size.height)))
    }

    /// Whether the windows in use already match what the settings ask for:
    /// the renderer and the width they were built with.
    ///
    /// `Auto` accepts whatever renderer was chosen, so a setting that only
    /// changes between `Auto` and the renderer `Auto` already picked rebuilds
    /// nothing. The width is fixed when a window is created, so a different
    /// width needs new windows.
    fn renderer_satisfied(&self) -> bool {
        let Some(windows) = &self.windows else {
            return false;
        };
        let renderer_ok = match self.state.settings.renderer {
            Renderer::Auto => true,
            wanted => windows.renderer == wanted,
        };
        renderer_ok && windows.width == self.state.settings.width
    }

    /// Creates the four windows with the renderer the settings ask for.
    fn open(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let (origin, screen) = Self::monitor(event_loop)?;
        let thickness = self.state.settings.width;
        let wanted = self.state.settings.renderer;

        // Each candidate is given the event loop when it is tried; none holds
        // on to it, and the CPU one shares a single X connection among its
        // four windows, which closes when the last of them is dropped.
        let mut candidates: Vec<Candidate<ActiveEventLoop, Vec<(Edge, Box<dyn StripDraw>)>>> = vec![
            Candidate {
                kind: Renderer::OpenGl,
                make: Box::new(move |event_loop| {
                    let mut strips: Vec<(Edge, Box<dyn StripDraw>)> = Vec::new();
                    for edge in EDGES {
                        let strip = GlStrip::create(event_loop, edge, origin, screen, thickness)?;
                        strips.push((edge, Box::new(GlWindow(strip))));
                    }
                    Ok(strips)
                }),
            },
            Candidate {
                kind: Renderer::Software,
                make: Box::new(move |event_loop| {
                    let display = Rc::new(x11::Display::open()?);
                    let mut strips: Vec<(Edge, Box<dyn StripDraw>)> = Vec::new();
                    for edge in EDGES {
                        let window =
                            EdgeWindow::plain(event_loop, edge, origin, screen, thickness)?;
                        let id = window
                            .x11_id()
                            .ok_or_else(|| RenderError("the window is not an X11 window".into()))?;
                        let pixels = x11::Pixels::new(&display, id)?;
                        strips.push((edge, Box::new(CpuWindow { window, pixels })));
                    }
                    Ok(strips)
                }),
            },
        ];
        let (strips, used) =
            choose(wanted, event_loop, &mut candidates).map_err(|error| error.to_string())?;
        for (renderer, why) in &used.skipped {
            report(&format!("{renderer:?} was not used: {why}"));
        }
        report(&format!("drawing with {:?}", used.used));
        // Only now, with the new windows built, are the old ones dropped.
        self.windows = Some(Windows {
            strips,
            renderer: used.used,
            width: thickness,
        });
        Ok(())
    }

    /// Draws what is lit and hides what is not.
    fn redraw(&mut self) {
        // The next frame is due a fixed interval after this one, whatever
        // else wakes the loop in between.
        self.next_frame = Instant::now() + FRAME_INTERVAL;
        let Some(windows) = &mut self.windows else {
            return;
        };
        let lit: Vec<Strip> = self.state.frame(Instant::now());
        let clock = self.started.elapsed().as_secs_f32();
        for (edge, strip) in &mut windows.strips {
            let result = match lit.iter().find(|candidate| candidate.edge == *edge) {
                Some(found) => strip.draw(&Paint {
                    edge: *edge,
                    light: found.light,
                    colour: self.state.settings.colour,
                    rainbow: self.state.settings.rainbow,
                    clock,
                }),
                None => strip.clear(),
            };
            if let Err(error) = result {
                report(&format!("cannot draw the {edge:?} edge: {error}"));
            }
        }
    }

    /// Draws the one strip whose window asked to be redrawn.
    fn redraw_window(&mut self, window: WindowId) {
        let Some(windows) = &mut self.windows else {
            return;
        };
        let lit: Vec<Strip> = self.state.frame(Instant::now());
        let clock = self.started.elapsed().as_secs_f32();
        for (edge, strip) in &mut windows.strips {
            if strip.window_id() != window {
                continue;
            }
            // A strip with nothing lit stays hidden: a redraw request for a
            // window being hidden is not a reason to show it.
            if let Some(found) = lit.iter().find(|candidate| candidate.edge == *edge) {
                let result = strip.draw(&Paint {
                    edge: *edge,
                    light: found.light,
                    colour: self.state.settings.colour,
                    rainbow: self.state.settings.rainbow,
                    clock,
                });
                if let Err(error) = result {
                    report(&format!("cannot draw the {edge:?} edge: {error}"));
                }
            }
        }
    }

    /// Sleeps until the next frame is due while something is lit, and until
    /// the next message otherwise.
    fn schedule(&self, event_loop: &ActiveEventLoop) {
        if self.state.next_wake().is_some() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.windows.is_some() {
            return;
        }
        if let Err(reason) = self.open(event_loop) {
            self.failed = Some(reason);
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        match event {
            Wake::Message(message) => {
                // One line per message the daemon sends. They are per crossing
                // and per settings change, never per pointer motion, so this
                // stays quiet, and it is what shows in the daemon's log that
                // an event reached the plugin at all.
                match &message {
                    Message::Pointer(event) => report(&format!("received {event:?}")),
                    Message::Settings { values } => {
                        report(&format!("received {} setting(s)", values.len()))
                    }
                    _ => {}
                }
                self.state.handle(message, Instant::now());
                if !self.renderer_satisfied() {
                    // The user asked for a different renderer than the one in
                    // use. A failure here is reported and the windows already
                    // showing are kept: an unusable choice must not take the
                    // whole plugin down, nor leave the screen without a glow.
                    if let Err(reason) = self.open(event_loop) {
                        report(&format!("keeping the current renderer: {reason}"));
                    }
                }
                // A message is a change worth showing at once, then the clock
                // starts again from here.
                self.redraw();
                self.schedule(event_loop);
            }
            Wake::Closed => event_loop.exit(),
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, window: WindowId, event: WindowEvent) {
        // A window the server asks to be redrawn (it has just been mapped, or
        // something above it moved) is drawn again, and only that window, once.
        // It does not ask for another frame, so this cannot feed on itself;
        // while something is animating, `about_to_wait` is what drives it.
        if matches!(event, WindowEvent::RedrawRequested) {
            self.redraw_window(window);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // winit calls this after every batch of events, not only when a timer
        // expires, and each frame produces events of its own. Drawing here
        // unconditionally made every frame start the next, thousands a second.
        // So a frame is drawn only once its time has come.
        if self.state.next_wake().is_some() && Instant::now() >= self.next_frame {
            self.redraw();
        }
        self.schedule(event_loop);
    }
}
