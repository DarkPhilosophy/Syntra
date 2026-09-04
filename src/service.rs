use crate::{
    adapter_manager::{
        AdapterId as ProcessAdapterId, AdapterPaths, AdapterProcessManager, ManagerCommand,
        ManagerEvent,
    },
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    clipboard::{Clipboard, ClipboardContent},
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    file_transfer::{FileOffer, OutgoingFile, TransferError},
    listen::{LanMouseListener, ListenerCreationError},
    transfer_manager::{TransferAction, TransferFrontendEvent, TransferOwner, TransferState},
};
use futures::StreamExt;
use lan_mouse_adapter_api::{Message as AdapterMessage, Operation, PublishFileClipboard, Released};
use lan_mouse_ipc::{
    AsyncFrontendListener, ClientHandle, ClipboardSettings, ClipboardTransferDirection,
    ClipboardTransferState, ClipboardTransferStatus, FrontendEvent, FrontendRequest, IpcError,
    IpcListenerCreationError, Position, Status,
};
use log;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, RwLock},
};
use thiserror::Error;
use tokio::{
    process::Command,
    signal,
    sync::{Notify, mpsc},
};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
    #[error(transparent)]
    Adapter(#[from] crate::adapter_manager::ManagerError),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// clipboard capability settings
    clipboard_settings: ClipboardSettings,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    clipboard: Clipboard,
    next_clipboard_transfer: u64,
    next_trigger_handle: u64,
    adapter_manager: Option<AdapterProcessManager>,
    adapter_events: mpsc::Receiver<ManagerEvent>,
    transfers: TransferState<Peer>,
    source_result_tx: mpsc::Sender<SourceReadResult>,
    source_results: mpsc::Receiver<SourceReadResult>,
    gtk_ready: bool,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

const SOURCE_RESULT_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Peer {
    Capture(ClientHandle),
    Emulation(SocketAddr),
}

struct SourceReadResult {
    owner: TransferOwner<Peer>,
    file_id: u64,
    request_id: u64,
    offset: u64,
    length: u64,
    result: Result<(OutgoingFile, Option<(u64, Vec<u8>)>, Option<[u8; 32]>), TransferError>,
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        // listener + connection
        let listener =
            LanMouseListener::new(config.port(), cert.clone(), authorized_keys.clone()).await?;
        let conn = LanMouseConnection::new(cert.clone(), client_manager.clone());

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(capture_backend, conn, config.release_bind());
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);
        let clipboard = Clipboard::new();
        let executable_dir = std::env::current_exe()?
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "service executable has no parent directory",
                )
            })?;
        let adapter_paths = AdapterPaths::new(
            executable_dir.join("lan-mouse-adapter-gtk-clipboard"),
            executable_dir.join("lan-mouse-adapter-fuse"),
        )?;
        let (adapter_manager, adapter_events) = AdapterProcessManager::start(adapter_paths);
        let (source_result_tx, source_results) = mpsc::channel(SOURCE_RESULT_CAPACITY);

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();
        let clipboard_settings = config.clipboard_settings();
        let service = Self {
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            public_key_fingerprint,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            clipboard_settings,
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            clipboard,
            next_clipboard_transfer: 0,
            next_trigger_handle: 0,
            adapter_manager: Some(adapter_manager),
            adapter_events,
            transfers: TransferState::new(),
            source_result_tx,
            source_results,
            gtk_ready: false,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        loop {
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.adapter_events.recv() => {
                    if let Some(event) = event {
                        self.handle_adapter_event(event);
                    }
                },
                result = self.source_results.recv() => {
                    if let Some(result) = result {
                        self.handle_source_result(result);
                    }
                },
                result = self.clipboard.next_read() => match result {
                    Ok(ClipboardContent::Text(text)) if self.clipboard_settings.text => {
                        for handle in self.client_manager.active_clients() {
                            self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1);
                            self.capture.send_clipboard(handle, self.next_clipboard_transfer, text.as_bytes().to_vec(), None);
                        }
                    }
                    Ok(ClipboardContent::Image { width, height, rgba }) if self.clipboard_settings.image => {
                        for handle in self.client_manager.active_clients() {
                            self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1);
                            self.capture.send_clipboard(handle, self.next_clipboard_transfer, rgba.clone(), Some((width, height)));
                        }
                    }
                    Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
                    Err(e) => log::warn!("failed to read clipboard: {e}"),
                },
                event = self.resolver.event() => self.handle_resolver_event(event),
                _ = self.config.changed() => self.handle_config_change(),
                r = signal::ctrl_c() => break r.expect("failed to wait for CTRL+C"),
            }
        }

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;
        log::debug!("terminating file adapters ...");
        if let Some(adapter_manager) = self.adapter_manager.take() {
            adapter_manager.shutdown().await?;
        }

        Ok(())
    }

    fn handle_frontend_request(&mut self, request: Option<Result<FrontendRequest, IpcError>>) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook)
            }
            FrontendRequest::SetClipboardText(value) => {
                self.clipboard_settings.text = value;
                self.config.set_clipboard_settings(self.clipboard_settings);
                self.save_config();
                self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
            }
            FrontendRequest::SetClipboardImage(value) => {
                self.clipboard_settings.image = value;
                self.config.set_clipboard_settings(self.clipboard_settings);
                self.save_config();
                self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
            }
            FrontendRequest::SetClipboardFiles(value) => {
                self.clipboard_settings.files = value;
                self.config.set_clipboard_settings(self.clipboard_settings);
                self.save_config();
                self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
            }
            FrontendRequest::CancelClipboardTransfer(transfer_id) => {
                log::info!("cancelling clipboard transfer {transfer_id}");
                match self.transfers.cancel_ui(
                    transfer_id,
                    None,
                    lan_mouse_proto::ClipboardCancelReason::User,
                ) {
                    Ok(actions) => self.execute_transfer_actions(actions),
                    Err(error) => log::warn!("cannot cancel transfer {transfer_id}: {error}"),
                }
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
        }
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.config.set_port(port);
                    self.save_config();
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => self.capture.release(),
            EmulationEvent::Connected { addr, fingerprint } => {
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
            EmulationEvent::Clipboard(content) => self.clipboard.write(content),
            EmulationEvent::ClipboardProtocol { addr, event } => {
                self.handle_file_protocol(Peer::Emulation(addr), event);
            }
        }
    }

    fn handle_file_protocol(&mut self, peer: Peer, event: lan_mouse_proto::ProtoEvent) {
        use lan_mouse_proto::ProtoEvent;
        match event {
            ProtoEvent::ClipboardManifest {
                transfer_id,
                entries,
            } => {
                let adapter_id = transfer_id.to_string();
                match self.transfers.inbound_manifest(
                    peer,
                    adapter_id,
                    transfer_id.to_string(),
                    transfer_id,
                    lan_mouse_adapter_api::Operation::Copy,
                    entries,
                ) {
                    Ok(actions) => self.execute_transfer_actions(actions),
                    Err(error) => log::warn!("file manifest rejected for {peer:?}: {error}"),
                }
            }
            event => match self.transfers.handle_protocol(&peer, event) {
                Ok(actions) => self.execute_transfer_actions(actions),
                Err(error) => log::warn!("file protocol rejected for {peer:?}: {error}"),
            },
        }
    }
    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::Clipboard { handle, event } => {
                self.handle_file_protocol(Peer::Capture(handle), event);
            }
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
                self.emulation.set_capture_ready(true);
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.spawn_hook_command(handle);
                self.clipboard.read_once();
            }
        }
    }
    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture.create(handle, pos, CaptureType::EnterOnly);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn spawn_hook_command(&self, handle: ClientHandle) {
        let Some(cmd) = self.client_manager.get_enter_cmd(handle) else {
            return;
        };
        tokio::task::spawn_local(async move {
            log::info!("spawning command!");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute cmd: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{cmd} exited successfully");
                    } else {
                        log::warn!("{cmd} exited with {s}");
                    }
                }
                Err(e) => log::warn!("{cmd}: {e}"),
            }
        });
    }
    fn handle_adapter_event(&mut self, event: ManagerEvent) {
        match event {
            ManagerEvent::Started(_) | ManagerEvent::Ready(_) => {}
            ManagerEvent::Message { adapter, message } => {
                let adapter_id = match &adapter {
                    ProcessAdapterId::Gtk => "gtk-clipboard".to_owned(),
                    ProcessAdapterId::Fuse { transfer_id } => transfer_id.clone(),
                };
                let actions = match message {
                    AdapterMessage::RangeRequest(request) => {
                        let transfer_id = request.transfer_id.clone();
                        let request_id = request.request_id;
                        let offset = request.offset;
                        match self.transfers.adapter_range_request(&adapter_id, request) {
                            Ok(actions) => Ok(actions),
                            Err(error) => {
                                if let Some(manager) = self.adapter_manager.as_ref() {
                                    let _ = manager.try_send(ManagerCommand::RangeResponse(
                                        lan_mouse_adapter_api::RangeResponse {
                                            transfer_id,
                                            request_id,
                                            offset,
                                            data_base64: String::new(),
                                            eof: true,
                                            error: Some(error.to_string()),
                                        },
                                    ));
                                }
                                Err(error)
                            }
                        }
                    }
                    AdapterMessage::RemoteManifest(_) => {
                        log::warn!(
                            "adapter {adapter_id} sent an unexpected remote manifest; \
                             cancelling its owned transfers"
                        );
                        Ok(self.transfers.adapter_lost(&adapter_id))
                    }
                    AdapterMessage::MountReady(ready) => {
                        self.transfers.adapter_mount_ready(&adapter_id, ready)
                    }
                    AdapterMessage::Progress(progress) => {
                        self.transfers.adapter_progress(&adapter_id, progress)
                    }
                    AdapterMessage::Completed(completion) => {
                        self.transfers.adapter_completed(&adapter_id, completion)
                    }
                    AdapterMessage::Cancelled(cancelled) => {
                        self.transfers.adapter_cancelled(&adapter_id, cancelled)
                    }
                    AdapterMessage::Released(released) => self.transfers.adapter_released(released),
                    AdapterMessage::Unmounted(unmounted) => {
                        self.transfers.adapter_unmounted(&adapter_id, unmounted)
                    }
                    AdapterMessage::CopyManifest(manifest) => {
                        if matches!(manifest.operation, Operation::Move) {
                            log::debug!("rejecting unsupported move clipboard manifest");
                            Ok(Vec::new())
                        } else {
                            let uri_text = manifest
                                .entries
                                .iter()
                                .filter(|entry| {
                                    !matches!(entry.kind, lan_mouse_adapter_api::EntryKind::Other)
                                })
                                .map(|entry| entry.uri.as_str())
                                .collect::<Vec<_>>()
                                .join("\r\n");
                            let mut actions = Vec::new();
                            for handle in self.client_manager.active_clients() {
                                self.next_clipboard_transfer =
                                    self.next_clipboard_transfer.wrapping_add(1).max(1);
                                let wire_id = self.next_clipboard_transfer;
                                match crate::file_transfer::FileOffer::from_uri_list(
                                    wire_id, &uri_text,
                                ) {
                                    Ok(offer) => match self.transfers.local_copy_manifest(
                                        Peer::Capture(handle),
                                        wire_id,
                                        adapter_id.clone(),
                                        manifest.clone(),
                                        offer,
                                    ) {
                                        Ok(mut peer_actions) => actions.append(&mut peer_actions),
                                        Err(error) => log::warn!(
                                            "clipboard manifest for {handle} rejected: {error}"
                                        ),
                                    },
                                    Err(error) => {
                                        log::warn!("clipboard manifest rejected: {error}")
                                    }
                                }
                            }
                            Ok(actions)
                        }
                    }
                    AdapterMessage::Hello { .. }
                    | AdapterMessage::Error { .. }
                    | AdapterMessage::PasteDestination(_)
                    | AdapterMessage::RangeResponse(_)
                    | AdapterMessage::PublishFileClipboard(_)
                    | AdapterMessage::Cancel { .. }
                    | AdapterMessage::ClipboardData { .. } => {
                        log::debug!("adapter {:?} message not handled by service", adapter);
                        Ok(Vec::new())
                    }
                };
                match actions {
                    Ok(actions) => self.execute_transfer_actions(actions),
                    Err(error) => log::warn!("adapter {:?} event rejected: {error}", adapter),
                }
            }
            ManagerEvent::Cancelled {
                adapter,
                transfer_id,
                ..
            } => {
                let adapter_id = match &adapter {
                    ProcessAdapterId::Gtk => "gtk-clipboard".to_owned(),
                    ProcessAdapterId::Fuse { transfer_id } => transfer_id.clone(),
                };
                if let Ok(actions) = self.transfers.adapter_cancelled(
                    &adapter_id,
                    lan_mouse_adapter_api::Cancelled { transfer_id },
                ) {
                    self.execute_transfer_actions(actions);
                }
            }
            ManagerEvent::Rejected { adapter, reason } => {
                log::warn!("adapter {:?} rejected transfer: {}", adapter, reason)
            }
            ManagerEvent::Exited { adapter, status } => {
                let id = match adapter {
                    ProcessAdapterId::Gtk => "gtk-clipboard".to_owned(),
                    ProcessAdapterId::Fuse { transfer_id } => transfer_id,
                };
                let actions = self.transfers.adapter_lost(&id);
                self.execute_transfer_actions(actions);
                log::warn!("adapter exited: {}", status);
            }
        }
    }

    fn handle_source_result(&mut self, result: SourceReadResult) {
        match result.result {
            Ok((file, Some((offset, data)), digest)) => {
                if let Err(error) = self
                    .transfers
                    .insert_outgoing_file(
                        &result.owner,
                        result.file_id,
                        result.request_id,
                        result.offset,
                        result.length,
                        file,
                    )
                    .and_then(|_| {
                        self.transfers
                            .source_chunk(
                                &result.owner,
                                result.file_id,
                                result.request_id,
                                offset,
                                data,
                            )
                            .map(|actions| {
                                self.execute_transfer_actions(actions);
                                ()
                            })
                    })
                {
                    log::warn!("source read failed: {error}");
                    return;
                }
                if let Some(digest) = digest {
                    match self.transfers.source_complete(
                        &result.owner,
                        result.file_id,
                        result.request_id,
                        result.offset + result.length,
                        digest,
                    ) {
                        Ok(actions) => self.execute_transfer_actions(actions),
                        Err(error) => log::warn!("source completion failed: {error}"),
                    }
                }
            }
            Ok(_) => log::warn!("source read returned no data"),
            Err(error) => log::warn!(
                "source read failed for {:?}/{}: {}",
                result.owner,
                result.file_id,
                error
            ),
        }
    }

    fn notify_transfer_frontend(&mut self, event: TransferFrontendEvent<Peer>) {
        match event {
            TransferFrontendEvent::Progress {
                ui_id,
                file_id,
                completed,
                total,
                ..
            } => {
                self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                    ClipboardTransferStatus {
                        transfer_id: ui_id,
                        file_id,
                        name: String::new(),
                        direction: ClipboardTransferDirection::Receiving,
                        transferred_bytes: completed,
                        total_bytes: total,
                        bytes_per_second: 0,
                        state: ClipboardTransferState::Transferring,
                    },
                ));
            }
            TransferFrontendEvent::Completed { ui_id, file_id, .. } => self.notify_frontend(
                FrontendEvent::ClipboardTransferStatus(ClipboardTransferStatus {
                    transfer_id: ui_id,
                    file_id: file_id.unwrap_or_default(),
                    name: String::new(),
                    direction: ClipboardTransferDirection::Receiving,
                    transferred_bytes: 0,
                    total_bytes: 0,
                    bytes_per_second: 0,
                    state: ClipboardTransferState::Completed,
                }),
            ),
            TransferFrontendEvent::Cancelled { ui_id, file_id, .. } => self.notify_frontend(
                FrontendEvent::ClipboardTransferStatus(ClipboardTransferStatus {
                    transfer_id: ui_id,
                    file_id: file_id.unwrap_or_default(),
                    name: String::new(),
                    direction: ClipboardTransferDirection::Receiving,
                    transferred_bytes: 0,
                    total_bytes: 0,
                    bytes_per_second: 0,
                    state: ClipboardTransferState::Cancelled,
                }),
            ),
            TransferFrontendEvent::Failed {
                ui_id,
                file_id,
                error,
                ..
            } => self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                ClipboardTransferStatus {
                    transfer_id: ui_id,
                    file_id: file_id.unwrap_or_default(),
                    name: String::new(),
                    direction: ClipboardTransferDirection::Receiving,
                    transferred_bytes: 0,
                    total_bytes: 0,
                    bytes_per_second: 0,
                    state: ClipboardTransferState::Failed(error),
                },
            )),
            TransferFrontendEvent::Offered { .. } => {}
        }
    }

    fn execute_transfer_actions(&mut self, actions: Vec<TransferAction<Peer>>) {
        for action in actions {
            match action {
                TransferAction::Peer { peer, event } => match peer {
                    Peer::Capture(handle) => self.capture.send_proto(handle, event),
                    Peer::Emulation(addr) => self.emulation.send_proto(addr, event),
                },
                TransferAction::Adapter {
                    adapter_id,
                    message,
                } => {
                    let command = match message {
                        AdapterMessage::RemoteManifest(m) => ManagerCommand::RemoteManifest(m),
                        AdapterMessage::RangeResponse(m) => ManagerCommand::RangeResponse(m),
                        AdapterMessage::PublishFileClipboard(m) => {
                            ManagerCommand::PublishFileClipboard(m)
                        }
                        AdapterMessage::Released(m) => ManagerCommand::Released(m),
                        AdapterMessage::Unmounted(m) => ManagerCommand::Unmounted(m),
                        AdapterMessage::Cancel { transfer_id } => {
                            let adapter = if adapter_id == "gtk-clipboard" {
                                ProcessAdapterId::Gtk
                            } else {
                                ProcessAdapterId::Fuse {
                                    transfer_id: adapter_id.clone(),
                                }
                            };
                            ManagerCommand::Cancel {
                                adapter,
                                transfer_id,
                            }
                        }
                        _ => continue,
                    };
                    if let Some(manager) = self.adapter_manager.as_ref() {
                        let _ = manager.try_send(command);
                    }
                }
                TransferAction::Frontend(event) => self.notify_transfer_frontend(event),
                TransferAction::ReadSource {
                    owner,
                    file_id,
                    request_id,
                    offset,
                    length,
                    offer,
                } => {
                    let tx = self.source_result_tx.clone();
                    tokio::task::spawn_local(async move {
                        let result = async {
                            let mut file = offer.open(file_id, offset).await?;
                            let mut hasher = Sha256::new();
                            let chunk = file.next_chunk_bounded(length as usize).await?;
                            if let Some((_, ref data)) = chunk {
                                hasher.update(data);
                            }
                            let digest = file.is_eof().then(|| hasher.finalize().into());
                            Ok((file, chunk, digest))
                        }
                        .await;
                        let _ = tx
                            .send(SourceReadResult {
                                owner,
                                file_id,
                                request_id,
                                offset,
                                length,
                                result,
                            })
                            .await;
                    });
                }
            }
        }
    }
}
