//! Probe: does GNOME give a transparent, click-through OpenGL window?
//!
//! Draws a green strip with a soft edge in a small borderless window in the
//! top-left corner and reports on stdout what the compositor did with it.
//! It stays small on purpose: if transparency does not work it is an opaque
//! patch of a few hundred pixels, not the whole screen. It ends by itself
//! after `LIFETIME` and prints its process id so it can be stopped at once.
//!
//! `cargo run -p syntra-plugin-edge-glow --bin probe`

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the edge glow probe is Linux-only");
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        num::NonZeroU32,
        time::{Duration, Instant},
    };

    use glow::HasContext;
    use glutin::{
        config::{ConfigTemplateBuilder, GlConfig},
        context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext, Version},
        display::{GetGlDisplay, GlDisplay},
        prelude::GlSurface,
        surface::{SwapInterval, WindowSurface},
    };
    use glutin_winit::{DisplayBuilder, GlWindow};
    use raw_window_handle::HasWindowHandle;
    use winit::{
        application::ApplicationHandler,
        dpi::LogicalSize,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
        window::{Window, WindowAttributes, WindowId},
    };

    /// How long the probe stays up, so it cannot be left behind.
    const LIFETIME: Duration = Duration::from_secs(40);

    struct Probe {
        started: Instant,
        window: Option<Window>,
        gl_surface: Option<glutin::surface::Surface<WindowSurface>>,
        gl_context: Option<glutin::context::PossiblyCurrentContext>,
        gl: Option<glow::Context>,
        template: ConfigTemplateBuilder,
        config: Option<glutin::config::Config>,
        click_through_set: bool,
        x11: bool,
    }

    pub fn run() {
        println!(
            "edge glow probe, pid {}: runs for {LIFETIME:?}",
            std::process::id()
        );
        let mut builder = EventLoop::builder();
        if std::env::args().any(|argument| argument == "x11") {
            // Force the X11 backend (through XWayland on a Wayland session):
            // winit cannot position or raise a Wayland toplevel.
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        }
        let event_loop = builder.build().expect("event loop");
        event_loop.set_control_flow(ControlFlow::Poll);
        let mut probe = Probe {
            started: Instant::now(),
            window: None,
            gl_surface: None,
            gl_context: None,
            gl: None,
            // Ask for an alpha channel: without it the strip cannot be clear.
            template: ConfigTemplateBuilder::new().with_alpha_size(8),
            config: None,
            click_through_set: false,
            x11: std::env::args().any(|argument| argument == "x11"),
        };
        event_loop.run_app(&mut probe).expect("run");
    }

    impl ApplicationHandler for Probe {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let mut attributes: WindowAttributes = Window::default_attributes()
                .with_title("syntra edge glow probe")
                .with_decorations(false)
                .with_transparent(true)
                .with_resizable(false)
                .with_inner_size(LogicalSize::new(360.0, 240.0));
            if self.x11 {
                use winit::platform::x11::{WindowAttributesExtX11, WindowType};
                // An edge strip: narrow, as tall as the screen, flush left,
                // above ordinary windows, typed as a notification so the
                // window manager neither decorates nor focuses it.
                let screen = event_loop
                    .primary_monitor()
                    .or_else(|| event_loop.available_monitors().next())
                    .expect("no monitor");
                let size = screen.size();
                let position = screen.position();
                println!(
                    "monitor: {}x{} at ({}, {})",
                    size.width, size.height, position.x, position.y
                );
                attributes = attributes
                    .with_inner_size(winit::dpi::PhysicalSize::new(48, size.height))
                    .with_position(winit::dpi::PhysicalPosition::new(position.x, position.y))
                    .with_window_level(winit::window::WindowLevel::AlwaysOnTop)
                    .with_x11_window_type(vec![WindowType::Notification]);
            }
            let builder = DisplayBuilder::new().with_window_attributes(Some(attributes));
            let (window, config) = builder
                .build(event_loop, self.template.clone(), |configs| {
                    // The configuration with the most transparency bits.
                    configs
                        .reduce(|best, next| {
                            if next.supports_transparency().unwrap_or(false)
                                && !best.supports_transparency().unwrap_or(false)
                            {
                                next
                            } else {
                                best
                            }
                        })
                        .expect("no EGL configuration")
                })
                .expect("display");
            let window = window.expect("window");
            println!(
                "config: alpha_size={} transparency={:?}",
                config.alpha_size(),
                config.supports_transparency()
            );
            let display = config.display();
            let handle = window.window_handle().expect("handle").as_raw();
            let context = unsafe {
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
                    .expect("context")
            };
            let attrs = window
                .build_surface_attributes(Default::default())
                .expect("surface attributes");
            let surface = unsafe {
                display
                    .create_window_surface(&config, &attrs)
                    .expect("surface")
            };
            let context = context.make_current(&surface).expect("make current");
            let _ = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::MIN));
            let gl = unsafe {
                glow::Context::from_loader_function_cstr(|name| display.get_proc_address(name))
            };
            println!("gl: {}", unsafe { gl.get_parameter_string(glow::VERSION) });
            self.config = Some(config);
            self.gl = Some(gl);
            self.gl_surface = Some(surface);
            self.gl_context = Some(context);
            // What the window manager actually did, not what was asked for.
            println!(
                "window: backend={} inner={:?} outer_position={:?} decorated={}",
                if self.x11 { "x11" } else { "default" },
                window.inner_size(),
                window.outer_position(),
                window.is_decorated(),
            );
            self.window = Some(window);
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::Focused(focused) => println!("focus changed: focused={focused}"),
                WindowEvent::RedrawRequested => self.paint(),
                _ => {}
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self.started.elapsed() > LIFETIME {
                println!("probe time is up");
                event_loop.exit();
                return;
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    impl Probe {
        fn paint(&mut self) {
            let (Some(gl), Some(surface), Some(context), Some(window)) =
                (&self.gl, &self.gl_surface, &self.gl_context, &self.window)
            else {
                return;
            };
            let size = window.inner_size();
            if let (Some(width), Some(height)) =
                (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
            {
                surface.resize(context, width, height);
            }
            unsafe {
                gl.viewport(0, 0, size.width as i32, size.height as i32);
                // Fully clear: whatever is behind the window must show through.
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
                // A premultiplied green strip down the left side, fading right.
                gl.enable(glow::SCISSOR_TEST);
                // In the narrow X11 strip the whole width fades; in the small
                // test window only the left sixth does.
                let strip = if self.x11 {
                    size.width.max(1)
                } else {
                    (size.width / 6).max(1)
                };
                for step in 0..strip {
                    let alpha = 1.0 - step as f32 / strip as f32;
                    gl.scissor(step as i32, 0, 1, size.height as i32);
                    gl.clear_color(0.0, alpha, 0.0, alpha);
                    gl.clear(glow::COLOR_BUFFER_BIT);
                }
                gl.disable(glow::SCISSOR_TEST);
            }
            let _ = surface.swap_buffers(context);
            // After the first frame is on screen, so the window exists first:
            // an empty input region sends every click to what is underneath.
            if !self.click_through_set {
                self.click_through_set = true;
                println!(
                    "click-through requested: {:?}",
                    window.set_cursor_hittest(false)
                );
            }
        }
    }
}
