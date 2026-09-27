//! File drops on the existing Slint/Winit Wayland connection.
//!
//! A separate event queue owns our data devices and a touch observer. It never
//! captures input or creates a portal session; window moves use the touch serial.
use super::{AppWindow, DropCallback, FileDropEvent, LogicalPosition, winit};
use slint::{ComponentHandle, Timer, TimerMode, winit_030::WinitWindowAccessor};
use std::{
    cell::RefCell,
    collections::HashMap,
    io::Read,
    os::fd::AsFd,
    os::unix::net::UnixStream,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    protocol::{
        wl_data_device, wl_data_device_manager, wl_data_offer, wl_registry, wl_seat, wl_touch,
    },
};
use wayland_protocols::xdg::shell::client::xdg_toplevel;
use winit::platform::wayland::WindowExtWayland;
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

thread_local! {
    static TOUCH_PUMP: RefCell<std::rc::Weak<RefCell<Pump>>> = RefCell::new(std::rc::Weak::new());
}

pub(super) fn begin_touch_drag(app: &AppWindow) -> bool {
    TOUCH_PUMP.with(|slot| {
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
        let Some((seat, serial, _)) = state.active_touch.as_ref() else {
            return false;
        };
        let moved = app
            .window()
            .with_winit_window(|window| {
                let Some(pointer) = window.xdg_toplevel() else {
                    return false;
                };
                // SAFETY: Winit retains the toplevel. This proxy only issues a move
                // request and never changes its queue or destroys the borrowed object.
                let id = unsafe {
                    wayland_client::backend::ObjectId::from_ptr(
                        xdg_toplevel::XdgToplevel::interface(),
                        pointer.as_ptr().cast(),
                    )
                };
                let Ok(id) = id else { return false };
                let Ok(toplevel) = xdg_toplevel::XdgToplevel::from_id(connection, id) else {
                    return false;
                };
                toplevel._move(seat, *serial);
                true
            })
            .unwrap_or(false);
        if moved {
            let _ = connection.flush();
        }
        moved
    })
}

const URI_LIST: &str = "text/uri-list";
const MAX_BYTES: usize = 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn is_wayland(window: &winit::window::Window) -> bool {
    matches!(
        window.window_handle().map(|handle| handle.as_raw()),
        Ok(RawWindowHandle::Wayland(_))
    )
}

pub(super) struct NativeDrop {
    timer: Timer,
    pump: Rc<RefCell<Pump>>,
    // Keeps the Slint backend (and thus the borrowed display) alive through cleanup.
    _app: AppWindow,
}

struct Pump {
    queue: EventQueue<State>,
    state: State,
    connection: Connection,
}

struct Offer {
    proxy: wl_data_offer::WlDataOffer,
    uri_list: bool,
}

struct Drag {
    offer: wl_data_offer::WlDataOffer,
    position: LogicalPosition,
}

struct PendingRead {
    drag: Drag,
    reader: UnixStream,
    bytes: Vec<u8>,
    deadline: Instant,
}

struct State {
    surface: usize,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    seats: Vec<(u32, wl_seat::WlSeat, Option<wl_data_device::WlDataDevice>)>,
    touches: HashMap<wayland_client::backend::ObjectId, wl_touch::WlTouch>,
    active_touch: Option<(wl_seat::WlSeat, u32, i32)>,
    offers: HashMap<wayland_client::backend::ObjectId, Offer>,
    drag: Option<Drag>,
    pending: Option<PendingRead>,
    events: Vec<FileDropEvent>,
}

impl NativeDrop {
    pub(super) fn new(app: &AppWindow, callback: DropCallback) -> Result<Self, String> {
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
        // SAFETY: Winit owns this display. The app is retained until our proxies,
        // queue, and guest backend have been destroyed; guest mode never disconnects it.
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
            touches: HashMap::new(),
            active_touch: None,
            offers: HashMap::new(),
            drag: None,
            pending: None,
            events: Vec::new(),
        };
        queue
            .roundtrip(&mut state)
            .map_err(|error| error.to_string())?;
        if state.manager.is_none() || state.seats.is_empty() {
            state.cleanup();
            return Err("compositor does not expose a file-drop data device".into());
        }
        // Registry remains alive in the queue so seat removal can be handled.
        let _ = registry;
        connection.flush().map_err(|error| error.to_string())?;
        let pump = Rc::new(RefCell::new(Pump {
            queue,
            state,
            connection,
        }));
        let weak = Rc::downgrade(&pump);
        TOUCH_PUMP.with(|slot| *slot.borrow_mut() = Rc::downgrade(&pump));
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
            let Some(pump) = weak.upgrade() else { return };
            let events = {
                let mut pump = pump.borrow_mut();
                let Pump {
                    queue,
                    state,
                    connection,
                } = &mut *pump;
                // Winit reads the shared display socket. Dispatch only this queue's
                // buffered events; never race its read guard or block the UI.
                if let Err(error) = queue.dispatch_pending(state) {
                    log::warn!("Wayland file-drop dispatch failed: {error}");
                    state.cleanup();
                }
                state.read_pending();
                if let Err(error) = connection.flush() {
                    log::debug!("Wayland file-drop flush: {error}");
                }
                std::mem::take(&mut state.events)
            };
            for event in events {
                callback.borrow_mut()(event);
            }
        });
        log::info!("native Wayland file-drop data device ready");
        Ok(Self {
            timer,
            pump,
            _app: app.clone_strong(),
        })
    }
}

impl Drop for NativeDrop {
    fn drop(&mut self) {
        self.timer.stop();
        let mut pump = self.pump.borrow_mut();
        pump.state.cleanup();
        let _ = pump.connection.flush();
    }
}

impl State {
    fn bind_devices(&mut self, qh: &QueueHandle<Self>) {
        if let Some(manager) = &self.manager {
            for (_, seat, device) in &mut self.seats {
                if device.is_none() {
                    *device = Some(manager.get_data_device(seat, qh, ()));
                }
            }
        }
    }

    fn discard_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            self.destroy_offer(&drag.offer);
        }
        self.events.push(FileDropEvent::Hover(None));
    }

    fn destroy_offer(&mut self, proxy: &wl_data_offer::WlDataOffer) {
        if self.offers.remove(&proxy.id()).is_some() {
            proxy.destroy();
        }
    }

    fn read_pending(&mut self) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        let mut chunk = [0u8; 8192];
        let result = loop {
            if Instant::now() >= pending.deadline {
                break Err("file-drop source timed out".to_owned());
            }
            match pending.reader.read(&mut chunk) {
                Ok(0) => break parse_uri_list(&pending.bytes),
                Ok(n) => {
                    if pending.bytes.len() + n > MAX_BYTES {
                        break Err("file-drop URI list exceeds 1 MiB".into());
                    }
                    pending.bytes.extend_from_slice(&chunk[..n]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => break Err(format!("cannot read file-drop data: {error}")),
            }
        };
        let pending = self.pending.take().expect("pending read exists");
        match result {
            Ok(paths) => {
                // Only Copy is negotiated: receiving must never delete source files.
                if pending.drag.offer.version() >= 3 {
                    pending.drag.offer.finish();
                }
                log::info!("native Wayland file drop received: files={}", paths.len());
                for path in paths {
                    self.events.push(FileDropEvent::Drop {
                        path,
                        position: pending.drag.position,
                    });
                }
            }
            Err(error) => log::warn!("{error}"),
        }
        self.destroy_offer(&pending.drag.offer);
    }

    fn cleanup(&mut self) {
        self.drag = None;
        self.pending = None;
        self.active_touch = None;
        for (_, touch) in self.touches.drain() {
            touch.release();
        }
        for (_, offer) in self.offers.drain() {
            offer.proxy.destroy();
        }
        for (_, seat, device) in self.seats.drain(..) {
            if let Some(device) = device {
                if device.version() >= 2 {
                    device.release();
                }
            }
            if seat.version() >= 5 {
                seat.release();
            }
        }
        self.manager = None;
    }
}

fn parse_uri_list(bytes: &[u8]) -> Result<Vec<PathBuf>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "file-drop URI list is not UTF-8")?;
    let mut paths = Vec::new();
    for line in text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let uri = url::Url::parse(line).map_err(|_| "invalid file-drop URI")?;
        let path = uri
            .to_file_path()
            .map_err(|_| "file drop contains a non-local URI")?;
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err("invalid local file-drop path".into());
        }
        paths.push(path);
    }
    if paths.is_empty() {
        return Err("file drop has no local files".into());
    }
    Ok(paths)
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
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_data_device_manager" && state.manager.is_none() {
                    state.manager = Some(registry.bind(name, version.min(3), qh, ()));
                } else if interface == "wl_seat" {
                    state
                        .seats
                        .push((name, registry.bind(name, version.min(5), qh, ()), None));
                }
                state.bind_devices(qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(index) = state.seats.iter().position(|(id, _, _)| *id == name) {
                    let (_, seat, device) = state.seats.remove(index);
                    if let Some(touch) = state.touches.remove(&seat.id()) {
                        touch.release();
                    }
                    if state
                        .active_touch
                        .as_ref()
                        .is_some_and(|(active, _, _)| active == &seat)
                    {
                        state.active_touch = None;
                    }
                    if let Some(device) = device {
                        if device.version() >= 2 {
                            device.release();
                        }
                    }
                    if seat.version() >= 5 {
                        seat.release();
                    }
                    state.discard_drag();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.offers.insert(
                    id.id(),
                    Offer {
                        proxy: id,
                        uri_list: false,
                    },
                );
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                state.discard_drag();
                if let Some(offer) = id {
                    let accepted = surface.id().as_ptr() as usize == state.surface
                        && state.pending.is_none()
                        && state
                            .offers
                            .get(&offer.id())
                            .is_some_and(|offer| offer.uri_list);
                    log::debug!(
                        "Wayland file drag entered: target={} files={} busy={}",
                        surface.id().as_ptr() as usize == state.surface,
                        state
                            .offers
                            .get(&offer.id())
                            .is_some_and(|offer| offer.uri_list),
                        state.pending.is_some()
                    );
                    offer.accept(serial, accepted.then(|| URI_LIST.into()));
                    if offer.version() >= 3 {
                        let action = if accepted {
                            wl_data_device_manager::DndAction::Copy
                        } else {
                            wl_data_device_manager::DndAction::empty()
                        };
                        offer.set_actions(action, action);
                    }
                    if accepted {
                        let position = LogicalPosition::new(x as f32, y as f32);
                        state.events.push(FileDropEvent::Hover(Some(position)));
                        state.drag = Some(Drag { offer, position });
                    } else {
                        state.destroy_offer(&offer);
                    }
                }
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                if let Some(drag) = &mut state.drag {
                    drag.position = LogicalPosition::new(x as f32, y as f32);
                    state.events.push(FileDropEvent::Hover(Some(drag.position)));
                }
            }
            wl_data_device::Event::Leave => state.discard_drag(),
            wl_data_device::Event::Drop => {
                if let Some(drag) = state.drag.take() {
                    match UnixStream::pair().and_then(|(reader, writer)| {
                        reader.set_nonblocking(true)?;
                        Ok((reader, writer))
                    }) {
                        Ok((reader, writer)) => {
                            drag.offer.receive(URI_LIST.into(), writer.as_fd());
                            state.pending = Some(PendingRead {
                                drag,
                                reader,
                                bytes: Vec::new(),
                                deadline: Instant::now() + READ_TIMEOUT,
                            });
                        }
                        Err(error) => {
                            log::warn!("cannot receive Wayland file drop: {error}");
                            state.destroy_offer(&drag.offer);
                        }
                    }
                }
                state.events.push(FileDropEvent::Hover(None));
            }
            // Never read, write, or replace the user's ordinary clipboard.
            wl_data_device::Event::Selection { id: Some(offer) } => state.destroy_offer(&offer),
            _ => {}
        }
    }
    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            if mime_type == URI_LIST {
                if let Some(offer) = state.offers.get_mut(&offer.id()) {
                    offer.uri_list = true;
                }
            }
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Touch) {
                state
                    .touches
                    .entry(seat.id())
                    .or_insert_with(|| seat.get_touch(qh, seat.clone()));
            } else {
                if let Some(touch) = state.touches.remove(&seat.id()) {
                    touch.release();
                }
                if state
                    .active_touch
                    .as_ref()
                    .is_some_and(|(active, _, _)| active == seat)
                {
                    state.active_touch = None;
                }
            }
        }
    }
}

impl Dispatch<wl_touch::WlTouch, wl_seat::WlSeat> for State {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        seat: &wl_seat::WlSeat,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_touch::Event::Down {
                serial,
                surface,
                id,
                ..
            } if surface.id().as_ptr() as usize == state.surface => {
                if state.active_touch.is_none() {
                    state.active_touch = Some((seat.clone(), serial, id));
                }
            }
            wl_touch::Event::Up { id, .. } => {
                if state
                    .active_touch
                    .as_ref()
                    .is_some_and(|(active, _, finger)| active == seat && *finger == id)
                {
                    state.active_touch = None;
                }
            }
            wl_touch::Event::Cancel => {
                state.active_touch = None;
            }
            _ => {}
        }
    }
}
delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uri_list_preserves_escaped_names_and_rejects_remote_resources() {
        assert_eq!(
            parse_uri_list(b"# selection\r\nfile:///tmp/a%20b%23c\r\nfile:///tmp/.cargo-lock\r\n")
                .unwrap(),
            vec![
                PathBuf::from("/tmp/a b#c"),
                PathBuf::from("/tmp/.cargo-lock")
            ]
        );
        for invalid in [
            b"https://example.com/file".as_slice(),
            b"file://remote/tmp/file",
            b"file:///tmp/a%00b",
            b"# no files\r\n",
        ] {
            assert!(parse_uri_list(invalid).is_err());
        }
    }
}
