//! Android host transport and capability-limited in-process controller.
#[cfg(target_os = "android")]
use std::sync::{Arc, Mutex};
use std::{
    collections::HashMap,
    fs, io,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
};

use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, ClipboardSettings, DEFAULT_PORT, FrontendEvent,
    FrontendRequest, Status,
};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "android")]
use crate::bridge::RequestSink;
use crate::bridge::{
    MemoryEventSource, MemoryRequestSink, memory_event_channel, memory_request_channel,
};
use crate::lifecycle::{Capability, HostCapabilities};
#[cfg(target_os = "android")]
use crate::lifecycle::{Lifecycle, LifecycleAction, LifecycleEvent};

pub type AndroidEventSource = MemoryEventSource;
pub type AndroidRequestSink = MemoryRequestSink;

pub struct AndroidHost {
    pub events: AndroidEventSource,
    pub requests: AndroidRequestSink,
    pub capabilities: HostCapabilities,
}

pub struct AndroidHostEndpoint {
    pub events: Sender<FrontendEvent>,
    pub requests: Receiver<FrontendRequest>,
}

impl AndroidHost {
    pub fn channel() -> (Self, AndroidHostEndpoint) {
        let (event_tx, event_rx) = memory_event_channel();
        let (request_tx, request_rx) = memory_request_channel();
        (
            Self {
                events: event_rx,
                requests: request_tx,
                capabilities: HostCapabilities::android_default(),
            },
            AndroidHostEndpoint {
                events: event_tx,
                requests: request_rx,
            },
        )
    }

    pub fn can_capture(&self) -> bool {
        self.capabilities.capture == Capability::Available
    }

    pub fn can_emulate(&self) -> bool {
        self.capabilities.emulation == Capability::Available
    }

    pub fn can_transfer_files(&self) -> bool {
        self.capabilities.clipboard_files == Capability::Available
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedClient {
    handle: ClientHandle,
    config: ClientConfig,
    active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedController {
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    next_handle: ClientHandle,
    #[serde(default)]
    clients: Vec<PersistedClient>,
    #[serde(default)]
    authorized_keys: HashMap<String, String>,
}

const fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for PersistedController {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            next_handle: 1,
            clients: Vec::new(),
            authorized_keys: HashMap::new(),
        }
    }
}

pub struct AndroidHostController {
    path: PathBuf,
    state: PersistedController,
}

impl AndroidHostController {
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let state = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => PersistedController::default(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, state })
    }

    pub fn run(mut self, endpoint: AndroidHostEndpoint) {
        while let Ok(request) = endpoint.requests.recv() {
            let events = self.handle(request);
            if events
                .into_iter()
                .any(|event| endpoint.events.send(event).is_err())
            {
                break;
            }
        }
    }

    fn client(&self, handle: ClientHandle) -> Option<&PersistedClient> {
        self.state
            .clients
            .iter()
            .find(|client| client.handle == handle)
    }

    fn client_mut(&mut self, handle: ClientHandle) -> Option<&mut PersistedClient> {
        self.state
            .clients
            .iter_mut()
            .find(|client| client.handle == handle)
    }

    fn frontend_client(client: &PersistedClient) -> (ClientHandle, ClientConfig, ClientState) {
        let state = ClientState {
            active: client.active,
            ..ClientState::default()
        };
        (client.handle, client.config.clone(), state)
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&self.state)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, &self.path)
    }

    fn persisted(&self, events: &mut Vec<FrontendEvent>) {
        if let Err(error) = self.save() {
            events.push(FrontendEvent::Error(format!(
                "Could not persist Android controller settings: {error}"
            )));
        }
    }

    fn state_event(&self, handle: ClientHandle) -> FrontendEvent {
        match self.client(handle) {
            Some(client) => {
                let (_, config, state) = Self::frontend_client(client);
                FrontendEvent::State(handle, config, state)
            }
            None => FrontendEvent::NoSuchClient(handle),
        }
    }

    fn sync_events(&self) -> Vec<FrontendEvent> {
        vec![
            FrontendEvent::Enumerate(
                self.state
                    .clients
                    .iter()
                    .map(Self::frontend_client)
                    .collect(),
            ),
            FrontendEvent::PortChanged(self.state.port, None),
            FrontendEvent::AuthorizedUpdated(self.state.authorized_keys.clone()),
            FrontendEvent::CaptureStatus(Status::Disabled),
            FrontendEvent::EmulationStatus(Status::Disabled),
            FrontendEvent::InputSharing(false),
            FrontendEvent::ClipboardSettings(ClipboardSettings::default()),
        ]
    }

    fn unavailable(operation: &str) -> FrontendEvent {
        FrontendEvent::Error(format!("{operation} is unavailable on Android"))
    }

    fn handle(&mut self, request: FrontendRequest) -> Vec<FrontendEvent> {
        let mut events = Vec::new();
        match request {
            FrontendRequest::Sync | FrontendRequest::Enumerate() => return self.sync_events(),
            FrontendRequest::Create => {
                let handle = self.state.next_handle.max(1);
                self.state.next_handle = handle.saturating_add(1);
                let client = PersistedClient {
                    handle,
                    config: ClientConfig::default(),
                    active: false,
                };
                events.push({
                    let (_, config, state) = Self::frontend_client(&client);
                    FrontendEvent::Created(handle, config, state)
                });
                self.state.clients.push(client);
                self.persisted(&mut events);
            }
            FrontendRequest::Delete(handle) => {
                let old_len = self.state.clients.len();
                self.state.clients.retain(|client| client.handle != handle);
                if self.state.clients.len() == old_len {
                    events.push(FrontendEvent::NoSuchClient(handle));
                } else {
                    events.push(FrontendEvent::Deleted(handle));
                    self.persisted(&mut events);
                }
            }
            FrontendRequest::Activate(handle, active) => {
                if let Some(client) = self.client_mut(handle) {
                    client.active = active;
                } else {
                    events.push(FrontendEvent::NoSuchClient(handle));
                    return events;
                }
                events.push(self.state_event(handle));
                self.persisted(&mut events);
            }
            FrontendRequest::UpdateHostname(handle, hostname) => {
                if let Some(client) = self.client_mut(handle) {
                    client.config.hostname = hostname;
                } else {
                    events.push(FrontendEvent::NoSuchClient(handle));
                    return events;
                }
                events.push(self.state_event(handle));
                self.persisted(&mut events);
            }
            FrontendRequest::UpdatePort(handle, port) => {
                if let Some(client) = self.client_mut(handle) {
                    client.config.port = port;
                } else {
                    events.push(FrontendEvent::NoSuchClient(handle));
                    return events;
                }
                events.push(self.state_event(handle));
                self.persisted(&mut events);
            }
            FrontendRequest::UpdatePosition(handle, position) => {
                if let Some(client) = self.client_mut(handle) {
                    client.config.pos = position;
                } else {
                    events.push(FrontendEvent::NoSuchClient(handle));
                    return events;
                }
                events.push(self.state_event(handle));
                self.persisted(&mut events);
            }
            FrontendRequest::UpdateFixIps(handle, ips) => {
                if let Some(client) = self.client_mut(handle) {
                    client.config.fix_ips = ips;
                } else {
                    events.push(FrontendEvent::NoSuchClient(handle));
                    return events;
                }
                events.push(self.state_event(handle));
                self.persisted(&mut events);
            }
            FrontendRequest::UpdateEnterHook(handle, command) => {
                if let Some(client) = self.client_mut(handle) {
                    client.config.cmd = command;
                } else {
                    events.push(FrontendEvent::NoSuchClient(handle));
                    return events;
                }
                events.push(self.state_event(handle));
                self.persisted(&mut events);
            }
            FrontendRequest::ChangePort(port) => {
                self.state.port = port;
                events.push(FrontendEvent::PortChanged(port, None));
                self.persisted(&mut events);
            }
            FrontendRequest::AuthorizeKey(description, fingerprint) => {
                self.state.authorized_keys.insert(fingerprint, description);
                events.push(FrontendEvent::AuthorizedUpdated(
                    self.state.authorized_keys.clone(),
                ));
                self.persisted(&mut events);
            }
            FrontendRequest::RemoveAuthorizedKey(fingerprint) => {
                self.state.authorized_keys.remove(&fingerprint);
                events.push(FrontendEvent::AuthorizedUpdated(
                    self.state.authorized_keys.clone(),
                ));
                self.persisted(&mut events);
            }
            FrontendRequest::SaveConfiguration => self.persisted(&mut events),
            FrontendRequest::EnableCapture => {
                events.push(Self::unavailable("Input capture"));
                events.push(FrontendEvent::CaptureStatus(Status::Disabled));
            }
            FrontendRequest::EnableEmulation => {
                events.push(Self::unavailable("Input emulation"));
                events.push(FrontendEvent::EmulationStatus(Status::Disabled));
            }
            FrontendRequest::SetInputSharing(_) => {
                events.push(Self::unavailable("Input sharing"));
                events.push(FrontendEvent::InputSharing(false));
            }
            FrontendRequest::ResolveDns(_) => {
                events.push(Self::unavailable("DNS resolution"));
            }
            FrontendRequest::DiscoverPeers => {
                events.push(Self::unavailable("mDNS discovery"));
            }
            FrontendRequest::StopService => {
                events.push(Self::unavailable("background daemon control"));
            }
            FrontendRequest::RegenerateIdentity | FrontendRequest::SetLocalDeviceProfile(_) => {
                events.push(Self::unavailable("Authenticated device identity"));
            }
            FrontendRequest::SendFiles { .. }
            | FrontendRequest::AcceptFileTransfer { .. }
            | FrontendRequest::DeclineFileTransfer { .. }
            | FrontendRequest::CancelManualTransfer { .. }
            | FrontendRequest::SetFileReceiveSettings(_) => {
                events.push(FrontendEvent::ManualTransferError(
                    "Manual file transfers are unavailable on this platform".into(),
                ));
            }
            FrontendRequest::QueryHistory { .. }
            | FrontendRequest::SetHistoryPinned { .. }
            | FrontendRequest::GetHistoryImage(_)
            | FrontendRequest::ClearGlobalHistory => {
                events.push(FrontendEvent::HistoryError(
                    "Clipboard history is unavailable on this platform".into(),
                ));
            }
            FrontendRequest::SetClipboardText(_)
            | FrontendRequest::SetClipboardImage(_)
            | FrontendRequest::SetClipboardFiles(_)
            | FrontendRequest::CancelClipboardTransfer(_) => {
                events.push(Self::unavailable("Clipboard synchronization"));
                events.push(FrontendEvent::ClipboardSettings(
                    ClipboardSettings::default(),
                ));
            }
        }
        events
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: slint::android::AndroidApp) {
    let Some(data_path) = app.internal_data_path() else {
        log::error!("Android did not provide an internal data directory");
        return;
    };
    let controller = match AndroidHostController::load(data_path.join("controller.json")) {
        Ok(controller) => controller,
        Err(error) => {
            log::error!("Could not load Android controller settings: {error}");
            return;
        }
    };
    let (host, endpoint) = AndroidHost::channel();
    let lifecycle_requests = host.requests.clone();
    let _ = lifecycle_requests.send(FrontendRequest::Sync);
    let lifecycle = Arc::new(Mutex::new(Lifecycle::new(true)));
    let lifecycle_state = Arc::clone(&lifecycle);
    slint::android::init_with_event_listener(app, move |event| {
        use slint::android::android_activity::{MainEvent, PollEvent};
        let lifecycle_event = match event {
            PollEvent::Main(MainEvent::Resume { .. }) => Some(LifecycleEvent::Foregrounded),
            PollEvent::Main(MainEvent::Pause | MainEvent::Stop) => {
                Some(LifecycleEvent::Backgrounded)
            }
            _ => None,
        };
        if let Some(event) = lifecycle_event {
            let Ok(mut lifecycle) = lifecycle_state.lock() else {
                return;
            };
            for action in lifecycle.transition(event) {
                if matches!(action, LifecycleAction::RequestResync) {
                    let _ = lifecycle_requests.send(FrontendRequest::Sync);
                }
            }
        }
    })
    .expect("failed to initialize Slint Android backend");
    std::thread::spawn(move || controller.run(endpoint));
    if let Err(error) = crate::app::run_with_transport(host.events, host.requests) {
        log::error!("Slint Android UI failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lan-mouse-android-{name}-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    #[test]
    fn controller_persists_supported_configuration() {
        let path = test_path("persistence");
        let _ = fs::remove_file(&path);
        let mut controller = AndroidHostController::load(&path).unwrap();
        assert!(matches!(
            controller.handle(FrontendRequest::Create).first(),
            Some(FrontendEvent::Created(1, _, _))
        ));
        controller.handle(FrontendRequest::UpdateHostname(1, Some("tablet".into())));
        controller.handle(FrontendRequest::Activate(1, true));
        drop(controller);

        let controller = AndroidHostController::load(&path).unwrap();
        let clients = match controller.sync_events().remove(0) {
            FrontendEvent::Enumerate(clients) => clients,
            event => panic!("unexpected event: {event:?}"),
        };
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].1.hostname.as_deref(), Some("tablet"));
        assert!(clients[0].2.active);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unsupported_runtime_capabilities_are_explicit() {
        let path = test_path("capabilities");
        let mut controller = AndroidHostController::load(&path).unwrap();
        let capture = controller.handle(FrontendRequest::EnableCapture);
        assert!(matches!(
            capture.as_slice(),
            [
                FrontendEvent::Error(_),
                FrontendEvent::CaptureStatus(Status::Disabled)
            ]
        ));
        let input = controller.handle(FrontendRequest::SetInputSharing(true));
        assert!(matches!(
            input.as_slice(),
            [FrontendEvent::Error(_), FrontendEvent::InputSharing(false)]
        ));
        let clipboard = controller.handle(FrontendRequest::SetClipboardFiles(true));
        let [
            FrontendEvent::Error(_),
            FrontendEvent::ClipboardSettings(settings),
        ] = clipboard.as_slice()
        else {
            panic!("unexpected events: {clipboard:?}");
        };
        assert_eq!(settings, &ClipboardSettings::default());
    }
}
