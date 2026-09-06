use crate::{
    adapter_manager::{
        AdapterId as ProcessAdapterId, AdapterPaths, AdapterProcessManager, ManagerCommand,
        ManagerEvent,
    },
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    clipboard::{Clipboard, ClipboardContent},
    config::{Config, ConfigClient, EmulationBackend},
    connect::SyntraConnection,
    crypto,
    discovery::{Discovery, DiscoveryEvent},
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    file_transfer::{OutgoingFile, TransferError},
    history, history_sync,
    listen::{ListenerCreationError, SyntraListener},
    manual_transfer::{ManualAction, ManualTransfers},
    peer_profile,
    transfer_manager::{TransferAction, TransferFrontendEvent, TransferOwner, TransferState},
};
use futures::StreamExt;
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
use syntra_api::{
    AsyncFrontendListener, ClientHandle, ClipboardSettings, ClipboardTransferDirection,
    ClipboardTransferState, ClipboardTransferStatus, DeviceProfile, FrontendEvent, FrontendRequest,
    IpcError, IpcListenerCreationError, Position, Status,
};
use syntra_plugin_api::{Message as AdapterMessage, Operation};
use syntra_store::{ImportedHistoryEvent, worker::HistoryWorker};
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
    History(#[from] syntra_store::HistoryError),
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
    pending_remote_history_echoes: VecDeque<syntra_store::HistoryContent>,
    /// The RemoteDesktop portal owns clipboard observation for libei/xdp.
    /// Do not probe Wayland through arboard when that native path is selected.
    legacy_clipboard: bool,
    next_clipboard_transfer: u64,
    next_trigger_handle: u64,
    adapter_manager: Option<AdapterProcessManager>,
    adapter_events: mpsc::Receiver<ManagerEvent>,
    transfers: TransferState<Peer>,
    manual_transfers: ManualTransfers<Peer>,
    file_receive_settings: syntra_api::FileReceiveSettings,
    source_result_tx: mpsc::Sender<SourceReadResult>,
    source_results: mpsc::Receiver<SourceReadResult>,
    /// UI transfer ids that actually moved bytes and therefore exist in the UI
    announced_transfers: HashSet<syntra_api::ClipboardTransferId>,
    /// Last progress snapshot used for accurate terminal status.
    transfer_progress: HashMap<syntra_api::ClipboardTransferId, (u64, u64)>,
    /// Last local file selection. Both portal sessions can report the same
    /// owner change; it must create one offer, not an A↔B echo storm.
    last_file_clipboard: Option<String>,
    /// The native portal currently exposes a file selection. Ignore the
    /// legacy arboard text view of its URI list so it cannot replace the
    /// remote file offer with plain text.
    native_file_selection: bool,
    /// Live logging configuration, shared with the installed logger so a
    /// client can retune verbosity without restarting the daemon.
    log_config: syntra_log::LogConfig,
    /// Plugins discovered from manifests, and their supervised state.
    ///
    /// The daemon is the master here: clients render this and request
    /// changes, they never manage a plugin process themselves.
    plugins: crate::plugins::PluginRegistry,
    /// When the daemon started, reported to clients as uptime.
    started_at: std::time::Instant,
    /// Capture backend actually selected, so a client can show which one.
    capture_backend: Option<String>,
    /// Emulation backend actually selected.
    emulation_backend: Option<String>,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

const SOURCE_RESULT_CAPACITY: usize = 32;

/// Executable names of the bundled plugins, looked up beside the daemon.
///
/// They are resolved relative to the running binary rather than `PATH` so a
/// build tree, a package install and a portable directory all pick the
/// plugins that match the daemon's version.
#[cfg(windows)]
const CLIPBOARD_PLUGIN_EXECUTABLE: &str = "syntra-plugin-clipboard.exe";
#[cfg(not(windows))]
const CLIPBOARD_PLUGIN_EXECUTABLE: &str = "syntra-plugin-clipboard";
#[cfg(windows)]
const FUSE_PLUGIN_EXECUTABLE: &str = "syntra-plugin-fuse.exe";
#[cfg(not(windows))]
const FUSE_PLUGIN_EXECUTABLE: &str = "syntra-plugin-fuse";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Peer {
    Capture(ClientHandle),
    Emulation(SocketAddr),
}

/// One chunk read from a local file, handed back from the blocking reader.
struct SourceReadResult {
    owner: TransferOwner<Peer>,
    file_id: u64,
    request_id: u64,
    offset: u64,
    length: u64,
    result: Result<SourceChunk, TransferError>,
}

/// The reopened file, the chunk read from it, and the digest once complete.
///
/// The chunk is absent at end of file, and the digest appears only on the
/// read that finishes the file.
type SourceChunk = (OutgoingFile, Option<(u64, Vec<u8>)>, Option<[u8; 32]>);

impl Service {
    /// Builds the service and every subsystem it owns.
    ///
    /// `log_config` must be the handle returned by [`syntra_log::install`] so
    /// that level changes requested over the API reach the live logger.
    pub async fn new(
        config: Config,
        log_config: syntra_log::LogConfig,
    ) -> Result<Self, ServiceError> {
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
            SyntraListener::new(config.port(), cert.clone(), authorized_keys.clone()).await?;
        let conn = SyntraConnection::new(cert.clone(), client_manager.clone());

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
        // Plugins are optional by definition: they run out of process so a
        // missing or broken one degrades a single capability. Refusing to
        // start the whole service because a helper is absent would make
        // input sharing depend on a clipboard integration.
        let adapter_paths = AdapterPaths::new(
            executable_dir.join(CLIPBOARD_PLUGIN_EXECUTABLE),
            executable_dir.join(FUSE_PLUGIN_EXECUTABLE),
        );
        let (adapter_manager, adapter_events) = match adapter_paths {
            Ok(paths) => {
                let (manager, events) = AdapterProcessManager::start(paths);
                (Some(manager), events)
            }
            Err(error) => {
                log::warn!("file transfer plugins unavailable: {error}");
                let (_tx, events) = mpsc::channel(1);
                (None, events)
            }
        };
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
            adapter_manager,
            adapter_events,
            transfers: TransferState::new(),
            native_file_selection: false,
            source_result_tx,
            source_results,
            last_file_clipboard: None,
            announced_transfers: HashSet::new(),
            manual_transfers: ManualTransfers::new(),
            file_receive_settings,
            transfer_progress: HashMap::new(),
            log_config,
            started_at: std::time::Instant::now(),
            capture_backend: None,
            emulation_backend: None,
            plugins: crate::plugins::PluginRegistry::discover(
                &std::env::current_exe().unwrap_or_default(),
                syntra_api::paths::config_dir()
                    .ok()
                    .map(|directory| directory.join("plugins")),
            ),
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
}

mod clients;
mod clipboard_sync;
mod frontend;
mod history_reconcile;
mod input;
mod peers;
mod transfers;

fn hex_operation(operation_id: [u8; 16]) -> String {
    let mut value = String::with_capacity(32);
    for byte in operation_id {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}
