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

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use async_trait::async_trait;
use syntra_input_event::{Event, KeyboardEvent, PointerEvent};

use super::{Emulation, EmulationHandle, error::EmulationError};

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
    device: File,
    scroll: ScrollAccumulator,
}

impl UinputEmulation {
    pub(crate) fn new() -> Result<Self, UinputEmulationCreationError> {
        let device = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(DEVICE_PATH)
            .map_err(UinputEmulationCreationError::Open)?;
        configure(&device).map_err(UinputEmulationCreationError::Setup)?;
        Ok(Self {
            device,
            scroll: ScrollAccumulator::default(),
        })
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
        let written =
            unsafe { libc::write(self.device.as_raw_fd(), buffer.as_ptr().cast(), bytes) };
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

impl Drop for UinputEmulation {
    fn drop(&mut self) {
        // SAFETY: plain ioctl on an fd owned by `self`; closing the fd would
        // destroy the device as well, this only makes it explicit.
        unsafe { libc::ioctl(self.device.as_raw_fd(), UI_DEV_DESTROY) };
    }
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

    // SAFETY: uinput_setup is plain old data and all-zero is valid.
    let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
    setup.id.bustype = BUS_VIRTUAL;
    setup.id.vendor = 0x5359; // "SY"
    setup.id.product = 0x0001;
    setup.id.version = 1;
    for (slot, byte) in setup.name.iter_mut().zip(DEVICE_NAME) {
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

impl ScrollAccumulator {
    fn translate(&mut self, event: Event) -> Vec<RawEvent> {
        match event {
            Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => {
                let mut events = Vec::with_capacity(2);
                for (index, (delta, code)) in [(dx, REL_X), (dy, REL_Y)].into_iter().enumerate() {
                    self.motion[index] += delta;
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
    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        let events = self.scroll.translate(event);
        self.write_events(&events)?;
        Ok(())
    }

    // One device serves every peer: key state per peer is tracked by
    // `InputEmulation`, and the kernel merges input like two real mice.
    async fn create(&mut self, _handle: EmulationHandle) {}
    async fn destroy(&mut self, _handle: EmulationHandle) {}
    async fn terminate(&mut self) {}
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
    fn ioctl_numbers_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<libc::uinput_setup>(), 92);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
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
