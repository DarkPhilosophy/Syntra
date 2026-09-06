use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect, connect_with_timeout};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind syntra socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4242;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
        }
    }
}

pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether the authenticated peer transport responds
    pub alive: bool,
    /// destination confirms both input reception and a return path are ready
    pub remote_ready: bool,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
    /// Peer's build short commit hash from the [`Hello`] proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
}

pub type ClipboardTransferId = u64;
pub type ClipboardFileId = u64;

#[derive(Debug, Default, Eq, PartialEq, Clone, Copy, Serialize, Deserialize)]
pub struct ClipboardSettings {
    pub text: bool,
    pub image: bool,
    pub files: bool,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardTransferDirection {
    Sending,
    Receiving,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardTransferState {
    Pending,
    Transferring,
    Completed,
    Cancelled,
    Failed(String),
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClipboardTransferStatus {
    pub transfer_id: ClipboardTransferId,
    pub file_id: ClipboardFileId,
    pub name: String,
    pub direction: ClipboardTransferDirection,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub bytes_per_second: u64,
    pub state: ClipboardTransferState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReceiveSettings {
    pub auto_accept: bool,
    pub download_directory: PathBuf,
}
impl Default for FileReceiveSettings {
    fn default() -> Self {
        Self {
            auto_accept: false,
            download_directory: PathBuf::new(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManualTransferState {
    Offering,
    AwaitingAcceptance,
    Transferring,
    Completed,
    Declined,
    Cancelled,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualTransferStatus {
    pub peer_fingerprint: String,
    pub transfer_id: u64,
    pub file_name: String,
    pub size: u64,
    pub transferred: u64,
    pub direction: ClipboardTransferDirection,
    pub state: ManualTransferState,
    pub destination: Option<PathBuf>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingFileOffer {
    pub peer_fingerprint: String,
    pub transfer_id: u64,
    pub file_name: String,
    pub size: u64,
    pub suggested_directory: PathBuf,
}

pub const MAX_HISTORY_PAGE_SIZE: u16 = 50;

#[derive(Debug, Eq, PartialEq, Clone, Hash, Serialize, Deserialize)]
pub struct HistoryEventId {
    pub origin_device_id: String,
    pub origin_sequence: u64,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    Text,
    Image,
    Files,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryPreview {
    Text {
        preview: String,
        truncated: bool,
    },
    Image {
        media_type: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
        size_bytes: u64,
    },
    Files {
        count: u32,
        total_size_bytes: u64,
        names: Vec<String>,
        names_truncated: bool,
    },
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct HistoryRecordSummary {
    pub event_id: HistoryEventId,
    pub created_at_ms: i64,
    pub origin_label: Option<String>,
    pub pinned: bool,
    pub kind: HistoryKind,
    pub preview: HistoryPreview,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct HistoryPage {
    pub query: String,
    pub offset: u64,
    pub next_offset: Option<u64>,
    pub records: Vec<HistoryRecordSummary>,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct HistoryImage {
    pub event_id: HistoryEventId,
    pub bytes: Vec<u8>,
    pub media_type: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// An unauthenticated service advertised by a peer through local mDNS.
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct DiscoveredPeer {
    /// Stable DNS-SD service record name.
    pub id: String,
    /// Human-readable device name supplied by the peer.
    pub display_name: String,
    /// Current addresses resolved for this service record.
    pub addresses: Vec<IpAddr>,
    /// UDP port advertised by the peer.
    pub port: u16,
}

pub const MAX_DEVICE_PROFILE_NAME_BYTES: usize = 128;
pub const MAX_PEER_AVATAR_DIMENSION: u32 = 128;

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct PeerAvatar {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub display_name: String,
    pub avatar: Option<PeerAvatar>,
}

impl DeviceProfile {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.display_name.as_bytes().len() > MAX_DEVICE_PROFILE_NAME_BYTES {
            return Err("device profile display name exceeds 128 UTF-8 bytes");
        }
        if let Some(avatar) = &self.avatar {
            if avatar.width == 0
                || avatar.height == 0
                || avatar.width > MAX_PEER_AVATAR_DIMENSION
                || avatar.height > MAX_PEER_AVATAR_DIMENSION
            {
                return Err("device profile avatar exceeds 128x128 pixels");
            }
            let expected = usize::try_from(avatar.width)
                .ok()
                .and_then(|width| {
                    usize::try_from(avatar.height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or("device profile avatar dimensions overflow")?;
            if avatar.rgba.len() != expected {
                return Err("device profile avatar RGBA length does not match dimensions");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// Current unauthenticated peers advertised through local mDNS.
    DiscoveredPeers(Vec<DiscoveredPeer>),
    /// an error occured
    Error(String),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// authorized public key fingerprints have been updated
    AuthorizedUpdated(HashMap<String, String>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// Profile associated with an authenticated certificate identity.
    PeerDeviceProfile {
        fingerprint: String,
        profile: DeviceProfile,
    },
    /// Certificate identity associated with an outgoing client route.
    ClientFingerprint {
        handle: ClientHandle,
        fingerprint: String,
    },
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt {
        fingerprint: String,
    },
    /// authoritative clipboard capability settings
    ClipboardSettings(ClipboardSettings),
    /// authoritative status for one file in a clipboard transfer
    ClipboardTransferStatus(ClipboardTransferStatus),
    /// Bounded clipboard history query result.
    /// Invalidation emitted after a visible history mutation.
    HistoryChanged,
    HistoryPage(HistoryPage),
    /// Result of changing the local pin state.
    HistoryPinResult {
        event_id: HistoryEventId,
        pinned: bool,
        updated: bool,
    },
    /// Full image payload fetched explicitly for one visible history record.
    HistoryImageResult {
        event_id: HistoryEventId,
        image: Option<HistoryImage>,
        error: Option<String>,
    },
    /// History operation failure. Global clear is reported here until peer coordination exists.
    HistoryError(String),
    /// Terminal result of an explicitly coordinated global clear.
    HistoryClearResult {
        operation_id: String,
        affected: u64,
        peers_acknowledged: u32,
        error: Option<String>,
    },
    FileReceiveSettingsChanged(FileReceiveSettings, Option<String>),
    IncomingFileOffer(IncomingFileOffer),
    ManualTransferStatus(ManualTransferStatus),
    ManualTransferError(String),
    /// authoritative global input sharing state
    InputSharing(bool),
    /// A replacement certificate was saved; active sessions change only on restart.
    IdentityRegenerated {
        fingerprint: String,
    },
    /// Authoritative logging configuration, in the `syntra-log` spec syntax
    /// (for example `info,clipboard=trace`).
    ///
    /// Emitted after any change so every attached client agrees on the level
    /// currently in force.
    LogSpec(String),
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// refresh local mDNS service discovery
    DiscoverPeers,
    /// gracefully stop the local daemon
    StopService,
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// Replace the authenticated device profile published to peers.
    SetLocalDeviceProfile(DeviceProfile),
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// save config file
    SaveConfiguration,
    /// independently enable or disable text clipboard synchronization
    SetClipboardText(bool),
    /// independently enable or disable image clipboard synchronization
    SetClipboardImage(bool),
    /// independently enable or disable file clipboard synchronization
    SetClipboardFiles(bool),
    /// cancel an in-progress clipboard transfer
    CancelClipboardTransfer(ClipboardTransferId),
    /// Query one bounded history page. The daemon clamps `limit` to 50.
    QueryHistory {
        query: String,
        offset: u64,
        limit: u16,
    },
    /// Change local pin state for one stable history event.
    SetHistoryPinned {
        event_id: HistoryEventId,
        pinned: bool,
    },
    /// Fetch one image payload separately from bounded page snapshots.
    GetHistoryImage(HistoryEventId),
    /// Request a coordinated global clear. Fails without deleting until peer coordination exists.
    ClearGlobalHistory,
    /// globally enable or disable outgoing and incoming input sharing
    SetInputSharing(bool),
    /// Explicitly confirmed replacement of the certificate used on the next restart.
    RegenerateIdentity,
    SendFiles {
        peer_fingerprint: String,
        paths: Vec<PathBuf>,
    },
    AcceptFileTransfer {
        peer_fingerprint: String,
        transfer_id: u64,
        destination_directory: PathBuf,
    },
    DeclineFileTransfer {
        peer_fingerprint: String,
        transfer_id: u64,
    },
    CancelManualTransfer {
        peer_fingerprint: String,
        transfer_id: u64,
    },
    SetFileReceiveSettings(FileReceiveSettings),
    /// Replace the daemon's logging configuration at runtime.
    ///
    /// Takes a `syntra-log` spec such as `warn,transfer=debug`. Diagnosing a
    /// live problem must not require restarting the service, because a
    /// restart discards the state that reproduces it.
    SetLogSpec(String),
    /// Ask for the logging configuration currently in force.
    QueryLogSpec,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

pub mod paths;

#[doc(inline)]
pub use paths::{PathError, PathError as SocketPathError, daemon_socket as default_socket_path};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_events_preserve_order_and_nested_payloads_through_json() {
        let expected = frontend_event_fixtures();
        let encoded = serde_json::to_vec(&expected).expect("frontend events should serialize");
        let decoded: Vec<FrontendEvent> =
            serde_json::from_slice(&encoded).expect("frontend events should deserialize");
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn frontend_requests_round_trip_through_json() {
        for expected in frontend_request_fixtures() {
            let encoded = serde_json::to_vec(&expected).unwrap();
            let decoded: FrontendRequest = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, expected);
        }
    }

    fn config() -> ClientConfig {
        ClientConfig {
            hostname: Some("nested.example".into()),
            fix_ips: vec!["192.0.2.10".parse().unwrap(), "::1".parse().unwrap()],
            port: 5353,
            pos: Position::Right,
            cmd: Some("echo entered".into()),
        }
    }

    fn state() -> ClientState {
        ClientState {
            active: true,
            active_addr: Some("192.0.2.10:5353".parse().unwrap()),
            alive: true,
            remote_ready: true,
            dns_ips: vec!["198.51.100.7".parse().unwrap()],
            ips: ["192.0.2.10".parse().unwrap()].into_iter().collect(),
            has_pressed_keys: true,
            resolving: true,
            peer_commit: Some(*b"12345678"),
        }
    }

    fn frontend_event_fixtures() -> Vec<FrontendEvent> {
        let client = (42, config(), state());
        vec![
            FrontendEvent::Created(client.0, client.1.clone(), client.2.clone()),
            FrontendEvent::NoSuchClient(43),
            FrontendEvent::State(client.0, client.1.clone(), client.2.clone()),
            FrontendEvent::Deleted(44),
            FrontendEvent::PortChanged(4243, Some("port occupied".into())),
            FrontendEvent::Enumerate(vec![
                client.clone(),
                (45, ClientConfig::default(), ClientState::default()),
            ]),
            FrontendEvent::Error("descriptive error".into()),
            FrontendEvent::CaptureStatus(Status::Enabled),
            FrontendEvent::EmulationStatus(Status::Disabled),
            FrontendEvent::AuthorizedUpdated(
                [
                    ("alice".into(), "SHA256:abc".into()),
                    ("bob".into(), "SHA256:def".into()),
                ]
                .into_iter()
                .collect(),
            ),
            FrontendEvent::PublicKeyFingerprint("SHA256:fingerprint".into()),
            FrontendEvent::DeviceConnected {
                addr: "203.0.113.8:4242".parse().unwrap(),
                fingerprint: "SHA256:device".into(),
            },
            FrontendEvent::DeviceEntered {
                fingerprint: "SHA256:entering".into(),
                addr: "[2001:db8::8]:4242".parse().unwrap(),
                pos: Position::Bottom,
            },
            FrontendEvent::IncomingDisconnected("203.0.113.9:4242".parse().unwrap()),
            FrontendEvent::ConnectionAttempt {
                fingerprint: "SHA256:attempt".into(),
            },
            FrontendEvent::InputSharing(false),
            FrontendEvent::ClipboardSettings(ClipboardSettings {
                text: true,
                image: false,
                files: true,
            }),
            FrontendEvent::ClipboardTransferStatus(ClipboardTransferStatus {
                transfer_id: 9,
                file_id: 10,
                name: "photo.png".into(),
                direction: ClipboardTransferDirection::Receiving,
                transferred_bytes: 128,
                total_bytes: 1024,
                bytes_per_second: 64,
                state: ClipboardTransferState::Failed("checksum mismatch".into()),
            }),
            FrontendEvent::DiscoveredPeers(vec![DiscoveredPeer {
                id: "peer._syntra._udp.local.".into(),
                display_name: "peer".into(),
                addresses: vec!["192.0.2.20".parse().unwrap()],
                port: 4242,
            }]),
        ]
    }

    fn frontend_request_fixtures() -> Vec<FrontendRequest> {
        vec![
            FrontendRequest::Activate(42, true),
            FrontendRequest::Create,
            FrontendRequest::ChangePort(5353),
            FrontendRequest::Delete(43),
            FrontendRequest::Enumerate(),
            FrontendRequest::ResolveDns(42),
            FrontendRequest::UpdateHostname(42, Some("host".into())),
            FrontendRequest::UpdatePort(42, 5353),
            FrontendRequest::DiscoverPeers,
            FrontendRequest::StopService,
            FrontendRequest::UpdateFixIps(
                42,
                vec!["192.0.2.1".parse().unwrap(), "::1".parse().unwrap()],
            ),
            FrontendRequest::EnableCapture,
            FrontendRequest::EnableEmulation,
            FrontendRequest::SetInputSharing(false),
            FrontendRequest::Sync,
            FrontendRequest::AuthorizeKey("alice".into(), "SHA256:key".into()),
            FrontendRequest::RemoveAuthorizedKey("SHA256:key".into()),
            FrontendRequest::UpdateEnterHook(42, Some("notify-send".into())),
            FrontendRequest::SaveConfiguration,
            FrontendRequest::SetClipboardText(true),
            FrontendRequest::SetClipboardImage(false),
            FrontendRequest::SetClipboardFiles(true),
            FrontendRequest::CancelClipboardTransfer(9),
        ]
    }
}
