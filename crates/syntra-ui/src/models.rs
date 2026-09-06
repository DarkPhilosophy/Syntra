use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use syntra_api::{
    ClientConfig, ClientHandle, ClientState, ClipboardFileId, ClipboardSettings,
    ClipboardTransferId, ClipboardTransferStatus, FrontendEvent, FrontendRequest, Position,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Navigation {
    pub page: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputHealth {
    pub capture: bool,
    pub emulation: bool,
}

/// Service health surfaced in the diagnostics page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// Most recent error reported by the daemon or the transport.
    pub last_error: Option<String>,
    /// Port the daemon currently listens on.
    pub port: u16,
    /// Client the daemon reported as unknown, if any.
    pub missing_client: Option<ClientHandle>,
    /// Logging configuration in force on the daemon, in `syntra-log` syntax.
    ///
    /// Empty until the daemon answers, so the page can distinguish
    /// "not yet known" from "no overrides".
    pub log_spec: String,
}

/// Whether the dashboard currently has a usable daemon connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TransportLifecycle {
    #[default]
    Unavailable,
    Reconnecting,
    Ready,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppStatus {
    pub connected: bool,
    pub reconnecting: bool,
    pub transport: TransportLifecycle,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    pub locale: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlatformCapabilities {
    pub capture: bool,
    pub emulation: bool,
    pub clipboard_files: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientView {
    pub handle: ClientHandle,
    pub hostname: Option<String>,
    pub fixed_ips: Vec<std::net::IpAddr>,
    pub port: u16,
    pub position: Position,
    pub enter_hook: Option<String>,
    pub active: bool,
    pub active_addr: Option<std::net::SocketAddr>,
    pub alive: bool,
    pub remote_ready: bool,
    pub dns_ips: Vec<std::net::IpAddr>,
    pub ips: Vec<std::net::IpAddr>,
    pub has_pressed_keys: bool,
    pub resolving: bool,
    pub peer_commit: Option<[u8; 8]>,
}
impl ClientView {
    fn from_wire(handle: ClientHandle, c: ClientConfig, s: ClientState) -> Self {
        let mut ips = s.ips.into_iter().collect::<Vec<_>>();
        ips.sort();
        Self {
            handle,
            hostname: c.hostname,
            fixed_ips: c.fix_ips,
            port: c.port,
            position: c.pos,
            enter_hook: c.cmd,
            active: s.active,
            active_addr: s.active_addr,
            alive: s.alive,
            remote_ready: s.remote_ready,
            dns_ips: s.dns_ips,
            ips,
            has_pressed_keys: s.has_pressed_keys,
            resolving: s.resolving,
            peer_commit: s.peer_commit,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceView {
    pub fingerprint: String,
    pub position: Option<Position>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionAttemptView {
    pub fingerprint: String,
}
#[derive(Clone, Debug, Default)]
pub struct AppViewState {
    pub navigation: Navigation,
    /// Plugins reported by the daemon, in its display order.
    ///
    /// Held verbatim: the daemon is authoritative, so the interface renders
    /// this rather than deriving plugin state of its own.
    pub plugins: Vec<syntra_api::PluginStatus>,
    pub clients: BTreeMap<ClientHandle, ClientView>,
    pub client_fingerprints: BTreeMap<ClientHandle, String>,
    pub peer_profiles: BTreeMap<String, Arc<syntra_api::DeviceProfile>>,
    pub discovered_peers: Vec<syntra_api::DiscoveredPeer>,
    pub authorization: HashMap<String, String>,
    pub input_health: InputHealth,
    pub input_sharing: Option<bool>,
    pub clipboard: ClipboardSettings,
    pub history_page: Option<syntra_api::HistoryPage>,
    pub history_query: String,
    pub history_error: Option<String>,
    pub history_clear_result: Option<(u64, u32)>,
    pub history_clear_error: Option<String>,
    pub history_query_pending: bool,
    pub history_clear_pending: bool,
    pub history_refresh_needed: bool,
    pub history_images: BTreeMap<(String, u64), Arc<syntra_api::HistoryImage>>,
    pub transfers: BTreeMap<(ClipboardTransferId, ClipboardFileId), ClipboardTransferStatus>,
    pub file_receive_settings: Option<syntra_api::FileReceiveSettings>,
    pub file_receive_error: Option<String>,
    pub file_receive_pending: bool,
    pub manual_transfers: BTreeMap<(String, u64), syntra_api::ManualTransferStatus>,
    pub pending_file_offers: Vec<syntra_api::IncomingFileOffer>,
    pub manual_transfer_error: Option<String>,
    pub diagnostics: Diagnostics,
    pub status: AppStatus,
    pub preferences: Preferences,
    pub capabilities: PlatformCapabilities,
    pub public_key_fingerprint: Option<String>,
    pub pending_identity_fingerprint: Option<String>,
    pub connection_attempt: Option<ConnectionAttemptView>,
    pub connected_devices: BTreeMap<std::net::SocketAddr, DeviceView>,
    pub incoming_devices: BTreeMap<std::net::SocketAddr, DeviceView>,
    pub last_disconnected_device: Option<std::net::SocketAddr>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportLifecycleEvent {
    DaemonUnavailable,
    Reconnecting,
    Resynchronized,
}
impl AppViewState {
    pub fn next_history_refresh(&mut self) -> Option<FrontendRequest> {
        if self.navigation.page != "history"
            || !self.status.connected
            || self.history_query_pending
            || !self.history_refresh_needed
        {
            return None;
        }
        self.history_refresh_needed = false;
        self.history_query_pending = true;
        Some(FrontendRequest::QueryHistory {
            query: self.history_query.clone(),
            offset: 0,
            limit: 50,
        })
    }

    pub fn reduce_transport(&mut self, e: TransportLifecycleEvent) {
        self.status.transport = match e {
            TransportLifecycleEvent::DaemonUnavailable => TransportLifecycle::Unavailable,
            TransportLifecycleEvent::Reconnecting => TransportLifecycle::Reconnecting,
            TransportLifecycleEvent::Resynchronized => TransportLifecycle::Ready,
        };
        self.status.connected = matches!(self.status.transport, TransportLifecycle::Ready);
        self.status.reconnecting =
            matches!(self.status.transport, TransportLifecycle::Reconnecting);
        if !self.status.connected {
            self.history_query_pending = false;
            self.history_clear_pending = false;
            self.history_refresh_needed = true;
            self.file_receive_pending = false;
            self.file_receive_settings = None;
            self.pending_file_offers.clear();
            self.manual_transfers.retain(|_, transfer| {
                matches!(
                    transfer.state,
                    syntra_api::ManualTransferState::Completed
                        | syntra_api::ManualTransferState::Declined
                        | syntra_api::ManualTransferState::Cancelled
                        | syntra_api::ManualTransferState::Failed
                )
            });
        }
    }
    pub fn reduce(&mut self, e: FrontendEvent) {
        match e {
            FrontendEvent::Created(h, c, s) | FrontendEvent::State(h, c, s) => {
                self.clients.insert(h, ClientView::from_wire(h, c, s));
            }
            FrontendEvent::Deleted(h) => {
                self.clients.remove(&h);
                self.client_fingerprints.remove(&h);
            }
            FrontendEvent::NoSuchClient(h) => {
                self.diagnostics.missing_client = Some(h);
            }
            FrontendEvent::Enumerate(xs) => {
                self.clients = xs
                    .into_iter()
                    .map(|(h, c, s)| (h, ClientView::from_wire(h, c, s)))
                    .collect();
            }
            FrontendEvent::DiscoveredPeers(peers) => self.discovered_peers = peers,
            FrontendEvent::ClientFingerprint {
                handle,
                fingerprint,
            } => {
                self.client_fingerprints.insert(handle, fingerprint);
            }
            FrontendEvent::PeerDeviceProfile {
                fingerprint,
                profile,
            } => {
                if profile.validate().is_ok() {
                    self.peer_profiles.insert(fingerprint, Arc::new(profile));
                }
            }
            FrontendEvent::IdentityRegenerated { fingerprint } => {
                self.pending_identity_fingerprint = Some(fingerprint);
            }
            FrontendEvent::PortChanged(p, e) => {
                self.diagnostics.port = p;
                self.diagnostics.last_error = e;
            }
            FrontendEvent::Error(e) => self.diagnostics.last_error = Some(e),
            FrontendEvent::FileReceiveSettingsChanged(settings, error) => {
                self.file_receive_settings = Some(settings);
                self.file_receive_error = error;
                self.file_receive_pending = false;
            }
            FrontendEvent::IncomingFileOffer(offer) => {
                if let Some(existing) = self.pending_file_offers.iter_mut().find(|existing| {
                    existing.peer_fingerprint == offer.peer_fingerprint
                        && existing.transfer_id == offer.transfer_id
                }) {
                    *existing = offer;
                } else {
                    self.pending_file_offers.push(offer);
                }
            }
            FrontendEvent::ManualTransferStatus(status) => {
                if !matches!(
                    status.state,
                    syntra_api::ManualTransferState::AwaitingAcceptance
                ) {
                    self.pending_file_offers.retain(|offer| {
                        offer.peer_fingerprint != status.peer_fingerprint
                            || offer.transfer_id != status.transfer_id
                    });
                }
                self.manual_transfers.insert(
                    (status.peer_fingerprint.clone(), status.transfer_id),
                    status,
                );
                while self.manual_transfers.len() > 132 {
                    let oldest = self
                        .manual_transfers
                        .iter()
                        .filter(|(_, status)| {
                            matches!(
                                status.state,
                                syntra_api::ManualTransferState::Completed
                                    | syntra_api::ManualTransferState::Declined
                                    | syntra_api::ManualTransferState::Cancelled
                                    | syntra_api::ManualTransferState::Failed
                            )
                        })
                        .min_by_key(|(_, status)| status.transfer_id)
                        .map(|(key, _)| key.clone());
                    let Some(oldest) = oldest else { break };
                    self.manual_transfers.remove(&oldest);
                }
            }
            FrontendEvent::ManualTransferError(error) => self.manual_transfer_error = Some(error),
            FrontendEvent::HistoryChanged => self.history_refresh_needed = true,
            FrontendEvent::HistoryPage(mut page) => {
                self.history_query_pending = false;
                if page.query == self.history_query {
                    if page.offset > 0 {
                        if let Some(previous) = self
                            .history_page
                            .take()
                            .filter(|previous| previous.query == page.query)
                        {
                            let mut records = previous.records;
                            for record in page.records {
                                if !records
                                    .iter()
                                    .any(|existing| existing.event_id == record.event_id)
                                {
                                    records.push(record);
                                }
                            }
                            page.records = records;
                            page.offset = 0;
                        }
                    }
                    self.history_images.retain(|key, _| {
                        page.records.iter().any(|record| {
                            record.event_id.origin_device_id == key.0
                                && record.event_id.origin_sequence == key.1
                        })
                    });
                    self.history_page = Some(page);
                    self.history_error = None;
                }
            }
            FrontendEvent::HistoryPinResult {
                event_id,
                pinned,
                updated,
            } => {
                if updated {
                    if let Some(page) = self.history_page.as_mut() {
                        if let Some(record) = page
                            .records
                            .iter_mut()
                            .find(|record| record.event_id == event_id)
                        {
                            record.pinned = pinned;
                        }
                    }
                }
                self.history_refresh_needed = true;
            }
            FrontendEvent::HistoryImageResult {
                event_id,
                image,
                error,
            } => {
                self.history_error = error;
                if let Some(image) = image {
                    if self.history_page.as_ref().is_some_and(|page| {
                        page.records
                            .iter()
                            .any(|record| record.event_id == event_id)
                    }) {
                        self.history_images.insert(
                            (event_id.origin_device_id, event_id.origin_sequence),
                            Arc::new(image),
                        );
                    }
                }
            }
            FrontendEvent::HistoryClearResult {
                affected,
                peers_acknowledged,
                error,
                ..
            } => {
                self.history_clear_result = Some((affected, peers_acknowledged));
                self.history_clear_error = error;
                self.history_clear_pending = false;
                self.history_refresh_needed = true;
            }
            FrontendEvent::HistoryError(error) => {
                self.history_error = Some(error);
                self.history_query_pending = false;
                self.history_clear_pending = false;
                self.history_refresh_needed = false;
            }
            FrontendEvent::CaptureStatus(s) => self.input_health.capture = bool::from(s),
            FrontendEvent::InputSharing(enabled) => self.input_sharing = Some(enabled),
            FrontendEvent::EmulationStatus(s) => self.input_health.emulation = bool::from(s),
            FrontendEvent::AuthorizedUpdated(a) => self.authorization = a,
            FrontendEvent::PublicKeyFingerprint(k) => self.public_key_fingerprint = Some(k),
            FrontendEvent::DeviceConnected { addr, fingerprint } => {
                self.connected_devices.insert(
                    addr,
                    DeviceView {
                        fingerprint,
                        position: None,
                    },
                );
            }
            FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            } => {
                self.incoming_devices.insert(
                    addr,
                    DeviceView {
                        fingerprint,
                        position: Some(pos),
                    },
                );
            }
            FrontendEvent::IncomingDisconnected(addr) => {
                self.connected_devices.remove(&addr);
                self.incoming_devices.remove(&addr);
                self.last_disconnected_device = Some(addr);
            }
            FrontendEvent::ConnectionAttempt { fingerprint } => {
                self.connection_attempt = Some(ConnectionAttemptView { fingerprint })
            }
            FrontendEvent::ClipboardSettings(s) => self.clipboard = s,
            FrontendEvent::ClipboardTransferStatus(s) => {
                self.transfers.insert((s.transfer_id, s.file_id), s);
            }
            FrontendEvent::LogSpec(spec) => self.diagnostics.log_spec = spec,
            FrontendEvent::Plugins(plugins) => self.plugins = plugins,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_offer_replay_and_decision_preserve_other_peers() {
        let first = syntra_api::IncomingFileOffer {
            peer_fingerprint: "peer-a".into(),
            transfer_id: 7,
            file_name: "example.txt".into(),
            size: 4,
            suggested_directory: "/synthetic/downloads".into(),
        };
        let second = syntra_api::IncomingFileOffer {
            peer_fingerprint: "peer-b".into(),
            ..first.clone()
        };
        let mut view = AppViewState::default();
        view.reduce(FrontendEvent::IncomingFileOffer(first.clone()));
        view.reduce(FrontendEvent::IncomingFileOffer(first.clone()));
        view.reduce(FrontendEvent::IncomingFileOffer(second.clone()));
        let pending = syntra_api::ManualTransferStatus {
            peer_fingerprint: first.peer_fingerprint.clone(),
            transfer_id: first.transfer_id,
            file_name: first.file_name.clone(),
            size: first.size,
            transferred: 0,
            direction: syntra_api::ClipboardTransferDirection::Receiving,
            state: syntra_api::ManualTransferState::AwaitingAcceptance,
            destination: None,
            error: Some("Choose another destination".into()),
        };
        view.reduce(FrontendEvent::ManualTransferStatus(pending.clone()));
        assert_eq!(view.pending_file_offers, vec![first, second.clone()]);
        view.reduce(FrontendEvent::ManualTransferStatus(
            syntra_api::ManualTransferStatus {
                state: syntra_api::ManualTransferState::Transferring,
                error: None,
                ..pending
            },
        ));
        assert_eq!(view.pending_file_offers, vec![second]);
    }

    #[test]
    fn disconnected_incoming_peer_is_not_left_connected() {
        let mut view = AppViewState::default();
        let address = "127.0.0.1:12345".parse().unwrap();
        view.reduce(FrontendEvent::DeviceConnected {
            addr: address,
            fingerprint: "peer".into(),
        });
        view.reduce(FrontendEvent::IncomingDisconnected(address));
        assert!(view.connected_devices.is_empty());
        assert_eq!(view.last_disconnected_device, Some(address));
    }

    #[test]
    fn lost_service_clears_stale_incoming_file_prompts() {
        let mut view = AppViewState::default();
        view.reduce(FrontendEvent::IncomingFileOffer(
            syntra_api::IncomingFileOffer {
                peer_fingerprint: "peer".into(),
                transfer_id: 9,
                file_name: "example.txt".into(),
                size: 1,
                suggested_directory: "/synthetic/downloads".into(),
            },
        ));
        view.reduce_transport(TransportLifecycleEvent::DaemonUnavailable);
        assert!(view.pending_file_offers.is_empty());
    }

    fn history_page(query: &str, sequences: &[u64]) -> syntra_api::HistoryPage {
        syntra_api::HistoryPage {
            query: query.into(),
            offset: 0,
            next_offset: None,
            records: sequences
                .iter()
                .map(|sequence| syntra_api::HistoryRecordSummary {
                    event_id: syntra_api::HistoryEventId {
                        origin_device_id: "synthetic-peer".into(),
                        origin_sequence: *sequence,
                    },
                    created_at_ms: 1,
                    origin_label: Some("Synthetic peer".into()),
                    pinned: *sequence == 2,
                    kind: syntra_api::HistoryKind::Text,
                    preview: syntra_api::HistoryPreview::Text {
                        preview: "needle".into(),
                        truncated: false,
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn clear_refreshes_filtered_records_and_preserves_partial_failure() {
        let mut view = AppViewState::default();
        view.navigation.page = "history".into();
        view.history_query = "needle".into();
        view.reduce_transport(TransportLifecycleEvent::Resynchronized);
        view.reduce(FrontendEvent::HistoryPage(history_page("needle", &[1, 2])));
        view.history_clear_pending = true;
        view.reduce(FrontendEvent::InputSharing(false));
        assert!(view.history_clear_pending);
        view.reduce(FrontendEvent::HistoryClearResult {
            operation_id: "synthetic-clear".into(),
            affected: 1,
            peers_acknowledged: 0,
            error: Some("One peer did not acknowledge".into()),
        });
        assert!(matches!(view.next_history_refresh(),
            Some(FrontendRequest::QueryHistory { query, offset: 0, .. }) if query == "needle"));
        view.reduce(FrontendEvent::HistoryPage(history_page("needle", &[2])));
        assert_eq!(
            view.history_page
                .as_ref()
                .unwrap()
                .records
                .iter()
                .map(|record| record.event_id.origin_sequence)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(
            view.history_clear_error.as_deref(),
            Some("One peer did not acknowledge")
        );
        assert!(!view.history_query_pending && !view.history_clear_pending);
    }

    #[test]
    fn clipboard_invalidations_coalesce_and_refresh_latest_search() {
        let mut view = AppViewState::default();
        view.navigation.page = "history".into();
        view.reduce_transport(TransportLifecycleEvent::Resynchronized);
        view.reduce(FrontendEvent::HistoryChanged);
        assert!(view.next_history_refresh().is_some());
        view.history_query = "needle".into();
        view.reduce(FrontendEvent::HistoryChanged);
        view.reduce(FrontendEvent::HistoryChanged);
        assert!(view.next_history_refresh().is_none());
        view.reduce(FrontendEvent::HistoryPage(history_page("", &[1])));
        assert!(view.history_page.is_none());
        assert!(matches!(view.next_history_refresh(),
            Some(FrontendRequest::QueryHistory { query, offset: 0, .. }) if query == "needle"));
        view.reduce(FrontendEvent::HistoryPage(history_page("needle", &[2])));
        assert!(view.next_history_refresh().is_none());
        assert_eq!(
            view.history_page.as_ref().unwrap().records[0]
                .event_id
                .origin_sequence,
            2
        );
    }

    #[test]
    fn enumerate_replaces_stale_clients_deterministically() {
        let mut s = AppViewState::default();
        s.reduce(FrontendEvent::Created(
            9,
            ClientConfig::default(),
            ClientState::default(),
        ));
        s.reduce(FrontendEvent::Enumerate(vec![
            (2, ClientConfig::default(), ClientState::default()),
            (1, ClientConfig::default(), ClientState::default()),
        ]));
        assert_eq!(s.clients.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use syntra_api::{
        ClipboardTransferDirection, ClipboardTransferState, ClipboardTransferStatus, Status,
    };
    #[test]
    fn event_families_reduce_without_transport_conflation() {
        let mut s = AppViewState::default();
        s.reduce(FrontendEvent::PortChanged(42, Some("busy".into())));
        s.reduce(FrontendEvent::Error("oops".into()));
        s.reduce(FrontendEvent::CaptureStatus(Status::Enabled));
        s.reduce(FrontendEvent::EmulationStatus(Status::Enabled));
        s.reduce(FrontendEvent::AuthorizedUpdated(HashMap::from([(
            "fp".into(),
            "name".into(),
        )])));
        s.reduce(FrontendEvent::PublicKeyFingerprint("local".into()));
        assert_eq!(s.diagnostics.port, 42);
        assert_eq!(s.diagnostics.last_error.as_deref(), Some("oops"));
        assert!(s.input_health.capture && s.input_health.emulation);
        s.reduce_transport(TransportLifecycleEvent::DaemonUnavailable);
        assert_eq!(s.status.transport, TransportLifecycle::Unavailable);
    }
    #[test]
    fn connection_context_lifecycle_and_composite_transfers() {
        let mut s = AppViewState::default();
        s.reduce(FrontendEvent::ConnectionAttempt {
            fingerprint: "attempt".into(),
        });
        assert_eq!(
            s.connection_attempt.as_ref().unwrap().fingerprint,
            "attempt"
        );
        for e in [
            TransportLifecycleEvent::DaemonUnavailable,
            TransportLifecycleEvent::Reconnecting,
            TransportLifecycleEvent::Resynchronized,
        ] {
            s.reduce_transport(e);
        }
        let mk = |f, state| ClipboardTransferStatus {
            transfer_id: 4,
            file_id: f,
            name: "x".into(),
            direction: ClipboardTransferDirection::Receiving,
            transferred_bytes: 1,
            total_bytes: 2,
            bytes_per_second: 1,
            state,
        };
        s.reduce(FrontendEvent::ClipboardTransferStatus(mk(
            1,
            ClipboardTransferState::Transferring,
        )));
        s.reduce(FrontendEvent::ClipboardTransferStatus(mk(
            2,
            ClipboardTransferState::Completed,
        )));
        assert_eq!(s.transfers.len(), 2);
    }
}
