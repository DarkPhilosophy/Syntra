//! Kernel-level emulation through `/dev/uinput`.
//!
//! The backend creates one virtual keyboard-and-mouse device. To the kernel
//! and every compositor it is indistinguishable from USB hardware, so it
//! works identically on GNOME, KDE, wlroots and X11, needs no portal consent,
//! has no session that can expire, and keeps working at a lock or login
//! screen. This is the approach Sunshine uses for remote input.
//!
//! Access requires write permission on `/dev/uinput`, normally granted by the
//! udev rule shipped in `build-aux/60-syntra-uinput.rules`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use async_trait::async_trait;
use syntra_input_event::{Event, KeyboardEvent, PointerEvent};

use super::{Emulation, EmulationHandle, PointerEdge, error::EmulationError};

const DEVICE_PATH: &str = "/dev/uinput";
const DEVICE_NAME: &[u8] = b"Syntra virtual input";

// linux/input-event-codes.h
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
/// Every keyboard key code below the button range.
const KEYS: std::ops::RangeInclusive<u16> = 1..=0xff;
/// `BTN_LEFT` through `BTN_TASK`.
const BUTTONS: std::ops::RangeInclusive<u16> = 0x110..=0x117;
const BUS_VIRTUAL: u16 = 0x06;

// linux/uinput.h ioctls, `_IO('U', n)` and `_IOW('U', n, T)`.
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong =
    0x4000_0000 | ((std::mem::size_of::<libc::uinput_setup>() as libc::c_ulong) << 16) | 0x5503;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;

/// One high-resolution wheel detent, as defined by the kernel.
const WHEEL_DETENT: i32 = 120;
/// Wayland reports a wheel detent as 15 logical scroll units.
const WAYLAND_UNITS_PER_DETENT: f64 = 15.0;

/// Why the uinput device could not be created.
#[derive(Debug, thiserror::Error)]
pub enum UinputEmulationCreationError {
    /// `/dev/uinput` is missing or not writable by this user.
    #[error(
        "cannot open {DEVICE_PATH}: {0} (install the Syntra udev rule or add the user to the input group)"
    )]
    Open(#[source] io::Error),
    /// The kernel rejected device setup.
    #[error("uinput device setup failed: {0}")]
    Setup(#[source] io::Error),
}

pub(crate) struct UinputEmulation {
    device: VirtualDevice,
    scroll: ScrollAccumulator,
    /// When set, each peer drives its own pen-tablet cursor instead of the
    /// shared pointer.
    independent: bool,
    pointers: HashMap<EmulationHandle, PeerPointer>,
}

impl UinputEmulation {
    pub(crate) fn new() -> Result<Self, UinputEmulationCreationError> {
        let device = VirtualDevice::open().map_err(UinputEmulationCreationError::Open)?;
        configure(&device.file).map_err(UinputEmulationCreationError::Setup)?;
        Ok(Self {
            device,
            scroll: ScrollAccumulator::default(),
            independent: false,
            pointers: HashMap::new(),
        })
    }

    /// Routes a pointer event to the peer's own cursor, creating it on first
    /// use. Returns `false` when the event is not handled there.
    fn consume_independent(&mut self, event: Event, handle: EmulationHandle) -> io::Result<bool> {
        let Event::Pointer(pointer_event) = event else {
            return Ok(false);
        };
        if matches!(
            pointer_event,
            PointerEvent::Axis { .. } | PointerEvent::AxisDiscrete120 { .. }
        ) {
            // A pen has no wheel: scrolling goes through the shared device.
            return Ok(false);
        }
        let pointer = match self.pointers.entry(handle) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let area = pointer_area();
                log::info!(
                    "creating independent pointer for peer {handle} over {}x{}",
                    area.0,
                    area.1
                );
                entry.insert(PeerPointer::create(handle, area)?)
            }
        };
        let events = pointer.translate(pointer_event);
        pointer.device.write_events(&events)?;
        Ok(true)
    }
}

/// An open `/dev/uinput` handle; dropping it destroys the device.
struct VirtualDevice {
    file: File,
}

impl VirtualDevice {
    fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(DEVICE_PATH)?;
        Ok(Self { file })
    }

    fn write_events(&self, events: &[RawEvent]) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut buffer = Vec::with_capacity(events.len() + 1);
        for event in events.iter().chain(std::iter::once(&SYN)) {
            // SAFETY: input_event is plain old data; zero is a valid value
            // for every field and the kernel stamps the time itself.
            let mut raw: libc::input_event = unsafe { std::mem::zeroed() };
            raw.type_ = event.kind;
            raw.code = event.code;
            raw.value = event.value;
            buffer.push(raw);
        }
        let bytes = std::mem::size_of_val(buffer.as_slice());
        // SAFETY: the pointer and length describe the initialised buffer.
        let written = unsafe { libc::write(self.file.as_raw_fd(), buffer.as_ptr().cast(), bytes) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize != bytes {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short write to uinput device",
            ));
        }
        Ok(())
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        // SAFETY: plain ioctl on an fd owned by `self`; closing the fd would
        // destroy the device as well, this only makes it explicit.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

// Pen-tablet vocabulary used for independent pointers.
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOUCH: u16 = 0x14a;
const BTN_STYLUS: u16 = 0x14b;
const BTN_STYLUS2: u16 = 0x14c;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
const UI_ABS_SETUP: libc::c_ulong =
    0x4000_0000 | ((std::mem::size_of::<libc::uinput_abs_setup>() as libc::c_ulong) << 16) | 0x5504;
/// Used when the desktop size cannot be determined.
const DEFAULT_POINTER_AREA: (i32, i32) = (1920, 1080);

/// A peer's own cursor on the local desktop.
///
/// Compositors give each pen-tablet tool a cursor of its own, separate from
/// the seat pointer; GNOME does so without any patch. Relative motion from
/// the peer is integrated into an absolute position whose range equals the
/// desktop in pixels, so one unit moves the cursor by one pixel.
struct PeerPointer {
    device: VirtualDevice,
    pen: PenState,
    /// Events sent before the compositor has opened the new device are
    /// lost, including the proximity-in that makes the cursor exist; after
    /// that libinput ignores the pen entirely. Motion is only integrated
    /// until then.
    ready_at: std::time::Instant,
}

/// Device-independent pen state, kept apart so it can be tested.
struct PenState {
    area: (i32, i32),
    position: (f64, f64),
    in_proximity: bool,
    /// Edge the last motion tried to cross.
    edge: Option<PointerEdge>,
}

impl PenState {
    fn new(area: (i32, i32)) -> Self {
        Self {
            area,
            position: (f64::from(area.0) / 2.0, f64::from(area.1) / 2.0),
            in_proximity: false,
            edge: None,
        }
    }
}

impl PeerPointer {
    fn create(handle: EmulationHandle, area: (i32, i32)) -> io::Result<Self> {
        let device = VirtualDevice::open()?;
        let file = &device.file;
        ioctl(file, UI_SET_EVBIT, EV_KEY.into())?;
        ioctl(file, UI_SET_EVBIT, EV_ABS.into())?;
        for key in [BTN_TOOL_PEN, BTN_TOUCH, BTN_STYLUS, BTN_STYLUS2] {
            ioctl(file, UI_SET_KEYBIT, key.into())?;
        }
        for (axis, extent) in [(ABS_X, area.0), (ABS_Y, area.1)] {
            ioctl(file, UI_SET_ABSBIT, axis.into())?;
            // SAFETY: plain old data; all-zero is valid.
            let mut setup: libc::uinput_abs_setup = unsafe { std::mem::zeroed() };
            setup.code = axis;
            setup.absinfo.maximum = extent - 1;
            // libinput rejects tablets without a physical resolution.
            setup.absinfo.resolution = 10;
            // SAFETY: UI_ABS_SETUP reads one uinput_abs_setup.
            if unsafe { libc::ioctl(file.as_raw_fd(), UI_ABS_SETUP, &setup) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let name = format!("Syntra peer pointer {handle}");
        create_device(file, name.as_bytes(), 0x0002)?;
        Ok(Self {
            device,
            pen: PenState::new(area),
            ready_at: std::time::Instant::now() + DEVICE_SETTLE_TIME,
        })
    }

    fn translate(&mut self, event: PointerEvent) -> Vec<RawEvent> {
        if std::time::Instant::now() < self.ready_at {
            self.pen.track(event);
            return Vec::new();
        }
        self.pen.translate(event)
    }

    /// Lifts the pen away so the compositor hides the cursor.
    fn leave(&mut self) -> io::Result<()> {
        let events = self.pen.leave();
        self.device.write_events(&events)
    }
}

/// How long a compositor needs to discover and open a new input device.
const DEVICE_SETTLE_TIME: std::time::Duration = std::time::Duration::from_millis(600);

impl PenState {
    /// Follows motion without emitting anything.
    fn track(&mut self, event: PointerEvent) {
        let proximity = self.in_proximity;
        self.in_proximity = true;
        let _ = self.translate(event);
        self.in_proximity = proximity;
    }

    fn translate(&mut self, event: PointerEvent) -> Vec<RawEvent> {
        let mut events = Vec::with_capacity(4);
        if !self.in_proximity {
            // Entering proximity makes the cursor appear where it last was.
            self.in_proximity = true;
            events.push(raw(EV_ABS, ABS_X, self.position.0 as i32));
            events.push(raw(EV_ABS, ABS_Y, self.position.1 as i32));
            events.push(raw(EV_KEY, BTN_TOOL_PEN, 1));
        }
        match event {
            PointerEvent::Motion { dx, dy, .. } => {
                if dx.is_finite() && dy.is_finite() {
                    let max = (f64::from(self.area.0 - 1), f64::from(self.area.1 - 1));
                    let target = (self.position.0 + dx, self.position.1 + dy);
                    // Pushing past an edge is how the user leaves this
                    // screen; the shared pointer gets the same signal from
                    // local capture, which cannot see this cursor.
                    self.edge = if target.0 < 0.0 {
                        Some(PointerEdge::Left)
                    } else if target.0 > max.0 {
                        Some(PointerEdge::Right)
                    } else if target.1 < 0.0 {
                        Some(PointerEdge::Top)
                    } else if target.1 > max.1 {
                        Some(PointerEdge::Bottom)
                    } else {
                        self.edge
                    };
                    self.position.0 = target.0.clamp(0.0, max.0);
                    self.position.1 = target.1.clamp(0.0, max.1);
                    events.push(raw(EV_ABS, ABS_X, self.position.0 as i32));
                    events.push(raw(EV_ABS, ABS_Y, self.position.1 as i32));
                }
            }
            PointerEvent::Button { button, state, .. } => {
                // Tip for the primary button; the barrel buttons carry the
                // secondary and middle clicks, as GNOME maps them by default.
                let code = match button {
                    syntra_input_event::BTN_LEFT => Some(BTN_TOUCH),
                    syntra_input_event::BTN_RIGHT => Some(BTN_STYLUS2),
                    syntra_input_event::BTN_MIDDLE => Some(BTN_STYLUS),
                    _ => None,
                };
                if let Some(code) = code {
                    events.push(raw(EV_KEY, code, i32::from(state != 0)));
                }
            }
            PointerEvent::Axis { .. } | PointerEvent::AxisDiscrete120 { .. } => {}
        }
        events
    }

    fn leave(&mut self) -> Vec<RawEvent> {
        if !self.in_proximity {
            return Vec::new();
        }
        self.in_proximity = false;
        vec![
            raw(EV_KEY, BTN_TOUCH, 0),
            raw(EV_KEY, BTN_STYLUS, 0),
            raw(EV_KEY, BTN_STYLUS2, 0),
            raw(EV_KEY, BTN_TOOL_PEN, 0),
        ]
    }
}

impl Drop for PeerPointer {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// Logical desktop size in pixels, which the pen range is mapped onto.
///
/// `SYNTRA_POINTER_AREA=WIDTHxHEIGHT` overrides detection. Otherwise the
/// Xwayland/X11 root window is used: it spans every monitor in logical
/// pixels, which is also what the compositor maps an unbound tablet onto.
fn pointer_area() -> (i32, i32) {
    if let Some(area) = std::env::var("SYNTRA_POINTER_AREA")
        .ok()
        .and_then(|value| parse_area(&value))
    {
        return area;
    }
    x_root_size().unwrap_or(DEFAULT_POINTER_AREA)
}

fn parse_area(value: &str) -> Option<(i32, i32)> {
    let (width, height) = value.trim().split_once(['x', 'X'])?;
    let area = (width.parse().ok()?, height.parse().ok()?);
    (area.0 > 1 && area.1 > 1).then_some(area)
}

#[cfg(x11)]
fn x_root_size() -> Option<(i32, i32)> {
    use x11::xlib;
    // SAFETY: standard Xlib connection lifecycle; the display is closed
    // before returning and no pointers escape.
    unsafe {
        let display = xlib::XOpenDisplay(std::ptr::null());
        if display.is_null() {
            return None;
        }
        let screen = xlib::XDefaultScreen(display);
        let size = (
            xlib::XDisplayWidth(display, screen),
            xlib::XDisplayHeight(display, screen),
        );
        xlib::XCloseDisplay(display);
        (size.0 > 1 && size.1 > 1).then_some(size)
    }
}

#[cfg(not(x11))]
fn x_root_size() -> Option<(i32, i32)> {
    None
}

fn ioctl(device: &File, request: libc::c_ulong, value: libc::c_int) -> io::Result<()> {
    // SAFETY: the UI_SET_* requests take an int by value.
    if unsafe { libc::ioctl(device.as_raw_fd(), request, value) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn configure(device: &File) -> io::Result<()> {
    ioctl(device, UI_SET_EVBIT, EV_KEY.into())?;
    ioctl(device, UI_SET_EVBIT, EV_REL.into())?;
    for key in KEYS.chain(BUTTONS) {
        ioctl(device, UI_SET_KEYBIT, key.into())?;
    }
    for axis in [
        REL_X,
        REL_Y,
        REL_WHEEL,
        REL_HWHEEL,
        REL_WHEEL_HI_RES,
        REL_HWHEEL_HI_RES,
    ] {
        ioctl(device, UI_SET_RELBIT, axis.into())?;
    }

    create_device(device, DEVICE_NAME, 0x0001)
}

fn create_device(device: &File, name: &[u8], product: u16) -> io::Result<()> {
    // SAFETY: uinput_setup is plain old data and all-zero is valid.
    let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
    setup.id.bustype = BUS_VIRTUAL;
    setup.id.vendor = 0x5359; // "SY"
    setup.id.product = product;
    setup.id.version = 1;
    for (slot, byte) in setup.name.iter_mut().zip(name) {
        *slot = *byte as libc::c_char;
    }
    // SAFETY: UI_DEV_SETUP reads one uinput_setup from the pointer.
    if unsafe { libc::ioctl(device.as_raw_fd(), UI_DEV_SETUP, &setup) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: UI_DEV_CREATE takes no argument.
    if unsafe { libc::ioctl(device.as_raw_fd(), UI_DEV_CREATE) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawEvent {
    kind: u16,
    code: u16,
    value: i32,
}

const SYN: RawEvent = RawEvent {
    kind: EV_SYN,
    code: SYN_REPORT,
    value: 0,
};

const fn raw(kind: u16, code: u16, value: i32) -> RawEvent {
    RawEvent { kind, code, value }
}

/// Carries fractional motion and partial wheel detents between events so
/// slow movement and smooth scrolling are not rounded away.
#[derive(Default)]
struct ScrollAccumulator {
    motion: [f64; 2],
    /// Sub-detent high-resolution scroll per axis (vertical, horizontal).
    hi_res: [i32; 2],
}

/// Largest scroll accepted from one event, in 1/120 detents.
const MAX_SCROLL_PER_EVENT: i32 = 120 * 100;
/// Largest pointer motion accepted from one event, in pixels.
const MAX_MOTION_PER_EVENT: f64 = 10_000.0;

impl ScrollAccumulator {
    fn translate(&mut self, event: Event) -> Vec<RawEvent> {
        match event {
            Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => {
                let mut events = Vec::with_capacity(2);
                for (index, (delta, code)) in [(dx, REL_X), (dy, REL_Y)].into_iter().enumerate() {
                    // A NaN or infinity would poison the accumulator and
                    // freeze the pointer for the rest of the session.
                    if !delta.is_finite() {
                        continue;
                    }
                    self.motion[index] += delta.clamp(-MAX_MOTION_PER_EVENT, MAX_MOTION_PER_EVENT);
                    let whole = self.motion[index].trunc();
                    self.motion[index] -= whole;
                    if whole != 0.0 {
                        events.push(raw(EV_REL, code, whole as i32));
                    }
                }
                events
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                if BUTTONS.contains(&u16::try_from(button).unwrap_or(0)) {
                    vec![raw(EV_KEY, button as u16, i32::from(state != 0))]
                } else {
                    Vec::new()
                }
            }
            Event::Pointer(PointerEvent::Axis { axis, value, .. }) => {
                let hi_res = (value * f64::from(WHEEL_DETENT) / WAYLAND_UNITS_PER_DETENT).round();
                self.scroll(axis, hi_res as i32)
            }
            Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
                self.scroll(axis, value)
            }
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => match u16::try_from(key) {
                Ok(code) if KEYS.contains(&code) => {
                    vec![raw(EV_KEY, code, i32::from(state != 0))]
                }
                _ => Vec::new(),
            },
            // The kernel derives modifier state from the keys themselves.
            Event::Keyboard(KeyboardEvent::Modifiers { .. }) => Vec::new(),
        }
    }

    /// `value` uses Wayland's sign convention (positive scrolls down/right);
    /// evdev wheels are positive up, horizontal wheels positive right.
    fn scroll(&mut self, axis: u8, value: i32) -> Vec<RawEvent> {
        // Peer-supplied: bound it so negation and accumulation cannot
        // overflow. The accumulator itself stays within one detent.
        let value = value.clamp(-MAX_SCROLL_PER_EVENT, MAX_SCROLL_PER_EVENT);
        if value == 0 {
            return Vec::new();
        }
        let (index, hi_res_code, detent_code, value) = match axis {
            0 => (0, REL_WHEEL_HI_RES, REL_WHEEL, -value),
            _ => (1, REL_HWHEEL_HI_RES, REL_HWHEEL, value),
        };
        let mut events = vec![raw(EV_REL, hi_res_code, value)];
        self.hi_res[index] += value;
        let detents = self.hi_res[index] / WHEEL_DETENT;
        if detents != 0 {
            self.hi_res[index] -= detents * WHEEL_DETENT;
            events.push(raw(EV_REL, detent_code, detents));
        }
        events
    }
}

#[async_trait]
impl Emulation for UinputEmulation {
    fn set_independent_pointers(&mut self, enabled: bool) {
        if self.independent == enabled {
            return;
        }
        log::info!(
            "independent peer pointers {}",
            if enabled { "enabled" } else { "disabled" }
        );
        // A button held on the old route would never see its release, which
        // leaves a drag stuck on the desktop. Release it before switching.
        if enabled {
            let release: Vec<_> = BUTTONS.map(|button| raw(EV_KEY, button, 0)).collect();
            if let Err(error) = self.device.write_events(&release) {
                log::warn!("could not release shared pointer buttons: {error}");
            }
        } else {
            // Dropping a peer pointer lifts its pen, releasing its buttons.
            self.pointers.clear();
        }
        self.independent = enabled;
    }

    fn take_pointer_edge(&mut self, handle: EmulationHandle) -> Option<PointerEdge> {
        self.pointers.get_mut(&handle)?.pen.edge.take()
    }

    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if self.independent && self.consume_independent(event, handle)? {
            return Ok(());
        }
        let events = self.scroll.translate(event);
        self.device.write_events(&events)?;
        Ok(())
    }

    // The shared keyboard/mouse serves every peer: key state per peer is
    // tracked by `InputEmulation`, and the kernel merges input like two real
    // mice. Independent pointers are created lazily on first motion.
    async fn create(&mut self, _handle: EmulationHandle) {}
    async fn destroy(&mut self, handle: EmulationHandle) {
        self.pointers.remove(&handle);
    }
    async fn terminate(&mut self) {
        self.pointers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntra_input_event::BTN_LEFT;

    fn translate(events: &[Event]) -> Vec<RawEvent> {
        let mut state = ScrollAccumulator::default();
        events
            .iter()
            .flat_map(|event| state.translate(*event))
            .collect()
    }

    #[test]
    fn peer_pen_enters_at_center_moves_in_pixels_and_clamps_to_the_desktop() {
        let mut pen = PenState::new((1000, 500));
        let motion = |dx, dy| PointerEvent::Motion { time: 0, dx, dy };
        assert_eq!(
            pen.translate(motion(10.0, -5.0)),
            vec![
                raw(EV_ABS, ABS_X, 500),
                raw(EV_ABS, ABS_Y, 250),
                raw(EV_KEY, BTN_TOOL_PEN, 1),
                raw(EV_ABS, ABS_X, 510),
                raw(EV_ABS, ABS_Y, 245),
            ]
        );
        assert_eq!(
            pen.translate(motion(1e9, -1e9)),
            vec![raw(EV_ABS, ABS_X, 999), raw(EV_ABS, ABS_Y, 0)]
        );
        assert_eq!(
            pen.translate(motion(f64::NAN, 1.0)),
            Vec::new(),
            "invalid motion is dropped without moving the pen"
        );
    }

    #[test]
    fn peer_pen_reports_the_edge_it_is_pushed_past() {
        let mut pen = PenState::new((100, 100));
        let motion = |dx, dy| PointerEvent::Motion { time: 0, dx, dy };
        pen.translate(motion(49.0, 0.0));
        assert_eq!(pen.edge.take(), None, "reaching the edge is not leaving");
        pen.translate(motion(1.0, 0.0));
        assert_eq!(pen.edge.take(), Some(PointerEdge::Right));
        pen.translate(motion(-200.0, 0.0));
        assert_eq!(pen.edge.take(), Some(PointerEdge::Left));
        pen.translate(motion(0.0, -500.0));
        assert_eq!(pen.edge.take(), Some(PointerEdge::Top));
        pen.translate(motion(10.0, 10.0));
        assert_eq!(pen.edge.take(), None);
    }

    #[test]
    fn peer_pen_maps_buttons_and_releases_everything_on_leave() {
        let mut pen = PenState::new((100, 100));
        pen.translate(PointerEvent::Motion {
            time: 0,
            dx: 0.0,
            dy: 0.0,
        });
        let button = |button, state| PointerEvent::Button {
            time: 0,
            button,
            state,
        };
        assert_eq!(
            pen.translate(button(BTN_LEFT, 1)),
            vec![raw(EV_KEY, BTN_TOUCH, 1)]
        );
        assert_eq!(
            pen.translate(button(syntra_input_event::BTN_RIGHT, 1)),
            vec![raw(EV_KEY, BTN_STYLUS2, 1)]
        );
        assert_eq!(
            pen.translate(button(syntra_input_event::BTN_BACK, 1)),
            Vec::new()
        );
        let leave = pen.leave();
        assert_eq!(leave.last(), Some(&raw(EV_KEY, BTN_TOOL_PEN, 0)));
        assert!(leave.contains(&raw(EV_KEY, BTN_TOUCH, 0)));
        assert!(pen.leave().is_empty(), "leaving twice sends nothing");
    }

    #[test]
    fn pointer_area_override_parses_and_rejects_nonsense() {
        assert_eq!(parse_area("3840x2160"), Some((3840, 2160)));
        assert_eq!(parse_area(" 1920X1080 "), Some((1920, 1080)));
        for bad in ["", "1920", "0x0", "ax b", "1x1"] {
            assert_eq!(parse_area(bad), None, "{bad}");
        }
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<libc::uinput_setup>(), 92);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
    }

    /// Hostile or broken peers must not panic the emulation task or wedge it.
    #[test]
    fn extreme_values_are_bounded_and_do_not_poison_state() {
        let mut state = ScrollAccumulator::default();
        for value in [i32::MIN, i32::MAX, i32::MIN, i32::MAX] {
            for axis in [0, 1] {
                for event in state.translate(Event::Pointer(PointerEvent::AxisDiscrete120 {
                    axis,
                    value,
                })) {
                    assert!(event.value.abs() <= MAX_SCROLL_PER_EVENT);
                }
            }
        }
        assert!(state.hi_res.iter().all(|rest| rest.abs() < WHEEL_DETENT));
        for _ in 0..10_000 {
            state.translate(Event::Pointer(PointerEvent::Axis {
                time: 0,
                axis: 0,
                value: f64::MAX,
            }));
        }
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            state.translate(Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: bad,
                dy: bad,
            }));
        }
        assert_eq!(
            state.translate(Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 1.0,
                dy: 0.0,
            })),
            vec![raw(EV_REL, REL_X, 1)],
            "motion must keep working after invalid input"
        );
    }

    #[test]
    fn fractional_motion_accumulates_instead_of_being_dropped() {
        let motion = |dx| {
            Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx,
                dy: 0.0,
            })
        };
        let events = translate(&[motion(0.4), motion(0.4), motion(0.4)]);
        assert_eq!(events, vec![raw(EV_REL, REL_X, 1)]);
    }

    #[test]
    fn negative_motion_is_symmetric() {
        let events = translate(&[Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: -3.7,
            dy: 2.0,
        })]);
        assert_eq!(events, vec![raw(EV_REL, REL_X, -3), raw(EV_REL, REL_Y, 2)]);
    }

    #[test]
    fn discrete_scroll_emits_hi_res_and_whole_detents_with_evdev_sign() {
        let down = Event::Pointer(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: 120,
        });
        assert_eq!(
            translate(&[down]),
            vec![
                raw(EV_REL, REL_WHEEL_HI_RES, -120),
                raw(EV_REL, REL_WHEEL, -1)
            ]
        );
    }

    #[test]
    fn partial_detents_combine_into_one_click() {
        let half = Event::Pointer(PointerEvent::AxisDiscrete120 { axis: 1, value: 60 });
        assert_eq!(
            translate(&[half, half]),
            vec![
                raw(EV_REL, REL_HWHEEL_HI_RES, 60),
                raw(EV_REL, REL_HWHEEL_HI_RES, 60),
                raw(EV_REL, REL_HWHEEL, 1),
            ]
        );
    }

    #[test]
    fn continuous_scroll_uses_wayland_units_per_detent() {
        let axis = Event::Pointer(PointerEvent::Axis {
            time: 0,
            axis: 0,
            value: 15.0,
        });
        assert_eq!(
            translate(&[axis]),
            vec![
                raw(EV_REL, REL_WHEEL_HI_RES, -120),
                raw(EV_REL, REL_WHEEL, -1)
            ]
        );
    }

    #[test]
    fn keys_and_buttons_map_to_evdev_codes_and_out_of_range_is_ignored() {
        let events = translate(&[
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 1,
            }),
            Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 1,
            }),
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 0x2ff,
                state: 1,
            }),
            Event::Pointer(PointerEvent::Button {
                time: 0,
                button: 0x200,
                state: 1,
            }),
            Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 1,
                latched: 0,
                locked: 0,
                group: 0,
            }),
        ]);
        assert_eq!(
            events,
            vec![raw(EV_KEY, 30, 1), raw(EV_KEY, BTN_LEFT as u16, 1)]
        );
    }

    /// Creates the real device, reads its events back from `/dev/input`, and
    /// measures write-to-read latency. Only net-zero pointer motion is sent,
    /// so running it on a live desktop leaves the cursor where it was.
    ///
    /// `cargo test -p syntra-input-emulation --features uinput -- --ignored --nocapture live_`
    #[tokio::test]
    #[ignore = "needs /dev/uinput and readable /dev/input event nodes"]
    async fn live_round_trip_and_latency() {
        use std::io::Read;
        use std::time::{Duration, Instant};

        let mut emulation = UinputEmulation::new().expect("uinput device");
        let node = {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(node) = event_node_named("Syntra virtual input") {
                    break node;
                }
                assert!(Instant::now() < deadline, "device never appeared");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        // udev applies group permissions shortly after the node appears.
        let mut reader = {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(format!("/dev/input/{node}"))
                {
                    Ok(reader) => break reader,
                    Err(error) if Instant::now() < deadline => {
                        let _ = error;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("event node not readable by the input group: {error}"),
                }
            }
        };
        let size = std::mem::size_of::<libc::input_event>();
        let mut buffer = vec![0u8; size * 64];
        let mut read_motion = |deadline: Instant| -> Option<i32> {
            loop {
                match reader.read(&mut buffer) {
                    Ok(n) => {
                        for chunk in buffer[..n].chunks_exact(size) {
                            // SAFETY: the kernel writes whole input_event records.
                            let event: libc::input_event =
                                unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                            if event.type_ == EV_REL && event.code == REL_X {
                                return Some(event.value);
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            return None;
                        }
                        std::hint::spin_loop();
                    }
                    Err(error) => panic!("read failed: {error}"),
                }
            }
        };

        let mut latencies = Vec::new();
        for step in 0..500 {
            let dx = if step % 2 == 0 { 1.0 } else { -1.0 };
            let started = Instant::now();
            emulation
                .consume(
                    Event::Pointer(PointerEvent::Motion {
                        time: 0,
                        dx,
                        dy: 0.0,
                    }),
                    0,
                )
                .await
                .unwrap();
            let value = read_motion(started + Duration::from_secs(1)).expect("event read back");
            latencies.push(started.elapsed());
            assert_eq!(value, dx as i32);
        }
        latencies.sort();
        let p50 = latencies[latencies.len() / 2];
        let p99 = latencies[latencies.len() * 99 / 100];
        println!("uinput round trip: p50={p50:?} p99={p99:?}");
        assert!(p99 < Duration::from_millis(5), "p99 {p99:?}");

        let started = Instant::now();
        for step in 0..20_000 {
            let dx = if step % 2 == 0 { 1.0 } else { -1.0 };
            emulation
                .consume(
                    Event::Pointer(PointerEvent::Motion {
                        time: 0,
                        dx,
                        dy: 0.0,
                    }),
                    0,
                )
                .await
                .unwrap();
        }
        let per_event = started.elapsed() / 20_000;
        println!("uinput throughput: {per_event:?} per event");
    }

    fn event_node_named(name: &str) -> Option<String> {
        let devices = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
        devices
            .split("\n\n")
            .filter(|block| block.contains(&format!("N: Name=\"{name}\"")))
            .find_map(|block| {
                block
                    .lines()
                    .find(|line| line.starts_with("H: Handlers="))?
                    .split_whitespace()
                    .find(|handler| handler.starts_with("event"))
                    .map(str::to_owned)
            })
    }
}
