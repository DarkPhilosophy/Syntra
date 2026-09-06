use crate::{
    adapter_manager::{
        AdapterId as ProcessAdapterId, AdapterPaths, AdapterProcessManager, ManagerCommand,
        ManagerEvent,
    },
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    clipboard::{Clipboard, ClipboardContent},
    config::{Config, ConfigClient, EmulationBackend},
    connect::LanMouseConnection,
    crypto,
    discovery::{Discovery, DiscoveryEvent},
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    file_transfer::{FileOffer, OutgoingFile, TransferError},
    history, history_sync,
    listen::{LanMouseListener, ListenerCreationError},
    manual_transfer::{ManualAction, ManualTransfers},
    peer_profile,
    transfer_manager::{TransferAction, TransferFrontendEvent, TransferOwner, TransferState},
};
use futures::StreamExt;
use lan_mouse_adapter_api::{Message as AdapterMessage, Operation, PublishFileClipboard, Released};
use lan_mouse_history::{ImportedHistoryEvent, worker::HistoryWorker};
use lan_mouse_ipc::{
    AsyncFrontendListener, ClientHandle, ClipboardSettings, ClipboardTransferDirection,
    ClipboardTransferState, ClipboardTransferStatus, DeviceProfile, FrontendEvent, FrontendRequest,
    IpcError, IpcListenerCreationError, Position, Status,
};
use log;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    #[error(transparent)]
    History(#[from] lan_mouse_history::HistoryError),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// hostname resolver
    resolver: DnsResolver,
    /// mDNS discovery
    discovery: Option<Discovery>,
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
    /// Local identity profile published to authenticated peers.
    local_device_profile: DeviceProfile,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// authoritative global input routing switch
    input_sharing: bool,
    /// clipboard capability settings
    clipboard_settings: ClipboardSettings,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    clipboard: Clipboard,
    history: HistoryWorker,
    /// Certificate identities for authenticated listener-side peers.
    authenticated_peer_fingerprints: HashMap<SocketAddr, String>,
    authenticated_capture_fingerprints: HashMap<ClientHandle, String>,
    profile_reassembler: peer_profile::Reassembler,
    peer_profiles: HashMap<String, DeviceProfile>,
    profile_requests: HashMap<String, (u64, u8, std::time::Instant)>,
    history_reassembler: history_sync::Reassembler<Peer>,
    history_clear: Option<history_sync::ClearCoordinator<String>>,
    history_connected_peers: HashSet<Peer>,
    next_history_request: u64,
    /// Current reconciliation page and bounded retry count for each authenticated peer.
    history_sync_progress: HashMap<Peer, (u64, u64, u8)>,
    /// Values just written from the network and expected to echo through a portal observer.
    pending_remote_history_echoes: VecDeque<lan_mouse_history::HistoryContent>,
    /// The RemoteDesktop portal owns clipboard observation for libei/xdp.
    /// Do not probe Wayland through arboard when that native path is selected.
    legacy_clipboard: bool,
    next_clipboard_transfer: u64,
    next_trigger_handle: u64,
    adapter_manager: Option<AdapterProcessManager>,
    adapter_events: mpsc::Receiver<ManagerEvent>,
    transfers: TransferState<Peer>,
    manual_transfers: ManualTransfers<Peer>,
    file_receive_settings: lan_mouse_ipc::FileReceiveSettings,
    source_result_tx: mpsc::Sender<SourceReadResult>,
    source_results: mpsc::Receiver<SourceReadResult>,
    /// UI transfer ids that actually moved bytes and therefore exist in the UI
    announced_transfers: HashSet<lan_mouse_ipc::ClipboardTransferId>,
    /// Last progress snapshot used for accurate terminal status.
    transfer_progress: HashMap<lan_mouse_ipc::ClipboardTransferId, (u64, u64)>,
    /// Last local file selection. Both portal sessions can report the same
    /// owner change; it must create one offer, not an A↔B echo storm.
    last_file_clipboard: Option<String>,
    /// The native portal currently exposes a file selection. Ignore the
    /// legacy arboard text view of its URI list so it cannot replace the
    /// remote file offer with plain text.
    native_file_selection: bool,
    gtk_ready: bool,
    /// automated copy/paste end-to-end check requested
    e2e: bool,
    /// the automated copy has not been injected yet
    e2e_pending: bool,
    /// Absolute deadline survives other busy select branches.
    e2e_check_at: tokio::time::Instant,
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
        let history = HistoryWorker::start(
            history::database_path(config.app_data_dir()),
            public_key_fingerprint.clone(),
            None,
        )?;

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
        let legacy_clipboard = !match config.emulation_backend() {
            #[cfg(libei_emulation)]
            Some(EmulationBackend::Libei) => true,
            #[cfg(rdp_emulation)]
            Some(EmulationBackend::Xdp) => true,
            _ => false,
        };
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
        let discovery_name = hostname::get()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "Syntra".into());
        let discovery =
            match Discovery::start(config.port(), &public_key_fingerprint, &discovery_name) {
                Ok(d) => Some(d),
                Err(e) => {
                    log::warn!("mDNS discovery unavailable: {e}");
                    None
                }
            };
        let port = config.port();
        let clipboard_settings = config.clipboard_settings();
        let file_receive_settings = config.file_receive_settings();
        let e2e_pending = config.test_copyfile_e2e();
        let mut service = Self {
            config,
            capture,
            emulation,
            resolver,
            discovery,
            frontend_listener,
            authorized_keys,
            public_key_fingerprint,
            local_device_profile: DeviceProfile {
                display_name: hostname::get()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "Syntra".into()),
                avatar: None,
            },
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            input_sharing: true,
            clipboard_settings,
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            clipboard,
            history,
            authenticated_peer_fingerprints: HashMap::new(),
            authenticated_capture_fingerprints: HashMap::new(),
            profile_reassembler: Default::default(),
            peer_profiles: HashMap::new(),
            profile_requests: HashMap::new(),
            history_reassembler: Default::default(),
            history_clear: None,
            history_connected_peers: Default::default(),
            history_sync_progress: HashMap::new(),
            next_history_request: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            pending_remote_history_echoes: VecDeque::new(),
            legacy_clipboard,
            // Wire transfer IDs must remain unique across process restarts.
            // Reusing 1, 2, … lets a peer's still-live clipboard offer reject
            // the next process's manifest as a duplicate/stale transfer.
            next_clipboard_transfer: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
                ^ u64::from(std::process::id()),
            next_trigger_handle: 0,
            adapter_manager: Some(adapter_manager),
            adapter_events,
            transfers: TransferState::new(),
            native_file_selection: false,
            source_result_tx,
            source_results,
            gtk_ready: false,
            last_file_clipboard: None,
            announced_transfers: HashSet::new(),
            manual_transfers: ManualTransfers::new(),
            file_receive_settings,
            transfer_progress: HashMap::new(),
            e2e: e2e_pending,
            e2e_pending,
            e2e_check_at: tokio::time::Instant::now() + Duration::from_secs(2),
        };
        match peer_profile::load_cached(
            service.config.config_path(),
            &service.public_key_fingerprint,
        )
        .await
        {
            Ok(Some(profile)) => service.local_device_profile = profile,
            Ok(None) => {}
            Err(error) => log::warn!("cannot load local device profile: {error}"),
        }
        for handle in service.client_manager.registered_clients() {
            if let Some(fingerprint) = service.client_manager.peer_fingerprint(handle) {
                match peer_profile::load_cached(service.config.config_path(), &fingerprint).await {
                    Ok(Some(profile)) => {
                        service.peer_profiles.insert(fingerprint, profile);
                    }
                    Ok(None) => {}
                    Err(error) => log::warn!("cannot load peer device profile: {error}"),
                }
            }
        }
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

        let mut profile_tick = tokio::time::interval(Duration::from_secs(1));
        profile_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut history_changes = self.history.subscribe_changes();
        loop {
            let clear_deadline = self
                .history_clear
                .as_ref()
                .map(|clear| tokio::time::Instant::from_std(clear.deadline));
            tokio::select! {
                changed = history_changes.changed() => {
                    if changed.is_ok() {
                        self.notify_frontend(FrontendEvent::HistoryChanged);
                    } else {
                        history_changes = self.history.subscribe_changes();
                    }
                },
                _ = profile_tick.tick() => {
                    self.retry_profiles();
                    let actions = self.manual_transfers.tick(std::time::Instant::now());
                    self.execute_manual_actions(actions);
                    self.refresh_manual_routes();
                    let discarded = self.history_reassembler.expire(std::time::Instant::now());
                    if discarded != 0 {
                        log::warn!("expired {discarded} incomplete history synchronization record(s)");
                    }
                },
                _ = tokio::time::sleep_until(self.e2e_check_at), if self.e2e_pending => {
                    // File transfer only needs the authenticated peer transport.
                    // Remote input readiness is an unrelated capability.
                    let ready = self.client_manager.active_clients().into_iter().any(|handle| {
                        self.client_manager.alive(handle)
                    });
                    if ready {
                        self.e2e_pending = false;
                        self.run_copyfile_e2e();
                    } else {
                        self.e2e_check_at += Duration::from_secs(2);
                    }
                },
                request = self.frontend_listener.next() => {
                    if self.handle_frontend_request(request).await { break; }
                },
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event).await,
                event = self.capture.event() => self.handle_capture_event(event).await,
                event = self.adapter_events.recv() => {
                    if let Some(event) = event {
                        self.handle_adapter_event(event).await;
                    }
                },
                result = self.source_results.recv() => {
                    if let Some(result) = result {
                        self.handle_source_result(result);
                    }
                },
                result = self.clipboard.next_read(), if self.legacy_clipboard => match result {
                    Ok(ClipboardContent::Text(text))
                        if self.clipboard_settings.text && !self.native_file_selection =>
                    {
                        self.record_local_history(history::text(text.clone())).await;
                        for handle in self.client_manager.clipboard_clients() {
                            self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1);
                            self.capture.send_clipboard(handle, self.next_clipboard_transfer, text.as_bytes().to_vec(), None);
                        }
                    }
                    Ok(ClipboardContent::Image { width, height, rgba })
                        if self.clipboard_settings.image && !self.native_file_selection =>
                    {
                        self.record_local_history(history::image(width, height, rgba.clone())).await;
                        for handle in self.client_manager.clipboard_clients() {
                            self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1);
                            self.capture.send_clipboard(handle, self.next_clipboard_transfer, rgba.clone(), Some((width, height)));
                        }
                    }
                    Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
                    Err(e) => log::warn!("failed to read clipboard: {e}"),
                },
                event = self.resolver.event() => self.handle_resolver_event(event),
                _ = tokio::time::sleep_until(clear_deadline.unwrap_or_else(tokio::time::Instant::now)), if clear_deadline.is_some() => {
                    if self.history_clear.as_ref().is_some_and(|clear| clear.deadline <= std::time::Instant::now()) {
                        let clear = self.history_clear.take().expect("clear exists");
                        let missing = (if clear.applying {
                            clear.awaiting_acks.len()
                        } else {
                            clear.awaiting_boundaries.len()
                        })
                        .max(clear.errors.len());
                        self.notify_frontend(FrontendEvent::HistoryClearResult {
                            operation_id: hex_operation(clear.operation_id),
                            affected: if clear.applying { clear.affected as u64 } else { 0 },
                            peers_acknowledged: (clear.peer_count.saturating_sub(missing)) as u32,
                            error: Some(format!(
                                "Global history clear timed out waiting for {missing} connected peer(s); not all devices were cleared."
                            )),
                        });
                    }
                },
                event = async {
                    match self.discovery.as_mut() {
                        Some(discovery) => discovery.event().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match event {
                        Some(DiscoveryEvent::Peers(peers)) => self.notify_frontend(FrontendEvent::DiscoveredPeers(peers)),
                        Some(DiscoveryEvent::Error(error)) => self.notify_frontend(FrontendEvent::Error(error)),
                        None => { self.discovery = None; }
                    }
                },
                _ = self.config.changed() => self.handle_config_change(),
                r = signal::ctrl_c() => break r.expect("failed to wait for CTRL+C"),
            }
        }

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        if let Some(discovery) = self.discovery.as_mut() {
            discovery.shutdown();
        }
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;
        log::debug!("terminating file adapters ...");
        if let Some(adapter_manager) = self.adapter_manager.take() {
            adapter_manager.shutdown().await?;
        }

        Ok(())
    }

    async fn handle_frontend_request(
        &mut self,
        request: Option<Result<FrontendRequest, IpcError>>,
    ) -> bool {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => {
                log::error!("error receiving request: {e}");
                return false;
            }
        };
        match request {
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::StopService => return true,
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                if active && self.input_sharing {
                    self.reenable_missing_backends();
                }
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::DiscoverPeers => {
                if let Some(discovery) = self.discovery.as_ref() {
                    if let Err(error) = discovery.refresh() {
                        self.notify_frontend(FrontendEvent::Error(format!(
                            "mDNS discovery refresh failed: {error}"
                        )));
                    }
                } else {
                    self.notify_frontend(FrontendEvent::Error("mDNS discovery unavailable".into()));
                }
            }
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
            FrontendRequest::SetInputSharing(enabled) => {
                self.input_sharing = enabled;
                self.capture.set_input_sharing(enabled);
                self.emulation.set_input_sharing(enabled);
                if enabled {
                    self.reenable_missing_backends();
                }
                self.notify_frontend(FrontendEvent::InputSharing(enabled));
            }
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
            FrontendRequest::SetLocalDeviceProfile(profile) => {
                if let Err(error) = profile.validate() {
                    self.notify_frontend(FrontendEvent::Error(error.into()));
                } else {
                    if self.local_device_profile != profile {
                        match peer_profile::save_cached(
                            self.config.config_path(),
                            &self.public_key_fingerprint,
                            &profile,
                        )
                        .await
                        {
                            Ok(()) => {
                                self.local_device_profile = profile;
                                for (_, peer) in self.connected_authenticated_peers() {
                                    self.send_peer(
                                        peer,
                                        lan_mouse_proto::ProtoEvent::ProfileChanged,
                                    );
                                }
                            }
                            Err(error) => self.notify_frontend(FrontendEvent::Error(format!(
                                "Cannot save device profile: {error}"
                            ))),
                        }
                    }
                }
            }
            FrontendRequest::RegenerateIdentity => {
                let path = self.config.cert_path().to_path_buf();
                match tokio::task::spawn_blocking(move || crypto::regenerate_key_and_cert(&path))
                    .await
                {
                    Ok(Ok(fingerprint)) => {
                        self.notify_frontend(FrontendEvent::IdentityRegenerated { fingerprint });
                    }
                    Ok(Err(error)) => self.notify_frontend(FrontendEvent::Error(error.to_string())),
                    Err(error) => self.notify_frontend(FrontendEvent::Error(format!(
                        "identity regeneration failed: {error}"
                    ))),
                }
            }
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
            FrontendRequest::QueryHistory {
                query,
                offset,
                limit,
            } => {
                match self
                    .history
                    .page(query.clone(), offset, usize::from(limit))
                    .await
                {
                    Ok(page) => self.notify_frontend(FrontendEvent::HistoryPage(
                        history::ipc_page(query, offset, page),
                    )),
                    Err(error) => self.notify_frontend(FrontendEvent::HistoryError(error)),
                }
            }
            FrontendRequest::SetHistoryPinned { event_id, pinned } => {
                let event_id = history::store_id(event_id);
                match self.history.set_pinned(event_id.clone(), pinned).await {
                    Ok(updated) => self.notify_frontend(FrontendEvent::HistoryPinResult {
                        event_id: history::ipc_id(event_id),
                        pinned,
                        updated,
                    }),
                    Err(error) => self.notify_frontend(FrontendEvent::HistoryError(error)),
                }
            }
            FrontendRequest::GetHistoryImage(event_id) => {
                let store_id = history::store_id(event_id.clone());
                match self.history.image(store_id).await {
                    Ok(image) => self.notify_frontend(FrontendEvent::HistoryImageResult {
                        event_id: event_id.clone(),
                        image: image.map(|image| history::ipc_image(event_id, image)),
                        error: None,
                    }),
                    Err(error) => self.notify_frontend(FrontendEvent::HistoryImageResult {
                        event_id,
                        image: None,
                        error: Some(error),
                    }),
                }
            }
            FrontendRequest::ClearGlobalHistory => {
                self.start_global_history_clear().await;
            }
            FrontendRequest::SendFiles {
                peer_fingerprint,
                paths,
            } => {
                let peer = self
                    .connected_authenticated_peers()
                    .get(&peer_fingerprint)
                    .copied();
                if let Some(peer) = peer {
                    let actions = self
                        .manual_transfers
                        .send_files(peer, peer_fingerprint, paths, std::time::Instant::now())
                        .await;
                    self.execute_manual_actions(actions);
                } else {
                    self.notify_frontend(FrontendEvent::ManualTransferError(
                        "The selected device is no longer connected".into(),
                    ));
                }
            }
            FrontendRequest::AcceptFileTransfer {
                peer_fingerprint,
                transfer_id,
                destination_directory,
            } => {
                let actions = self
                    .manual_transfers
                    .accept(
                        &peer_fingerprint,
                        transfer_id,
                        destination_directory,
                        std::time::Instant::now(),
                    )
                    .await;
                self.execute_manual_actions(actions);
            }
            FrontendRequest::DeclineFileTransfer {
                peer_fingerprint,
                transfer_id,
            } => {
                let actions = self
                    .manual_transfers
                    .decline(&peer_fingerprint, transfer_id);
                self.execute_manual_actions(actions);
            }
            FrontendRequest::CancelManualTransfer {
                peer_fingerprint,
                transfer_id,
            } => {
                let actions = self.manual_transfers.cancel(&peer_fingerprint, transfer_id);
                self.execute_manual_actions(actions);
            }
            FrontendRequest::SetFileReceiveSettings(settings) => {
                if !settings.download_directory.is_absolute() {
                    self.notify_frontend(FrontendEvent::FileReceiveSettingsChanged(
                        self.file_receive_settings.clone(),
                        Some("Download directory must be an absolute path".into()),
                    ));
                } else {
                    match self.config.persist_file_receive_settings(&settings) {
                        Ok(()) => {
                            self.file_receive_settings = settings;
                            self.notify_frontend(FrontendEvent::FileReceiveSettingsChanged(
                                self.file_receive_settings.clone(),
                                None,
                            ));
                        }
                        Err(error) => {
                            self.notify_frontend(FrontendEvent::FileReceiveSettingsChanged(
                                self.file_receive_settings.clone(),
                                Some(format!("Cannot save file receive settings: {error}")),
                            ));
                        }
                    }
                }
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
        }
        false
    }

    fn save_config(&mut self) {
        let clients = self
            .client_manager
            .get_client_states()
            .into_iter()
            .map(|(handle, c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                peer_fingerprint: self.client_manager.peer_fingerprint(handle),
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

    async fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                if !self.input_sharing {
                    self.emulation.send_leave_event(addr);
                    return;
                }
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
                self.history_peer_lost(Peer::Emulation(addr));
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    if let Some(discovery) = self.discovery.as_mut() {
                        if let Err(error) = discovery.change_port(port) {
                            self.notify_frontend(FrontendEvent::Error(format!(
                                "mDNS advertisement failed: {error}"
                            )));
                        }
                    }
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
                self.authenticated_peer_fingerprints
                    .insert(addr, fingerprint.clone());
                self.refresh_manual_routes();
                self.history_connected_peers.insert(Peer::Emulation(addr));
                self.notify_frontend(FrontendEvent::DeviceConnected {
                    addr,
                    fingerprint: fingerprint.clone(),
                });
                self.request_profile(fingerprint, Peer::Emulation(addr));
                self.request_history_sync(Peer::Emulation(addr), 0);
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
            EmulationEvent::Clipboard {
                addr: _,
                transfer_id: _,
                content,
            } => {
                // Live clipboard delivery and durable history reconciliation are separate:
                // applying the live value must not allocate a second local-origin record.
                let history_content = match &content {
                    ClipboardContent::Text(text) => Some(history::text(text.clone())),
                    ClipboardContent::Image {
                        width,
                        height,
                        rgba,
                    } => Some(history::image(*width, *height, rgba.clone())),
                };
                if let Some(content) = history_content {
                    self.pending_remote_history_echoes.push_back(content);
                    if self.pending_remote_history_echoes.len() > 8 {
                        self.pending_remote_history_echoes.pop_front();
                    }
                }
                match &content {
                    ClipboardContent::Text(text) if self.clipboard_settings.text => {
                        self.emulation.publish_file_clipboard(vec![
                            (
                                "text/plain;charset=utf-8".to_string(),
                                text.as_bytes().to_vec(),
                            ),
                            ("text/plain".to_string(), text.as_bytes().to_vec()),
                        ]);
                    }
                    _ => self.clipboard.write(content),
                }
            }
            EmulationEvent::NativeClipboard(ClipboardContent::Text(text))
                if self.clipboard_settings.text =>
            {
                self.native_file_selection = false;
                let content = history::text(text.clone());
                if let Some(index) = self
                    .pending_remote_history_echoes
                    .iter()
                    .position(|queued| queued == &content)
                {
                    self.pending_remote_history_echoes.remove(index);
                    return;
                }
                self.record_local_history(content).await;
                for handle in self.client_manager.clipboard_clients() {
                    self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1);
                    self.capture.send_clipboard(
                        handle,
                        self.next_clipboard_transfer,
                        text.as_bytes().to_vec(),
                        None,
                    );
                }
            }
            EmulationEvent::NativeClipboard(ClipboardContent::Image {
                width,
                height,
                rgba,
            }) if self.clipboard_settings.image => {
                self.native_file_selection = false;
                let content = history::image(width, height, rgba.clone());
                if let Some(index) = self
                    .pending_remote_history_echoes
                    .iter()
                    .position(|queued| queued == &content)
                {
                    self.pending_remote_history_echoes.remove(index);
                    return;
                }
                self.record_local_history(content).await;
                for handle in self.client_manager.clipboard_clients() {
                    self.next_clipboard_transfer = self.next_clipboard_transfer.wrapping_add(1);
                    self.capture.send_clipboard(
                        handle,
                        self.next_clipboard_transfer,
                        rgba.clone(),
                        Some((width, height)),
                    );
                }
            }
            EmulationEvent::NativeClipboard(_) => {
                self.native_file_selection = false;
            }
            EmulationEvent::FileClipboard { mime_type, value } => {
                self.native_file_selection = true;
                self.handle_native_file_clipboard(mime_type, value);
            }
            EmulationEvent::ClipboardProtocol { addr, event } => {
                self.handle_peer_protocol(Peer::Emulation(addr), event)
                    .await;
            }
        }
    }

    async fn record_local_history(&mut self, content: lan_mouse_history::HistoryContent) {
        match self.history.record_event(content).await {
            Ok(event) => self.broadcast_history_record(&event),
            Err(error) => log::warn!("failed to persist local clipboard history: {error}"),
        }
    }

    /// A file clipboard published by our own FUSE mount must never be
    /// re-announced as a local copy: that would echo the transfer back to
    /// the peer that sent it.
    fn is_own_clipboard_mount(value: &str) -> bool {
        value
            .lines()
            .filter(|line| line.starts_with("file://"))
            .all(|line| {
                line.contains("lan-mouse/clipboard/") || line.contains("lan%2Dmouse/clipboard/")
            })
            && value.lines().any(|line| line.starts_with("file://"))
    }

    fn handle_native_file_clipboard(&mut self, mime_type: String, value: String) {
        if Self::is_own_clipboard_mount(&value) {
            log::debug!("ignoring clipboard echo of our own mount");
            return;
        }
        if self.last_file_clipboard.as_deref() == Some(value.as_str()) {
            log::debug!("ignoring duplicate native file clipboard event");
            return;
        }
        self.last_file_clipboard = Some(value.clone());
        log::info!("native file clipboard detected: mime={mime_type}");
        if let Some(manager) = self.adapter_manager.as_ref() {
            let _ = manager.try_send(ManagerCommand::ClipboardData {
                transfer_id: "remote-desktop".to_string(),
                mime_type,
                value,
            });
        }
    }

    /// Automated end-to-end check of the file clipboard: generate a file,
    /// publish it as a local copy and let the peer paste it back.
    fn run_copyfile_e2e(&mut self) {
        let dir = std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("tmp");
        if let Err(error) = std::fs::create_dir_all(&dir) {
            log::error!("copyfile-e2e: cannot create {}: {error}", dir.display());
            return;
        }
        let path = dir.join(format!("lan-mouse-e2e-{}.bin", std::process::id()));
        // 96 KiB of deterministic but non-trivial content: larger than a
        // single chunk, so a real multi-chunk stream is exercised.
        let payload = Self::e2e_payload();
        if let Err(error) = std::fs::write(&path, &payload) {
            log::error!("copyfile-e2e: cannot write {}: {error}", path.display());
            return;
        }
        let digest = Sha256::digest(&payload);
        log::info!(
            "copyfile-e2e: source={} size={} sha256={:x}",
            path.display(),
            payload.len(),
            digest
        );
        let uri = format!("file://{}", path.display());
        self.handle_native_file_clipboard(
            lan_mouse_adapter_api::GNOME_COPIED_FILES_MIME.to_string(),
            format!("copy\r\n{uri}\r\n"),
        );
    }

    /// Deterministic payload used by the automated end-to-end check.
    fn e2e_payload() -> Vec<u8> {
        (0..98_304u32).map(|i| (i % 251) as u8).collect()
    }

    /// Paste side of the automated check: copy the remote FUSE file into
    /// `~/tmp`, exactly as a file manager paste would, then verify the result.
    fn verify_copyfile_e2e(&self, uris: &[String]) {
        let paths: Vec<PathBuf> = uris
            .iter()
            .filter_map(|uri| uri.strip_prefix("file://").map(percent_decode_path))
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("lan-mouse-e2e-"))
            })
            .collect();
        if paths.is_empty() {
            return;
        }
        let destination_dir = std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("tmp");
        let expected = Self::e2e_payload();
        let expected_digest = Sha256::digest(&expected);
        tokio::task::spawn_blocking(move || {
            for source in paths {
                let Some(name) = source.file_name() else {
                    log::error!("copyfile-e2e: FAIL source has no file name");
                    continue;
                };
                let destination =
                    destination_dir.join(format!("pasted-{}", name.to_string_lossy()));
                if let Err(error) = std::fs::copy(&source, &destination) {
                    log::error!(
                        "copyfile-e2e: FAIL paste {} -> {}: {error}",
                        source.display(),
                        destination.display()
                    );
                    continue;
                }
                match std::fs::read(&destination) {
                    Ok(data) => {
                        let digest = Sha256::digest(&data);
                        if data.len() == expected.len() && digest == expected_digest {
                            log::info!(
                                "copyfile-e2e: PASS pasted={} size={} sha256={:x}",
                                destination.display(),
                                data.len(),
                                digest
                            );
                        } else {
                            log::error!(
                                "copyfile-e2e: FAIL pasted={} size={} expected={} sha256={:x} expected={:x}",
                                destination.display(),
                                data.len(),
                                expected.len(),
                                digest,
                                expected_digest
                            );
                        }
                    }
                    Err(error) => log::error!(
                        "copyfile-e2e: FAIL read pasted file {}: {error}",
                        destination.display()
                    ),
                }
            }
        });
    }
    fn connected_authenticated_peers(&self) -> HashMap<String, Peer> {
        history_sync::deduplicate_routes(
            self.authenticated_capture_fingerprints
                .iter()
                .map(|(handle, fingerprint)| (fingerprint.clone(), Peer::Capture(*handle)))
                .chain(
                    self.authenticated_peer_fingerprints
                        .iter()
                        .map(|(addr, fingerprint)| (fingerprint.clone(), Peer::Emulation(*addr))),
                ),
        )
    }

    fn send_peer(&self, peer: Peer, event: lan_mouse_proto::ProtoEvent) {
        match peer {
            Peer::Capture(handle) => self.capture.send_proto(handle, event),
            Peer::Emulation(addr) => self.emulation.send_proto(addr, event),
        }
    }
    fn refresh_manual_routes(&mut self) {
        let routes = self.connected_authenticated_peers();
        let actions = self.manual_transfers.refresh_routes(&routes);
        self.execute_manual_actions(actions);
    }

    fn next_history_request_id(&mut self) -> u64 {
        self.next_history_request = self.next_history_request.wrapping_add(1);
        self.next_history_request
    }

    fn request_history_sync(&mut self, peer: Peer, offset: u64) {
        let request_id = self.next_history_request_id();
        self.history_sync_progress
            .insert(peer, (request_id, offset, 0));
        self.send_peer(
            peer,
            lan_mouse_proto::ProtoEvent::HistorySyncRequest { request_id, offset },
        );
    }

    fn broadcast_history_record(&mut self, record: &ImportedHistoryEvent) {
        let request_id = self.next_history_request_id();
        let Ok(events) = history_sync::record_events(request_id, 0, record) else {
            log::warn!("new history record exceeds synchronization limit");
            return;
        };
        for (_, peer) in self.connected_authenticated_peers() {
            for event in events.iter().cloned() {
                self.send_peer(peer, event);
            }
            self.send_peer(
                peer,
                lan_mouse_proto::ProtoEvent::HistorySyncPageEnd {
                    request_id,
                    next_offset: None,
                },
            );
        }
    }

    async fn send_history_page(&self, peer: Peer, request_id: u64, offset: u64) {
        match self.history.export(offset, history_sync::PAGE_SIZE).await {
            Ok((records, next_offset)) => {
                for (record_id, record) in records.iter().enumerate() {
                    if let Ok(events) =
                        history_sync::record_events(request_id, record_id as u64, record)
                    {
                        for event in events {
                            self.send_peer(peer, event);
                        }
                    }
                }
                self.send_peer(
                    peer,
                    lan_mouse_proto::ProtoEvent::HistorySyncPageEnd {
                        request_id,
                        next_offset,
                    },
                );
            }
            Err(error) => log::warn!("cannot export clipboard history page: {error}"),
        }
    }

    async fn start_global_history_clear(&mut self) {
        if self.history_clear.is_some() {
            self.notify_frontend(FrontendEvent::HistoryError(
                "A global history clear is already in progress.".into(),
            ));
            return;
        }
        let boundary = match self.history.snapshot_boundary().await {
            Ok(boundary) => boundary,
            Err(error) => {
                self.notify_frontend(FrontendEvent::HistoryError(error));
                return;
            }
        };
        let mut operation_hasher = Sha256::new();
        operation_hasher.update(self.public_key_fingerprint.as_bytes());
        operation_hasher.update(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_be_bytes(),
        );
        operation_hasher.update(self.next_history_request_id().to_be_bytes());
        let digest = operation_hasher.finalize();
        let mut operation_id = [0u8; 16];
        operation_id.copy_from_slice(&digest[..16]);
        let peers = self.connected_authenticated_peers();
        let coordinator = history_sync::ClearCoordinator::new(
            operation_id,
            peers.keys().cloned().collect(),
            boundary,
        );
        if peers.is_empty() {
            self.finish_local_only_clear(coordinator).await;
            return;
        }
        for peer in peers.values().copied() {
            self.send_peer(
                peer,
                lan_mouse_proto::ProtoEvent::HistoryClearBoundaryRequest { operation_id },
            );
        }
        self.history_clear = Some(coordinator);
    }

    async fn finish_local_only_clear(
        &mut self,
        coordinator: history_sync::ClearCoordinator<String>,
    ) {
        let operation = hex_operation(coordinator.operation_id);
        match self
            .history
            .apply_clear(operation.clone(), coordinator.boundary)
            .await
        {
            Ok(affected) => self.notify_frontend(FrontendEvent::HistoryClearResult {
                operation_id: operation,
                affected: affected as u64,
                peers_acknowledged: 0,
                error: None,
            }),
            Err(error) => self.notify_frontend(FrontendEvent::HistoryClearResult {
                operation_id: operation,
                affected: 0,
                peers_acknowledged: 0,
                error: Some(error),
            }),
        }
    }

    fn request_profile(&mut self, fingerprint: String, peer: Peer) {
        if self.profile_requests.contains_key(&fingerprint) {
            return;
        }
        let request_id = self.next_history_request_id();
        self.profile_requests
            .insert(fingerprint, (request_id, 0, std::time::Instant::now()));
        self.send_peer(
            peer,
            lan_mouse_proto::ProtoEvent::ProfileRequest { request_id },
        );
    }

    fn retry_profiles(&mut self) {
        let now = std::time::Instant::now();
        self.profile_reassembler.expire(now);
        let routes = self.connected_authenticated_peers();
        let expired = self
            .profile_requests
            .iter()
            .filter(|(_, (_, _, sent))| {
                now.saturating_duration_since(*sent) >= Duration::from_secs(2)
            })
            .map(|(fingerprint, (_, retries, _))| (fingerprint.clone(), *retries))
            .collect::<Vec<_>>();
        for (fingerprint, retries) in expired {
            self.profile_requests.remove(&fingerprint);
            self.profile_reassembler.remove_peer(&fingerprint);
            if retries >= 3 {
                log::warn!("device profile exchange timed out for {fingerprint}");
                continue;
            }
            if let Some(peer) = routes.get(&fingerprint) {
                let request_id = self.next_history_request_id();
                self.profile_requests
                    .insert(fingerprint, (request_id, retries + 1, now));
                self.send_peer(
                    *peer,
                    lan_mouse_proto::ProtoEvent::ProfileRequest { request_id },
                );
            }
        }
    }

    async fn accept_profile(&mut self, fingerprint: String, profile: DeviceProfile) {
        self.profile_requests.remove(&fingerprint);
        if self.peer_profiles.get(&fingerprint) == Some(&profile) {
            return;
        }
        if let Err(error) =
            peer_profile::save_cached(self.config.config_path(), &fingerprint, &profile).await
        {
            self.notify_frontend(FrontendEvent::Error(format!(
                "Cannot save received peer profile: {error}"
            )));
        }
        self.peer_profiles
            .insert(fingerprint.clone(), profile.clone());
        self.notify_frontend(FrontendEvent::PeerDeviceProfile {
            fingerprint,
            profile,
        });
    }

    fn history_peer_lost(&mut self, peer: Peer) {
        self.history_connected_peers.remove(&peer);
        self.history_reassembler.peer_lost(&peer);
        self.history_sync_progress.remove(&peer);
        let identity = match peer {
            Peer::Capture(handle) => self.authenticated_capture_fingerprints.remove(&handle),
            Peer::Emulation(addr) => self.authenticated_peer_fingerprints.remove(&addr),
        };
        self.refresh_manual_routes();
        if let Some(identity) = identity {
            if let Some(route) = self.connected_authenticated_peers().get(&identity).copied() {
                let retry = self.history_clear.as_ref().and_then(|clear| {
                    if clear.awaiting_boundaries.contains(&identity) {
                        Some(lan_mouse_proto::ProtoEvent::HistoryClearBoundaryRequest {
                            operation_id: clear.operation_id,
                        })
                    } else if clear.awaiting_acks.contains(&identity) {
                        Some(lan_mouse_proto::ProtoEvent::HistoryClearApply {
                            operation_id: clear.operation_id,
                            boundary: history_sync::boundary_to_wire(&clear.boundary),
                        })
                    } else {
                        None
                    }
                });
                if let Some(event) = retry {
                    self.send_peer(route, event);
                }
            } else if let Some(clear) = self.history_clear.as_mut() {
                clear.peer_lost(&identity);
            }
        }
    }

    async fn handle_peer_protocol(&mut self, peer: Peer, event: lan_mouse_proto::ProtoEvent) {
        use lan_mouse_proto::ProtoEvent;
        let Some(identity) = (match peer {
            Peer::Capture(handle) => self.authenticated_capture_fingerprints.get(&handle),
            Peer::Emulation(addr) => self.authenticated_peer_fingerprints.get(&addr),
        })
        .cloned() else {
            log::warn!(
                "ignoring history protocol from route without certificate identity: {peer:?}"
            );
            return;
        };
        match event {
            ProtoEvent::ManualFileOffer { .. }
            | ProtoEvent::ManualFileDecision { .. }
            | ProtoEvent::ManualFileRequest { .. }
            | ProtoEvent::ManualFileChunk { .. }
            | ProtoEvent::ManualFileComplete { .. }
            | ProtoEvent::ManualFileResult { .. }
            | ProtoEvent::ManualFileCancel { .. } => {
                let actions = self
                    .manual_transfers
                    .handle_protocol(
                        peer,
                        identity,
                        event,
                        &self.file_receive_settings,
                        std::time::Instant::now(),
                    )
                    .await;
                self.execute_manual_actions(actions);
            }
            ProtoEvent::ProfileChanged => {
                self.profile_requests.remove(&identity);
                self.profile_reassembler.remove_peer(&identity);
                self.request_profile(identity, peer);
            }
            ProtoEvent::ProfileRequest { request_id } => {
                if let Ok(events) =
                    peer_profile::encode_profile(request_id, &self.local_device_profile)
                {
                    for event in events {
                        self.send_peer(peer, event);
                    }
                }
            }
            ProtoEvent::ProfileStart {
                request_id,
                width,
                height,
                total_len,
                chunks,
                display_name,
            } => {
                if !self
                    .profile_requests
                    .get(&identity)
                    .is_some_and(|(id, _, _)| *id == request_id)
                {
                    return;
                }
                match self.profile_reassembler.start(
                    &identity,
                    request_id,
                    width,
                    height,
                    total_len,
                    chunks,
                    display_name,
                ) {
                    Ok(Some(profile)) => self.accept_profile(identity.clone(), profile).await,
                    Ok(None) => {}
                    Err(error) => log::warn!("profile rejected: {error}"),
                }
            }
            ProtoEvent::ProfileChunk {
                request_id,
                index,
                data,
            } => {
                if !self
                    .profile_requests
                    .get(&identity)
                    .is_some_and(|(id, _, _)| *id == request_id)
                {
                    return;
                }
                match self
                    .profile_reassembler
                    .chunk(&identity, request_id, index, data)
                {
                    Ok(Some(profile)) => self.accept_profile(identity.clone(), profile).await,
                    Ok(None) => {}
                    Err(error) => log::warn!("profile rejected: {error}"),
                }
            }
            ProtoEvent::HistorySyncRequest { request_id, offset } => {
                self.send_history_page(peer, request_id, offset).await;
            }
            ProtoEvent::HistoryRecordStart {
                request_id,
                record_id,
                total_len,
                chunks,
            } => {
                if let Err(error) = self
                    .history_reassembler
                    .start(peer, request_id, record_id, total_len, chunks)
                {
                    log::warn!("history synchronization rejected: {error}");
                }
            }
            ProtoEvent::HistoryRecordChunk {
                request_id,
                record_id,
                index,
                data,
            } => {
                match self
                    .history_reassembler
                    .chunk(&peer, request_id, record_id, index, data)
                {
                    Ok(Some(record)) => {
                        if let Err(error) = self.history.import(record).await {
                            log::warn!("history synchronization import rejected: {error}");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => log::warn!("history synchronization rejected: {error}"),
                }
            }
            ProtoEvent::HistorySyncPageEnd {
                request_id,
                next_offset,
            } => {
                let discarded = self.history_reassembler.finish_request(&peer, request_id);
                let Some((active_request, page_offset, retries)) =
                    self.history_sync_progress.get(&peer).copied()
                else {
                    return;
                };
                if active_request != request_id {
                    return;
                }
                if discarded != 0 {
                    if retries >= 3 {
                        self.history_sync_progress.remove(&peer);
                        let error = format!(
                            "History synchronization with a device stopped after repeated incomplete transfers ({discarded} incomplete record(s))."
                        );
                        log::warn!("{error}");
                        self.notify_frontend(FrontendEvent::HistoryError(error));
                    } else {
                        let retry_request = self.next_history_request_id();
                        self.history_sync_progress
                            .insert(peer, (retry_request, page_offset, retries + 1));
                        self.send_peer(
                            peer,
                            ProtoEvent::HistorySyncRequest {
                                request_id: retry_request,
                                offset: page_offset,
                            },
                        );
                    }
                } else if let Some(offset) = next_offset {
                    self.history_sync_progress
                        .insert(peer, (request_id, offset, 0));
                    self.send_peer(peer, ProtoEvent::HistorySyncRequest { request_id, offset });
                } else {
                    self.history_sync_progress.remove(&peer);
                }
            }
            ProtoEvent::HistoryClearBoundaryRequest { operation_id } => {
                match self.history.snapshot_boundary().await {
                    Ok(boundary) => self.send_peer(
                        peer,
                        ProtoEvent::HistoryClearBoundary {
                            operation_id,
                            boundary: history_sync::boundary_to_wire(&boundary),
                        },
                    ),
                    Err(error) => self.send_peer(
                        peer,
                        ProtoEvent::HistoryClearAck {
                            operation_id,
                            affected: 0,
                            error: Some(error),
                        },
                    ),
                }
            }
            ProtoEvent::HistoryClearBoundary {
                operation_id,
                boundary,
            } => {
                let ready = self.history_clear.as_mut().is_some_and(|clear| {
                    clear.operation_id == operation_id
                        && clear.add_boundary(&identity, history_sync::boundary_from_wire(boundary))
                });
                if ready {
                    let mut clear = self.history_clear.take().expect("clear exists");
                    let operation = hex_operation(operation_id);
                    match self
                        .history
                        .apply_clear(operation, clear.boundary.clone())
                        .await
                    {
                        Ok(affected) => {
                            let peers = clear.participants.clone();
                            clear.begin_apply(peers.clone(), affected);
                            let boundary = history_sync::boundary_to_wire(&clear.boundary);
                            for identity in peers {
                                if let Some(peer) =
                                    self.connected_authenticated_peers().get(&identity).copied()
                                {
                                    self.send_peer(
                                        peer,
                                        ProtoEvent::HistoryClearApply {
                                            operation_id,
                                            boundary: boundary.clone(),
                                        },
                                    );
                                }
                            }
                            self.history_clear = Some(clear);
                        }
                        Err(error) => self.notify_frontend(FrontendEvent::HistoryClearResult {
                            operation_id: hex_operation(operation_id),
                            affected: 0,
                            peers_acknowledged: 0,
                            error: Some(error),
                        }),
                    }
                }
            }
            ProtoEvent::HistoryClearApply {
                operation_id,
                boundary,
            } => {
                let result = self
                    .history
                    .apply_clear(
                        hex_operation(operation_id),
                        history_sync::boundary_from_wire(boundary),
                    )
                    .await;
                self.send_peer(
                    peer,
                    ProtoEvent::HistoryClearAck {
                        operation_id,
                        affected: result.as_ref().map_or(0, |affected| *affected as u64),
                        error: result.err(),
                    },
                );
            }
            ProtoEvent::HistoryClearAck {
                operation_id,
                affected,
                error,
            } => {
                let done = self.history_clear.as_mut().is_some_and(|clear| {
                    clear.operation_id == operation_id
                        && clear.applying
                        && clear.add_ack(&identity, affected, error)
                });
                if done {
                    let clear = self.history_clear.take().expect("clear exists");
                    self.notify_frontend(FrontendEvent::HistoryClearResult {
                        operation_id: hex_operation(operation_id),
                        affected: clear.affected as u64,
                        peers_acknowledged: clear.peer_count as u32,
                        error: (!clear.errors.is_empty()).then(|| clear.errors.join("; ")),
                    });
                }
            }
            event => self.handle_file_protocol(peer, event),
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
    async fn handle_capture_event(&mut self, event: ICaptureEvent) {
        // Capture events may already be queued when a configured client is deleted.
        // Handles are never reused, so a removed route cannot authenticate a new peer.
        let configured_handle = match &event {
            ICaptureEvent::Clipboard { handle, .. }
            | ICaptureEvent::PeerAuthenticated { handle, .. }
            | ICaptureEvent::FileClipboard { handle, .. }
            | ICaptureEvent::PeerStateChanged(handle)
            | ICaptureEvent::ClientEntered(handle) => Some(*handle),
            _ => None,
        };
        if configured_handle.is_some_and(|handle| !self.client_manager.contains(handle)) {
            return;
        }
        match event {
            ICaptureEvent::Clipboard {
                handle,
                fingerprint,
                event,
            } => {
                self.authenticated_capture_fingerprints
                    .insert(handle, fingerprint);
                self.refresh_manual_routes();
                self.handle_peer_protocol(Peer::Capture(handle), event)
                    .await;
            }
            ICaptureEvent::PeerAuthenticated {
                handle,
                fingerprint,
            } => {
                let changed =
                    self.authenticated_capture_fingerprints.get(&handle) != Some(&fingerprint);
                self.authenticated_capture_fingerprints
                    .insert(handle, fingerprint.clone());
                self.refresh_manual_routes();
                if changed {
                    if self.client_manager.peer_fingerprint(handle).as_ref() != Some(&fingerprint) {
                        self.client_manager
                            .set_peer_fingerprint(handle, fingerprint.clone());
                        self.save_config();
                    }
                    self.notify_frontend(FrontendEvent::ClientFingerprint {
                        handle,
                        fingerprint: fingerprint.clone(),
                    });
                    self.request_profile(fingerprint, Peer::Capture(handle));
                }
            }
            ICaptureEvent::FileClipboard {
                handle,
                mime_type,
                value,
            } => {
                if Self::is_own_clipboard_mount(&value) {
                    log::debug!("ignoring clipboard echo of our own mount");
                    return;
                }
                log::info!(
                    "native clipboard selection routed from capture handle={handle} mime={mime_type}"
                );
                if let Some(manager) = self.adapter_manager.as_ref() {
                    let _ = manager.try_send(ManagerCommand::ClipboardData {
                        transfer_id: format!("portal-{handle}"),
                        mime_type,
                        value,
                    });
                }
            }
            ICaptureEvent::PeerLost(handle) => {
                // The transport died: every transfer bound to it can never
                // complete, so drop it instead of leaving it stalled.
                let actions = self.transfers.peer_lost(&Peer::Capture(handle));
                self.history_peer_lost(Peer::Capture(handle));
                if !actions.is_empty() {
                    log::info!(
                        "peer {handle} lost: dropping {} transfer action(s)",
                        actions.len()
                    );
                    self.execute_transfer_actions(actions);
                }
            }
            ICaptureEvent::PeerStateChanged(handle) => {
                self.broadcast_client(handle);
                if self.client_manager.alive(handle) {
                    if self.history_connected_peers.insert(Peer::Capture(handle)) {
                        self.request_history_sync(Peer::Capture(handle), 0);
                    }
                } else {
                    self.history_peer_lost(Peer::Capture(handle));
                }
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
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
                self.emulation.set_capture_ready(false);
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
                self.emulation.set_capture_ready(true);
            }
            ICaptureEvent::ClientEntered(handle) => {
                if self.input_sharing {
                    log::info!("entering client {handle} ...");
                    self.spawn_hook_command(handle);
                    if self.legacy_clipboard {
                        self.clipboard.read_once();
                    }
                }
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
        self.notify_frontend(FrontendEvent::InputSharing(self.input_sharing));
        self.notify_frontend(FrontendEvent::ClipboardSettings(self.clipboard_settings));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        for handle in self.client_manager.registered_clients() {
            if let Some(fingerprint) = self.client_manager.peer_fingerprint(handle) {
                self.notify_frontend(FrontendEvent::ClientFingerprint {
                    handle,
                    fingerprint,
                });
            }
        }
        for (fingerprint, profile) in self.peer_profiles.clone() {
            self.notify_frontend(FrontendEvent::PeerDeviceProfile {
                fingerprint,
                profile,
            });
        }
        self.notify_frontend(FrontendEvent::FileReceiveSettingsChanged(
            self.file_receive_settings.clone(),
            None,
        ));
        for status in self.manual_transfers.snapshot() {
            self.notify_frontend(FrontendEvent::ManualTransferStatus(status));
        }
        for offer in self.manual_transfers.pending_offers() {
            self.notify_frontend(FrontendEvent::IncomingFileOffer(offer));
        }
    }

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = crate::client::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
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
    fn reenable_missing_backends(&self) {
        if self.capture_status == Status::Disabled {
            self.capture.reenable();
        }
        if self.emulation_status == Status::Disabled {
            self.emulation.reenable();
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
        self.history_peer_lost(Peer::Capture(handle));
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
    async fn handle_adapter_event(&mut self, event: ManagerEvent) {
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
                        log::info!(
                            "file clipboard detected: transfer={} entries={}",
                            manifest.transfer_id,
                            manifest.entries.len()
                        );
                        if matches!(manifest.operation, Operation::Move) {
                            log::debug!("rejecting unsupported move clipboard manifest");
                            Ok(Vec::new())
                        } else {
                            if let Some(content) = history::files(
                                manifest
                                    .entries
                                    .iter()
                                    .filter(|entry| {
                                        !matches!(
                                            entry.kind,
                                            lan_mouse_adapter_api::EntryKind::Other
                                        )
                                    })
                                    .map(|entry| {
                                        (entry.uri.clone(), entry.size.unwrap_or_default())
                                    }),
                            ) {
                                self.record_local_history(content).await;
                            }
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
                            for handle in self.client_manager.clipboard_clients() {
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
                completed,
                total,
                ..
            } => {
                self.announced_transfers.insert(ui_id);
                self.transfer_progress.insert(ui_id, (completed, total));
                self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                    ClipboardTransferStatus {
                        transfer_id: ui_id,
                        file_id: 0,
                        name: String::new(),
                        direction: ClipboardTransferDirection::Receiving,
                        transferred_bytes: completed,
                        total_bytes: total,
                        bytes_per_second: 0,
                        state: ClipboardTransferState::Transferring,
                    },
                ));
            }
            TransferFrontendEvent::Completed {
                ui_id,
                file_id,
                completed,
                total,
                ..
            } => {
                if self.announced_transfers.remove(&ui_id) {
                    self.transfer_progress.remove(&ui_id);
                    self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                        ClipboardTransferStatus {
                            transfer_id: ui_id,
                            file_id: file_id.unwrap_or_default(),
                            name: String::new(),
                            direction: ClipboardTransferDirection::Receiving,
                            transferred_bytes: completed,
                            total_bytes: total,
                            bytes_per_second: 0,
                            state: ClipboardTransferState::Completed,
                        },
                    ));
                }
            }
            TransferFrontendEvent::Cancelled {
                ui_id,
                file_id,
                completed,
                total,
                ..
            } => {
                if self.announced_transfers.remove(&ui_id) {
                    self.transfer_progress.remove(&ui_id);
                    self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                        ClipboardTransferStatus {
                            transfer_id: ui_id,
                            file_id: file_id.unwrap_or_default(),
                            name: String::new(),
                            direction: ClipboardTransferDirection::Receiving,
                            transferred_bytes: completed,
                            total_bytes: total,
                            bytes_per_second: 0,
                            state: ClipboardTransferState::Cancelled,
                        },
                    ));
                }
            }
            TransferFrontendEvent::Failed {
                ui_id,
                file_id,
                completed,
                total,
                error,
                ..
            } => {
                if self.announced_transfers.remove(&ui_id) {
                    self.transfer_progress.remove(&ui_id);
                    self.notify_frontend(FrontendEvent::ClipboardTransferStatus(
                        ClipboardTransferStatus {
                            transfer_id: ui_id,
                            file_id: file_id.unwrap_or_default(),
                            name: String::new(),
                            direction: ClipboardTransferDirection::Receiving,
                            transferred_bytes: completed,
                            total_bytes: total,
                            bytes_per_second: 0,
                            state: ClipboardTransferState::Failed(error),
                        },
                    ));
                }
            }
            // A plain copy only publishes metadata: never a transfer row.
            TransferFrontendEvent::Offered { .. } => {}
        }
    }

    fn execute_manual_actions(&mut self, actions: Vec<ManualAction<Peer>>) {
        for action in actions {
            match action {
                ManualAction::Send { peer, event } => match peer {
                    Peer::Capture(handle) => self.capture.send_proto(handle, event),
                    Peer::Emulation(addr) => self.emulation.send_proto(addr, event),
                },
                ManualAction::Notify(event) => self.notify_frontend(event),
            }
        }
    }

    fn execute_transfer_actions(&mut self, actions: Vec<TransferAction<Peer>>) {
        for action in actions {
            match action {
                TransferAction::Peer { peer, event } => {
                    log::info!("file transfer protocol event: peer={peer:?} event={event:?}");
                    match peer {
                        Peer::Capture(handle) => self.capture.send_proto(handle, event),
                        Peer::Emulation(addr) => self.emulation.send_proto(addr, event),
                    }
                }
                TransferAction::Adapter {
                    adapter_id,
                    message,
                } => {
                    log::info!(
                        "file transfer adapter event: adapter={adapter_id} message={message:?}"
                    );
                    let command = match message {
                        AdapterMessage::RemoteManifest(m) => ManagerCommand::RemoteManifest(m),
                        AdapterMessage::RangeResponse(m) => ManagerCommand::RangeResponse(m),
                        AdapterMessage::PublishFileClipboard(m) => {
                            self.verify_copyfile_e2e(&m.uris);
                            let operation =
                                if matches!(m.operation, lan_mouse_adapter_api::Operation::Move) {
                                    "cut"
                                } else {
                                    "copy"
                                };
                            let uri_list = format!("{}\r\n", m.uris.join("\r\n")).into_bytes();
                            let gnome_files =
                                format!("{operation}\n{}\n", m.uris.join("\n")).into_bytes();
                            self.emulation.publish_file_clipboard(vec![
                                (lan_mouse_adapter_api::URI_LIST_MIME.to_string(), uri_list),
                                (
                                    lan_mouse_adapter_api::GNOME_COPIED_FILES_MIME.to_string(),
                                    gnome_files,
                                ),
                            ]);
                            continue;
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

fn hex_operation(operation_id: [u8; 16]) -> String {
    let mut value = String::with_capacity(32);
    for byte in operation_id {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

/// Decode a percent-encoded `file://` path body into a filesystem path.
fn percent_decode_path(encoded: &str) -> PathBuf {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}
