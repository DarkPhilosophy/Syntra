//! Native desktop file-drop integration for the Slint window.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;

use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use slint::{ComponentHandle, LogicalPosition, Timer, TimerMode};

use crate::app::AppWindow;

const HOVER_POLL_INTERVAL: Duration = Duration::from_millis(75);
const MAX_HOVER_POLLS: u16 = 1_200;

static BACKEND_CONFIGURATION: LazyLock<BackendConfiguration> =
    LazyLock::new(configure_backend_once);

/// A native file-drag update for the application window.
#[derive(Debug, Clone)]
pub enum FileDropEvent {
    /// A drag is over the window. `None` means that it left or was cancelled.
    Hover(Option<LogicalPosition>),
    /// One file was dropped at the current pointer position.
    Drop {
        path: PathBuf,
        position: LogicalPosition,
    },
    /// Native file-drop support or position lookup failed.
    Error(String),
}

#[derive(Clone)]
enum BackendConfiguration {
    Configured,
    Skipped(String),
    Failed(String),
}

/// Configures the Winit backend before the application window is created.
///
/// Native file drop is an enhancement, not a prerequisite: if the backend
/// cannot be configured the dashboard must still open. Returning an error
/// here previously aborted start-up, so a broken XWayland cookie produced a
/// process that exited silently with status 0 and no window.
///
/// This deliberately does not modify the process environment, so the
/// daemon's input backends keep their independently selected session.
pub fn configure_backend() {
    match &*BACKEND_CONFIGURATION {
        BackendConfiguration::Configured => {}
        BackendConfiguration::Skipped(reason) => {
            log::info!("{reason}");
        }
        BackendConfiguration::Failed(error) => {
            log::warn!("{error}; continuing without native file drop");
        }
    }
}

fn configure_backend_once() -> BackendConfiguration {
    if let Some(backend) = explicit_non_winit_backend() {
        return BackendConfiguration::Skipped(format!(
            "native file drop is unavailable with explicitly selected Slint backend `{backend}`"
        ));
    }

    #[cfg(target_os = "linux")]
    {
        use winit::platform::x11::EventLoopBuilderExtX11;

        // Under Wayland the compositor delivers drops natively. Forcing the
        // event loop onto X11 there would route the whole UI through
        // XWayland, and fail outright when XWayland is unavailable or its
        // authority cookie is stale.
        // winit 0.30 exposes file-drop event variants, but its Wayland backend
        // does not bind the data-device protocol, so those events never arrive.
        // Keep the dashboard on Wayland rather than forcing the whole UI through
        // XWayland, and tell users how to transfer a file instead.
        if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()) {
            return BackendConfiguration::Skipped(
                "native file drop is unavailable on Wayland: winit 0.30 does not implement the Wayland data-device protocol; use your file manager's Copy action instead".into(),
            );
        }

        let display_is_usable = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
        if !display_is_usable {
            return BackendConfiguration::Skipped(
                "native file drop requires an X11 or XWayland DISPLAY on Linux".into(),
            );
        }

        let mut event_loop =
            winit::event_loop::EventLoop::<slint::winit_030::SlintEvent>::with_user_event();
        event_loop.with_x11();
        select_winit_backend(event_loop)
    }

    #[cfg(not(target_os = "linux"))]
    select_winit_backend(
        winit::event_loop::EventLoop::<slint::winit_030::SlintEvent>::with_user_event(),
    )
}

fn select_winit_backend(event_loop: slint::winit_030::EventLoopBuilder) -> BackendConfiguration {
    match slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_event_loop_builder(event_loop)
        .select()
    {
        Ok(()) => BackendConfiguration::Configured,
        Err(error) => BackendConfiguration::Failed(format!(
            "failed to select the Slint Winit backend for native file drop: {error}"
        )),
    }
}

fn explicit_non_winit_backend() -> Option<String> {
    let value = std::env::var("SLINT_BACKEND").ok()?;
    let normalized = value.trim().to_ascii_lowercase();
    let backend = normalized.split('-').next().unwrap_or_default();
    matches!(backend, "qt" | "linuxkms" | "headless" | "android-activity").then_some(value)
}

/// Returns whether this application window is backed by a supported native
/// Winit window.
pub fn supported(app: &AppWindow) -> bool {
    if !matches!(&*BACKEND_CONFIGURATION, BackendConfiguration::Configured) {
        return false;
    }
    app.window()
        .with_winit_window(|window| PositionSource::new(window).is_ok())
        .unwrap_or(false)
}

type DropCallback = Rc<RefCell<Box<dyn FnMut(FileDropEvent)>>>;

/// Keeps the installed file-drop callback and bounded hover polling alive.
/// Dropping the guard immediately disables callback delivery and clears hover.
pub struct FileDropGuard {
    active: Rc<Cell<bool>>,
    hovering: Rc<Cell<bool>>,
    callback: DropCallback,
    hover_timer: Rc<Timer>,
}

impl Drop for FileDropGuard {
    fn drop(&mut self) {
        self.active.set(false);
        self.hover_timer.stop();
        if self.hovering.replace(false) {
            self.callback.borrow_mut()(FileDropEvent::Hover(None));
        }
    }
}

/// Installs the Winit file-drop event filter for `app`.
///
/// The returned guard must be retained for as long as events should be
/// delivered. This is the sole Winit filter installed by the application;
/// callers must not install a second filter because Slint's API replaces the
/// previous one rather than chaining it.
pub fn install(app: &AppWindow, callback: impl FnMut(FileDropEvent) + 'static) -> FileDropGuard {
    let active = Rc::new(Cell::new(true));
    let hovering = Rc::new(Cell::new(false));
    let polls_remaining = Rc::new(Cell::new(0));
    let callback: DropCallback = Rc::new(RefCell::new(Box::new(callback)));
    let hover_timer = Rc::new(Timer::default());
    let source = app.window().with_winit_window(PositionSource::new);

    let source = match source {
        Some(Ok(source)) => Some(Rc::new(source)),
        Some(Err(error)) => {
            callback.borrow_mut()(FileDropEvent::Error(error));
            None
        }
        None => {
            let reason = match &*BACKEND_CONFIGURATION {
                BackendConfiguration::Skipped(reason) | BackendConfiguration::Failed(reason) => {
                    reason.clone()
                }
                BackendConfiguration::Configured => {
                    "native file drop requires a Slint Winit window".into()
                }
            };
            callback.borrow_mut()(FileDropEvent::Error(reason));
            None
        }
    };

    if let Some(source) = source {
        let callback_for_events = Rc::clone(&callback);
        let active_for_events = Rc::clone(&active);
        let hovering_for_events = Rc::clone(&hovering);
        let polls_for_events = Rc::clone(&polls_remaining);
        let timer_for_events = Rc::clone(&hover_timer);
        let scale_factor = Rc::new(Cell::new(f64::from(app.window().scale_factor())));
        let scale_for_events = Rc::clone(&scale_factor);

        app.window().on_winit_window_event(move |window, event| {
            if !active_for_events.get() {
                return EventResult::Propagate;
            }

            scale_for_events.set(match event {
                winit::event::WindowEvent::ScaleFactorChanged { scale_factor, .. } => *scale_factor,
                _ => f64::from(window.scale_factor()),
            });
            match event {
                winit::event::WindowEvent::HoveredFile(_) => {
                    let was_hovering = hovering_for_events.replace(true);
                    polls_for_events.set(MAX_HOVER_POLLS);
                    emit_position(&source, scale_for_events.get(), &callback_for_events);

                    if !was_hovering {
                        let source_for_timer = Rc::clone(&source);
                        let callback_for_timer = Rc::clone(&callback_for_events);
                        let active_for_timer = Rc::clone(&active_for_events);
                        let hovering_for_timer = Rc::clone(&hovering_for_events);
                        let polls_for_timer = Rc::clone(&polls_for_events);
                        let scale_for_timer = Rc::clone(&scale_for_events);
                        let weak_timer = Rc::downgrade(&timer_for_events);
                        timer_for_events.start(
                            TimerMode::Repeated,
                            HOVER_POLL_INTERVAL,
                            move || {
                                let Some(timer) = weak_timer.upgrade() else {
                                    return;
                                };
                                if !active_for_timer.get() || !hovering_for_timer.get() {
                                    timer.stop();
                                    return;
                                }
                                let remaining = polls_for_timer.get();
                                if remaining <= 1 {
                                    polls_for_timer.set(0);
                                    hovering_for_timer.set(false);
                                    timer.stop();
                                    callback_for_timer.borrow_mut()(FileDropEvent::Hover(None));
                                    return;
                                }
                                polls_for_timer.set(remaining - 1);
                                emit_position(
                                    &source_for_timer,
                                    scale_for_timer.get(),
                                    &callback_for_timer,
                                );
                            },
                        );
                    }
                }
                winit::event::WindowEvent::HoveredFileCancelled => {
                    timer_for_events.stop();
                    polls_for_events.set(0);
                    if hovering_for_events.replace(false) {
                        callback_for_events.borrow_mut()(FileDropEvent::Hover(None));
                    }
                }
                winit::event::WindowEvent::DroppedFile(path) => {
                    timer_for_events.stop();
                    polls_for_events.set(0);
                    if hovering_for_events.replace(false) {
                        callback_for_events.borrow_mut()(FileDropEvent::Hover(None));
                    }
                    match source.position(scale_for_events.get()) {
                        Ok(position) => callback_for_events.borrow_mut()(FileDropEvent::Drop {
                            path: path.clone(),
                            position,
                        }),
                        Err(error) => callback_for_events.borrow_mut()(FileDropEvent::Error(error)),
                    }
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }

    FileDropGuard {
        active,
        hovering,
        callback,
        hover_timer,
    }
}

fn emit_position(source: &PositionSource, scale_factor: f64, callback: &DropCallback) {
    match source.position(scale_factor) {
        Ok(position) => callback.borrow_mut()(FileDropEvent::Hover(Some(position))),
        Err(error) => callback.borrow_mut()(FileDropEvent::Error(error)),
    }
}

#[cfg(target_os = "linux")]
struct PositionSource {
    connection: x11rb::rust_connection::RustConnection,
    window: u32,
}

#[cfg(target_os = "linux")]
impl PositionSource {
    fn new(window: &winit::window::Window) -> Result<Self, String> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let window_handle = window
            .window_handle()
            .map_err(|error| format!("cannot access the X11 window handle: {error}"))?;
        let native_window = match window_handle.as_raw() {
            RawWindowHandle::Xlib(handle) => u32::try_from(handle.window)
                .map_err(|_| "X11 window identifier does not fit in 32 bits".to_string())?,
            RawWindowHandle::Xcb(handle) => handle.window.get(),
            RawWindowHandle::Wayland(_) => {
                // Not a defect here: winit 0.30 declares DroppedFile and
                // HoveredFile but its Wayland backend binds no data device,
                // so no drop ever arrives. Say what to do instead rather
                // than reporting a bare failure.
                return Err(
                    "Dragging files onto the window is not supported on Wayland. \
                     Copy the files in your file manager instead, and they will \
                     be offered to the selected device."
                        .into(),
                );
            }
            other => {
                return Err(format!(
                    "unsupported Linux window handle for file drop: {other:?}"
                ));
            }
        };
        let (connection, _) = x11rb::connect(None)
            .map_err(|error| format!("cannot connect to X11 for file-drop coordinates: {error}"))?;
        Ok(Self {
            connection,
            window: native_window,
        })
    }

    fn position(&self, scale_factor: f64) -> Result<LogicalPosition, String> {
        use x11rb::protocol::xproto::ConnectionExt;

        let reply = self
            .connection
            .query_pointer(self.window)
            .map_err(|error| format!("cannot query the X11 drop pointer: {error}"))?
            .reply()
            .map_err(|error| format!("cannot read the X11 drop pointer: {error}"))?;
        Ok(LogicalPosition::new(
            reply.win_x as f32 / scale_factor as f32,
            reply.win_y as f32 / scale_factor as f32,
        ))
    }
}

#[cfg(target_os = "windows")]
struct PositionSource {
    hwnd: windows::Win32::Foundation::HWND,
}

#[cfg(target_os = "windows")]
impl PositionSource {
    fn new(window: &winit::window::Window) -> Result<Self, String> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let handle = window
            .window_handle()
            .map_err(|error| format!("cannot access the Win32 window handle: {error}"))?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return Err("native file drop requires a Win32 window".into());
        };
        Ok(Self {
            hwnd: windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _),
        })
    }

    fn position(&self, scale_factor: f64) -> Result<LogicalPosition, String> {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::ScreenToClient;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }
            .map_err(|error| format!("cannot query the Windows drop pointer: {error}"))?;
        if !unsafe { ScreenToClient(self.hwnd, &mut point) }.as_bool() {
            return Err(format!(
                "cannot convert the Windows drop pointer to client coordinates: {}",
                windows::core::Error::from_win32()
            ));
        }
        Ok(LogicalPosition::new(
            point.x as f32 / scale_factor as f32,
            point.y as f32 / scale_factor as f32,
        ))
    }
}

#[cfg(target_os = "macos")]
struct PositionSource {
    view: objc2::rc::Retained<objc2_app_kit::NSView>,
}

#[cfg(target_os = "macos")]
impl PositionSource {
    fn new(window: &winit::window::Window) -> Result<Self, String> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let handle = window
            .window_handle()
            .map_err(|error| format!("cannot access the AppKit window handle: {error}"))?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return Err("native file drop requires an AppKit window".into());
        };
        let view = unsafe { objc2::rc::Retained::retain(handle.ns_view.as_ptr().cast()) }
            .ok_or_else(|| "cannot retain the AppKit content view".to_string())?;
        Ok(Self { view })
    }

    fn position(&self, _scale_factor: f64) -> Result<LogicalPosition, String> {
        let window = self
            .view
            .window()
            .ok_or_else(|| "AppKit content view is not attached to a window".to_string())?;
        let screen_point = objc2_app_kit::NSEvent::mouseLocation();
        let window_point = window.convertPointFromScreen(screen_point);
        let point = self.view.convertPoint_fromView(window_point, None);
        let y = if self.view.isFlipped() {
            point.y
        } else {
            self.view.bounds().size.height - point.y
        };
        Ok(LogicalPosition::new(point.x as f32, y as f32))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
struct PositionSource;

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
impl PositionSource {
    fn new(_window: &winit::window::Window) -> Result<Self, String> {
        Err(format!(
            "native file drop is unsupported on {}",
            std::env::consts::OS
        ))
    }

    fn position(&self, _scale_factor: f64) -> Result<LogicalPosition, String> {
        Err(format!(
            "native file drop is unsupported on {}",
            std::env::consts::OS
        ))
    }
}
