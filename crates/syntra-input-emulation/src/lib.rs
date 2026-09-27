//! Replaying pointer and keyboard input on the host.
//!
//! The [`Emulation`] trait is the backend contract: `consume` applies an
//! event, `create` and `destroy` manage per-peer virtual devices, and
//! `terminate` shuts a backend down.
//!
//! Every backend tracks pressed keys and releases them on disconnect. A peer
//! that vanishes mid-keystroke must not leave a key held down locally, so
//! this is a correctness requirement rather than tidiness.

use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use syntra_input_event::{Event, KeyboardEvent};
use tokio::sync::mpsc;

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

#[cfg(wlroots)]
mod wlroots;

#[cfg(rdp)]
mod xdg_desktop_portal;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

/// fallback input emulation (logs events)
mod dummy;
mod error;

pub type EmulationHandle = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(wlroots)]
    Wlroots,
    #[cfg(libei)]
    Libei,
    #[cfg(rdp)]
    Xdp,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots)]
            Backend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei)]
            Backend::Libei => write!(f, "libei"),
            #[cfg(rdp)]
            Backend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "macos"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    /// Which backend `emulation` is, so callers can report it.
    selected: Backend,
    handles: HashSet<EmulationHandle>,
    pressed_keys: HashMap<EmulationHandle, HashSet<u32>>,
    clipboard_rx: Option<mpsc::Receiver<(String, Vec<u8>)>>,
}

impl InputEmulation {
    async fn with_backend(backend: Backend) -> Result<InputEmulation, EmulationCreationError> {
        let mut emulation: Box<dyn Emulation> = match backend {
            #[cfg(wlroots)]
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new()?),
            #[cfg(libei)]
            Backend::Libei => Box::new(libei::LibeiEmulation::new().await?),
            #[cfg(x11)]
            Backend::X11 => Box::new(x11::X11Emulation::new()?),
            #[cfg(rdp)]
            Backend::Xdp => Box::new(xdg_desktop_portal::DesktopPortalEmulation::new().await?),
            #[cfg(windows)]
            Backend::Windows => Box::new(windows::WindowsEmulation::new()?),
            #[cfg(target_os = "macos")]
            Backend::MacOs => Box::new(macos::MacOSEmulation::new().await?),
            Backend::Dummy => Box::new(dummy::DummyEmulation::new()),
        };
        let clipboard_rx = emulation.take_clipboard_receiver();
        Ok(Self {
            emulation,
            selected: backend,
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            clipboard_rx,
        })
    }

    pub async fn new(backend: Option<Backend>) -> Result<InputEmulation, EmulationCreationError> {
        if let Some(backend) = backend {
            let b = Self::with_backend(backend).await;
            if b.is_ok() {
                log::info!("using emulation backend: {backend}");
            }
            return b;
        }

        for backend in [
            #[cfg(wlroots)]
            Backend::Wlroots,
            #[cfg(libei)]
            Backend::Libei,
            #[cfg(rdp)]
            Backend::Xdp,
            #[cfg(x11)]
            Backend::X11,
            #[cfg(windows)]
            Backend::Windows,
            #[cfg(target_os = "macos")]
            Backend::MacOs,
            Backend::Dummy,
        ] {
            match Self::with_backend(backend).await {
                Ok(b) => {
                    log::info!("using emulation backend: {backend}");
                    return Ok(b);
                }
                Err(e) if e.cancelled_by_user() => return Err(e),
                Err(e) => log::warn!("{e}"),
            }
        }

        Err(EmulationCreationError::NoAvailableBackend)
    }

    /// Backend that was actually selected.
    ///
    /// Which backend won the priority order decides what the user must grant
    /// permission for, so an interface has to be able to show it.
    pub fn backend(&self) -> Backend {
        self.selected
    }

    /// Whether the backend transport is still able to consume input.
    pub fn healthy(&self) -> bool {
        self.emulation.healthy()
    }

    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                // prevent double pressed / released keys
                if self.update_pressed_keys(handle, key, state) {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            _ => self.emulation.consume(event, handle).await,
        }
    }

    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.handles.insert(handle) {
            self.pressed_keys.insert(handle, HashSet::new());
            self.emulation.create(handle).await;
            true
        } else {
            false
        }
    }

    pub async fn destroy(&mut self, handle: EmulationHandle) {
        if let Err(error) = self.release_keys(handle).await {
            log::warn!("could not fully release keys for handle {handle}: {error}");
        }
        if self.handles.remove(&handle) {
            self.pressed_keys.remove(&handle);
            self.emulation.destroy(handle).await
        }
    }

    pub async fn terminate(&mut self) {
        for handle in self.handles.iter().cloned().collect::<Vec<_>>() {
            self.destroy(handle).await
        }
        self.emulation.terminate().await
    }

    pub async fn release_keys(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        let mut release_result = Ok(());
        if let Some(keys) = self.pressed_keys.get_mut(&handle) {
            let keys = keys.drain().collect::<Vec<_>>();
            for key in keys {
                let event = Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state: 0,
                });
                if let Err(error) = self.emulation.consume(event, handle).await {
                    if release_result.is_ok() {
                        release_result = Err(error);
                    }
                }
                if let Ok(key) = syntra_input_event::scancode::Linux::try_from(key) {
                    log::warn!("releasing stuck key: {key:?}");
                }
            }
        }

        let event = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        let modifiers_result = self.emulation.consume(event, handle).await;
        release_result.and(modifiers_result)
    }
    pub async fn clipboard_event(&mut self) -> Option<(String, Vec<u8>)> {
        match self.clipboard_rx.as_mut() {
            Some(receiver) => receiver.recv().await,
            None => std::future::pending().await,
        }
    }
    pub async fn set_file_clipboard(
        &mut self,
        contents: Vec<(String, Vec<u8>)>,
    ) -> Result<(), EmulationError> {
        self.emulation.set_file_clipboard(contents).await
    }

    pub fn has_pressed_keys(&self, handle: EmulationHandle) -> bool {
        self.pressed_keys
            .get(&handle)
            .is_some_and(|p| !p.is_empty())
    }

    /// update the pressed_keys for the given handle
    /// returns whether the event should be processed
    fn update_pressed_keys(&mut self, handle: EmulationHandle, key: u32, state: u8) -> bool {
        let Some(pressed_keys) = self.pressed_keys.get_mut(&handle) else {
            return false;
        };

        if state == 0 {
            // currently pressed => can release
            pressed_keys.remove(&key)
        } else {
            // currently not pressed => can press
            pressed_keys.insert(key)
        }
    }
}

#[async_trait]
trait Emulation: Send {
    fn healthy(&self) -> bool {
        true
    }
    fn take_clipboard_receiver(&mut self) -> Option<mpsc::Receiver<(String, Vec<u8>)>> {
        None
    }
    async fn set_file_clipboard(
        &mut self,
        _contents: Vec<(String, Vec<u8>)>,
    ) -> Result<(), EmulationError> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "clipboard publishing is unavailable for this backend",
        )
        .into())
    }

    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError>;
    async fn create(&mut self, handle: EmulationHandle);
    async fn destroy(&mut self, handle: EmulationHandle);
    async fn terminate(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct FailingRelease {
        events: Arc<Mutex<Vec<Event>>>,
    }

    #[async_trait]
    impl Emulation for FailingRelease {
        async fn consume(
            &mut self,
            event: Event,
            _handle: EmulationHandle,
        ) -> Result<(), EmulationError> {
            let mut events = self.events.lock().await;
            events.push(event);
            if events.len() == 1 {
                Err(std::io::Error::other("first release failed").into())
            } else {
                Ok(())
            }
        }

        async fn create(&mut self, _handle: EmulationHandle) {}
        async fn destroy(&mut self, _handle: EmulationHandle) {}
        async fn terminate(&mut self) {}
    }

    #[tokio::test]
    async fn release_attempts_every_key_and_modifiers_after_an_error() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut emulation = InputEmulation {
            emulation: Box::new(FailingRelease {
                events: Arc::clone(&events),
            }),
            selected: Backend::Dummy,
            handles: HashSet::from([1]),
            pressed_keys: HashMap::from([(1, HashSet::from([29, 42]))]),
            clipboard_rx: None,
        };

        let error = emulation.release_keys(1).await.unwrap_err();
        assert!(error.to_string().contains("first release failed"));
        let events = events.lock().await;
        let released = events
            .iter()
            .filter_map(|event| match event {
                Event::Keyboard(KeyboardEvent::Key { key, state: 0, .. }) => Some(*key),
                _ => None,
            })
            .collect::<HashSet<_>>();
        assert_eq!(released, HashSet::from([29, 42]));
        assert!(matches!(
            events.last(),
            Some(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }))
        ));
    }
}
