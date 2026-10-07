//! Pen input for the Slint window on Wayland.
//!
//! A compositor delivers a graphics-tablet tool only to clients that bind the
//! tablet protocol; Slint's winit backend does not, so a pen (including the
//! pointer another Syntra device drives on this desktop) could move over the
//! dashboard but never click it. This module binds the protocol on the
//! window's own connection and turns the tool's events into the pointer
//! events Slint already understands. It touches neither the shared pointer
//! nor any setting.

use slint::platform::{PointerEventButton, WindowEvent};

/// What the tool did, reduced to what a pointer needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Tool {
    /// In proximity over the window at a surface-local position.
    Hover { x: f32, y: f32 },
    /// The tip, or a barrel button, went down or up.
    Button {
        pressed: bool,
        button: PointerEventButton,
    },
    /// Left the window.
    Left,
}

/// Linux evdev codes the tablet protocol reports for the tool buttons.
const BTN_STYLUS: u32 = 0x14b;
const BTN_STYLUS2: u32 = 0x14c;

/// Barrel buttons as pointer buttons. GNOME delivers `BTN_STYLUS` as the
/// secondary click and `BTN_STYLUS2` as the middle one (the same order the
/// emulation uses), so the window follows it.
pub(crate) fn barrel_button(code: u32) -> Option<PointerEventButton> {
    match code {
        BTN_STYLUS => Some(PointerEventButton::Right),
        BTN_STYLUS2 => Some(PointerEventButton::Middle),
        _ => None,
    }
}

/// The pointer events one tool event stands for, given whether the pointer is
/// already inside. Exits and button releases always go through, so nothing is
/// left pressed.
pub(crate) fn pointer_events(tool: Tool, position: &mut Option<(f32, f32)>) -> Vec<WindowEvent> {
    match tool {
        Tool::Hover { x, y } => {
            *position = Some((x, y));
            vec![WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(x, y),
            }]
        }
        Tool::Button { pressed, button } => {
            let Some((x, y)) = *position else {
                return Vec::new();
            };
            let position = slint::LogicalPosition::new(x, y);
            vec![if pressed {
                WindowEvent::PointerPressed { position, button }
            } else {
                WindowEvent::PointerReleased { position, button }
            }]
        }
        Tool::Left => {
            *position = None;
            vec![WindowEvent::PointerExited]
        }
    }
}

/// The part that talks to the compositor. It starts nothing by itself: a
/// `NativePen` is created only for a live Wayland window, so it compiles in
/// the tests too, and they cover the translation above.
mod native {
    use super::{PointerEventButton, Tool, barrel_button, pointer_events};
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::platform::wayland::WindowExtWayland;
    use slint::winit_030::winit::raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };
    use slint::{ComponentHandle, Timer, TimerMode};
    use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Duration};
    use wayland_client::{
        Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
        protocol::{wl_compositor, wl_registry, wl_seat, wl_shm, wl_surface},
    };
    use wayland_protocols::wp::tablet::zv2::client::{
        zwp_tablet_manager_v2, zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2,
        zwp_tablet_v2,
    };
    use wayland_protocols::xdg::shell::client::xdg_toplevel;

    use crate::app::AppWindow;

    /// Linux evdev code of the tip, which some compositors also report as a
    /// button: the tip is already handled by `down` and `up`.
    const BTN_TOUCH: u32 = 0x14a;

    struct State {
        surface: usize,
        manager: Option<zwp_tablet_manager_v2::ZwpTabletManagerV2>,
        seats: Vec<wl_seat::WlSeat>,
        tablet_seats: Vec<zwp_tablet_seat_v2::ZwpTabletSeatV2>,
        compositor: Option<wl_compositor::WlCompositor>,
        shm: Option<wl_shm::WlShm>,
        cursor_theme: Option<wayland_cursor::CursorTheme>,
        cursor_surfaces: HashMap<wayland_client::backend::ObjectId, wl_surface::WlSurface>,
        tools: HashMap<wayland_client::backend::ObjectId, zwp_tablet_tool_v2::ZwpTabletToolV2>,
        /// Whether the tool is over the dashboard window.
        over_window: bool,
        /// Serial of the last tip-down over the window: the only thing the
        /// compositor accepts to start a window move from a pen.
        down_serial: Option<u32>,
        events: Vec<Tool>,
    }

    struct Pump {
        queue: EventQueue<State>,
        state: State,
        connection: Connection,
    }

    pub(crate) struct NativePen {
        timer: Timer,
        pump: Rc<RefCell<Pump>>,
        // Keeps the Slint backend, and so the borrowed display, alive.
        _app: AppWindow,
    }

    impl NativePen {
        pub(crate) fn new(app: &AppWindow) -> Result<Self, String> {
            let (display, surface) = app
                .window()
                .with_winit_window(|window| {
                    let display = window.display_handle().map_err(|error| error.to_string())?;
                    let surface = window.window_handle().map_err(|error| error.to_string())?;
                    match (display.as_raw(), surface.as_raw()) {
                        (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) => {
                            Ok((display.display, surface.surface))
                        }
                        _ => Err("window is not a native Wayland surface".to_owned()),
                    }
                })
                .ok_or("native window is unavailable")??;
            // SAFETY: Winit owns this display. The app is retained until our
            // proxies and queue are gone; a guest backend never disconnects it.
            let backend = unsafe {
                wayland_backend::client::Backend::from_foreign_display(display.as_ptr().cast())
            };
            let connection = Connection::from_backend(backend);
            let mut queue = connection.new_event_queue();
            let registry = connection.display().get_registry(&queue.handle(), ());
            let mut state = State {
                surface: surface.as_ptr() as usize,
                manager: None,
                seats: Vec::new(),
                tablet_seats: Vec::new(),
                compositor: None,
                shm: None,
                cursor_theme: None,
                cursor_surfaces: HashMap::new(),
                tools: HashMap::new(),
                over_window: false,
                down_serial: None,
                events: Vec::new(),
            };
            queue
                .roundtrip(&mut state)
                .map_err(|error| error.to_string())?;
            if state.manager.is_none() || state.seats.is_empty() {
                return Err("compositor does not expose the tablet protocol".into());
            }
            if let Some(shm) = state.shm.clone() {
                match wayland_cursor::CursorTheme::load(&connection, shm, 32) {
                    Ok(theme) => state.cursor_theme = Some(theme),
                    Err(error) => log::warn!("could not load the pen cursor: {error}"),
                }
            }
            let _ = registry;
            connection.flush().map_err(|error| error.to_string())?;
            let pump = Rc::new(RefCell::new(Pump {
                queue,
                state,
                connection,
            }));
            let weak = Rc::downgrade(&pump);
            let target = app.as_weak();
            let position = RefCell::new(None);
            let timer = Timer::default();
            timer.start(TimerMode::Repeated, Duration::from_millis(8), move || {
                let Some(pump) = weak.upgrade() else { return };
                let events = {
                    let mut pump = pump.borrow_mut();
                    let Pump {
                        queue,
                        state,
                        connection,
                    } = &mut *pump;
                    // Winit reads the shared socket: dispatch only what is
                    // already buffered for this queue and never block the UI.
                    if let Err(error) = queue.dispatch_pending(state) {
                        log::warn!("Wayland tablet dispatch failed: {error}");
                    }
                    let _ = connection.flush();
                    std::mem::take(&mut state.events)
                };
                let Some(app) = target.upgrade() else { return };
                for tool in events {
                    for event in pointer_events(tool, &mut position.borrow_mut()) {
                        app.window().dispatch_event(event);
                    }
                }
            });
            log::info!("native Wayland tablet input ready");
            PEN_PUMP.with(|slot| *slot.borrow_mut() = Rc::downgrade(&pump));
            Ok(Self {
                timer,
                pump,
                _app: app.clone_strong(),
            })
        }
    }

    impl Drop for NativePen {
        fn drop(&mut self) {
            self.timer.stop();
            let mut pump = self.pump.borrow_mut();
            for (_, tool) in pump.state.tools.drain() {
                tool.destroy();
            }
            for (_, surface) in pump.state.cursor_surfaces.drain() {
                surface.destroy();
            }
            for seat in pump.state.tablet_seats.drain(..) {
                seat.destroy();
            }
            pump.state.manager = None;
            let _ = pump.connection.flush();
        }
    }

    impl State {
        fn bind_tablet_seats(&mut self, qh: &QueueHandle<Self>) {
            let Some(manager) = &self.manager else { return };
            while self.tablet_seats.len() < self.seats.len() {
                let seat = &self.seats[self.tablet_seats.len()];
                self.tablet_seats
                    .push(manager.get_tablet_seat(seat, qh, ()));
            }
        }
    }

    thread_local! {
        static PEN_PUMP: RefCell<std::rc::Weak<RefCell<Pump>>> = RefCell::new(std::rc::Weak::new());
    }

    /// Starts a window move from the pen's last tip-down. Returns whether the
    /// move was requested: false when no pen pressed the window, so the caller
    /// can fall back to the pointer's own drag.
    pub(crate) fn begin_pen_drag(app: &AppWindow) -> bool {
        PEN_PUMP.with(|slot| {
            let Some(pump) = slot.borrow().upgrade() else {
                return false;
            };
            let mut pump = pump.borrow_mut();
            let Pump {
                queue,
                state,
                connection,
            } = &mut *pump;
            if queue.dispatch_pending(state).is_err() {
                return false;
            }
            let (Some(serial), Some(seat)) = (state.down_serial, state.seats.first()) else {
                return false;
            };
            let moved = app
                .window()
                .with_winit_window(|window| {
                    let Some(toplevel) = window.xdg_toplevel() else {
                        return false;
                    };
                    // SAFETY: Winit retains the toplevel. This proxy only issues a
                    // move request and never changes its queue or destroys it.
                    let id = unsafe {
                        wayland_client::backend::ObjectId::from_ptr(
                            xdg_toplevel::XdgToplevel::interface(),
                            toplevel.as_ptr().cast(),
                        )
                    };
                    let Ok(id) = id else { return false };
                    let Ok(toplevel) = xdg_toplevel::XdgToplevel::from_id(connection, id) else {
                        return false;
                    };
                    toplevel._move(seat, serial);
                    true
                })
                .unwrap_or(false);
            if moved {
                let _ = connection.flush();
            }
            moved
        })
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                match interface.as_str() {
                    "wl_compositor" => {
                        state.compositor =
                            Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                                name,
                                version.min(4),
                                qh,
                                (),
                            ));
                    }
                    "wl_shm" => {
                        state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                    }
                    "wl_seat" => {
                        state.seats.push(registry.bind::<wl_seat::WlSeat, _, _>(
                            name,
                            version.min(5),
                            qh,
                            (),
                        ));
                    }
                    "zwp_tablet_manager_v2" => {
                        state.manager = Some(
                            registry.bind::<zwp_tablet_manager_v2::ZwpTabletManagerV2, _, _>(
                                name,
                                version.min(2),
                                qh,
                                (),
                            ),
                        );
                    }
                    _ => {}
                }
                state.bind_tablet_seats(qh);
            }
        }
    }

    impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for State {
        fn event(
            state: &mut Self,
            _: &zwp_tablet_seat_v2::ZwpTabletSeatV2,
            event: zwp_tablet_seat_v2::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let zwp_tablet_seat_v2::Event::ToolAdded { id } = event {
                log::debug!("tablet tool added");
                state.tools.insert(id.id(), id);
            }
        }
        wayland_client::event_created_child!(State, zwp_tablet_seat_v2::ZwpTabletSeatV2, [
            zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (zwp_tablet_v2::ZwpTabletV2, ()),
            zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (zwp_tablet_tool_v2::ZwpTabletToolV2, ()),
            zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (zwp_tablet_pad_v2::ZwpTabletPadV2, ())
        ]);
    }

    impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, ()> for State {
        fn event(
            state: &mut Self,
            tool: &zwp_tablet_tool_v2::ZwpTabletToolV2,
            event: zwp_tablet_tool_v2::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            use zwp_tablet_tool_v2::Event;
            match event {
                Event::ProximityIn {
                    surface, serial, ..
                } => {
                    let ours = surface.id().as_ptr() as usize == state.surface;
                    log::debug!(
                        "tablet tool entered a surface: ours={ours} (surface {:#x}, window {:#x})",
                        surface.id().as_ptr() as usize,
                        state.surface
                    );
                    state.over_window = ours;
                    if ours {
                        if let (Some(compositor), Some(theme)) =
                            (&state.compositor, &mut state.cursor_theme)
                        {
                            if let Some(cursor) = theme.get_cursor("left_ptr") {
                                let image = &cursor[0];
                                let cursor_surface = state
                                    .cursor_surfaces
                                    .entry(tool.id())
                                    .or_insert_with(|| compositor.create_surface(qh, ()));
                                let (width, height) = image.dimensions();
                                let (x, y) = image.hotspot();
                                tool.set_cursor(serial, Some(cursor_surface), x as i32, y as i32);
                                cursor_surface.attach(Some(image), 0, 0);
                                cursor_surface.damage(0, 0, width as i32, height as i32);
                                cursor_surface.commit();
                            }
                        }
                    }
                }
                Event::ProximityOut => {
                    state.down_serial = None;
                    if state.over_window {
                        state.events.push(Tool::Left);
                    }
                    state.over_window = false;
                }
                Event::Motion { x, y } if state.over_window => {
                    state.events.push(Tool::Hover {
                        x: x as f32,
                        y: y as f32,
                    });
                }
                Event::Down { serial } if state.over_window => {
                    state.down_serial = Some(serial);
                    state.events.push(Tool::Button {
                        pressed: true,
                        button: PointerEventButton::Left,
                    });
                }
                Event::Up => {
                    // The serial only lets a window move start while the tip is
                    // down: later, a stale one would be refused by the compositor
                    // and would also hide the real mouse's own window drag.
                    state.down_serial = None;
                    if state.over_window {
                        state.events.push(Tool::Button {
                            pressed: false,
                            button: PointerEventButton::Left,
                        });
                    }
                }
                Event::Button {
                    button,
                    state: wayland_client::WEnum::Value(button_state),
                    ..
                } if state.over_window && button != BTN_TOUCH => {
                    if let Some(button) = barrel_button(button) {
                        state.events.push(Tool::Button {
                            pressed: button_state == zwp_tablet_tool_v2::ButtonState::Pressed,
                            button,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for State {
        fn event(
            _: &mut Self,
            _: &wl_seat::WlSeat,
            _: wl_seat::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(State: ignore wl_compositor::WlCompositor);
    delegate_noop!(State: ignore wl_shm::WlShm);
    delegate_noop!(State: ignore wl_surface::WlSurface);
    delegate_noop!(State: ignore zwp_tablet_manager_v2::ZwpTabletManagerV2);
    delegate_noop!(State: ignore zwp_tablet_v2::ZwpTabletV2);
    delegate_noop!(State: ignore zwp_tablet_pad_v2::ZwpTabletPadV2);
}

pub(crate) use native::{NativePen, begin_pen_drag};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barrel_buttons_follow_the_compositor_mapping() {
        assert_eq!(barrel_button(BTN_STYLUS), Some(PointerEventButton::Right));
        assert_eq!(barrel_button(BTN_STYLUS2), Some(PointerEventButton::Middle));
        assert_eq!(barrel_button(0x110), None);
    }

    #[test]
    fn a_click_lands_where_the_pen_last_was() {
        let mut position = None;
        let moved = pointer_events(Tool::Hover { x: 40.0, y: 25.0 }, &mut position);
        assert!(matches!(
            moved.as_slice(),
            [WindowEvent::PointerMoved { position }] if position.x == 40.0 && position.y == 25.0
        ));
        let down = pointer_events(
            Tool::Button {
                pressed: true,
                button: PointerEventButton::Left,
            },
            &mut position,
        );
        assert!(matches!(
            down.as_slice(),
            [WindowEvent::PointerPressed { position, button: PointerEventButton::Left }]
                if position.x == 40.0 && position.y == 25.0
        ));
    }

    #[test]
    fn a_button_before_any_position_is_dropped_and_leaving_clears_it() {
        let mut position = None;
        let early = pointer_events(
            Tool::Button {
                pressed: true,
                button: PointerEventButton::Left,
            },
            &mut position,
        );
        assert!(early.is_empty(), "no position yet: nothing to press at");

        pointer_events(Tool::Hover { x: 1.0, y: 2.0 }, &mut position);
        let gone = pointer_events(Tool::Left, &mut position);
        assert!(matches!(gone.as_slice(), [WindowEvent::PointerExited]));
        assert!(position.is_none());
    }
}
