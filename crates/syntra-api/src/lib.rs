//! The Syntra control contract: the only sanctioned boundary between the
//! daemon and everything that talks to it.
//!
//! Clients — the desktop dashboard, the CLI and third-party plugins — send
//! [`FrontendRequest`] and receive [`FrontendEvent`]. Both are encoded as
//! newline-delimited JSON over a Unix socket (a loopback socket on Windows),
//! chosen so a plugin can be written in any language without linking Rust.
//!
//! This crate deliberately depends on no other workspace crate. A plugin
//! author compiles against it alone, and an architecture test enforces that
//! property so it cannot regress.
//!
//! # Layout
//!
//! * [`FrontendRequest`] — everything a client may ask the daemon to do.
//! * [`FrontendEvent`] — everything the daemon reports back. The daemon is
//!   authoritative: clients render these rather than predicting outcomes.
//! * [`paths`] — every runtime-visible socket, directory and identifier,
//!   each with an environment override.
//!
//! # Connecting
//!
//! [`connect`] and [`connect_with_timeout`] open a blocking client pair,
//! [`connect_async`] an async one, and [`AsyncFrontendListener`] is the
//! daemon's accept side.

use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

// Path resolution moved to `paths`; only the socket type is still needed here.
#[cfg(unix)]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect, connect_with_timeout};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
/// Describes why opening the frontend IPC connection failed.
pub enum ConnectionError {
    #[error(transparent)]
    /// This variant reports or requests the socketpath protocol state.
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    /// This variant reports or requests the io protocol state.
    Io(#[from] io::Error),
    #[error("connection timed out")]
    /// This variant reports or requests the timeout protocol state.
    Timeout,
}

#[derive(Debug, Error)]
/// Describes why the daemon could not create its frontend IPC listener.
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    /// This variant reports or requests the socketpath protocol state.
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    /// This variant reports or requests the alreadyrunning protocol state.
    AlreadyRunning,
    #[error("failed to bind syntra socket: `{0}`")]
    /// This variant reports or requests the bind protocol state.
    Bind(io::Error),
}

#[derive(Debug, Error)]
/// Describes an error encountered while transporting or encoding the frontend IPC protocol.
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    /// This variant reports or requests the io protocol state.
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    /// This variant reports or requests the json protocol state.
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    /// This variant reports or requests the connection protocol state.
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    /// This variant reports or requests the listen protocol state.
    Listen(#[from] IpcListenerCreationError),
}

/// The default UDP port used when no client or daemon port is configured.
pub const DEFAULT_PORT: u16 = 4242;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
/// Identifies the edge of the display where a client is positioned.
pub enum Position {
    #[default]
    /// This variant reports or requests the left protocol state.
    Left,
    /// This variant reports or requests the right protocol state.
    Right,
    /// This variant reports or requests the top protocol state.
    Top,
    /// This variant reports or requests the bottom protocol state.
    Bottom,
}

impl Position {
    /// Returns the opposite display edge, preserving the directional relationship.
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
/// Reports that text could not be converted to a supported display position.
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
/// Configuration used by the daemon to address and position one client.
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

/// Identifies a client routing target; a handle is never reused after its client is deleted.
pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
/// Authoritative runtime state for one configured client.
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
    /// Peer's build short commit hash from the the Hello protocol proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
}

/// Identifies one clipboard transfer across frontend status events.
pub type ClipboardTransferId = u64;
/// Identifies one file within a clipboard transfer.
pub type ClipboardFileId = u64;

#[derive(Debug, Default, Eq, PartialEq, Clone, Copy, Serialize, Deserialize)]
/// Selects which clipboard content categories the daemon synchronises.
pub struct ClipboardSettings {
    /// Whether text clipboard synchronisation is enabled.
    pub text: bool,
    /// Whether image clipboard synchronisation is enabled.
    pub image: bool,
    /// Whether file clipboard synchronisation is enabled.
    pub files: bool,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// States whether clipboard bytes flow to or from the local client.
pub enum ClipboardTransferDirection {
    /// This variant reports or requests the sending protocol state.
    Sending,
    /// This variant reports or requests the receiving protocol state.
    Receiving,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Reports the lifecycle outcome of a clipboard file transfer.
pub enum ClipboardTransferState {
    /// This variant reports or requests the pending protocol state.
    Pending,
    /// This variant reports or requests the transferring protocol state.
    Transferring,
    /// This variant reports or requests the completed protocol state.
    Completed,
    /// This variant reports or requests the cancelled protocol state.
    Cancelled,
    /// This variant reports or requests the failed protocol state.
    Failed(String),
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
/// Authoritative progress snapshot for one file in a clipboard transfer.
pub struct ClipboardTransferStatus {
    /// Transfer identity used to correlate progress and terminal events.
    pub transfer_id: ClipboardTransferId,
    /// File identity used to distinguish files within a transfer.
    pub file_id: ClipboardFileId,
    /// Name presented for the transferred file.
    pub name: String,
    /// Direction of byte flow relative to the local daemon.
    pub direction: ClipboardTransferDirection,
    /// Number of file bytes transferred so far.
    pub transferred_bytes: u64,
    /// Total file size used to calculate progress.
    pub total_bytes: u64,
    /// Current transfer rate reported to the frontend.
    pub bytes_per_second: u64,
    /// Current authoritative transfer lifecycle state.
    pub state: ClipboardTransferState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Controls whether incoming manual files are accepted automatically and where they are saved.
pub struct FileReceiveSettings {
    /// Whether incoming files are accepted without a frontend decision.
    pub auto_accept: bool,
    /// Default directory used for automatically received files.
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
/// Reports the lifecycle stage of a manually initiated file transfer.
pub enum ManualTransferState {
    /// This variant reports or requests the offering protocol state.
    Offering,
    /// This variant reports or requests the awaitingacceptance protocol state.
    AwaitingAcceptance,
    /// This variant reports or requests the transferring protocol state.
    Transferring,
    /// This variant reports or requests the completed protocol state.
    Completed,
    /// This variant reports or requests the declined protocol state.
    Declined,
    /// This variant reports or requests the cancelled protocol state.
    Cancelled,
    /// This variant reports or requests the failed protocol state.
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Authoritative progress and outcome for a manual file transfer.
pub struct ManualTransferStatus {
    /// Certificate fingerprint identifying the remote peer.
    pub peer_fingerprint: String,
    /// Transfer identity used to correlate progress and terminal events.
    pub transfer_id: u64,
    /// Name of the file offered or transferred.
    pub file_name: String,
    /// Total size of the offered or transferred file in bytes.
    pub size: u64,
    /// Number of file bytes transferred so far.
    pub transferred: u64,
    /// Direction of byte flow relative to the local daemon.
    pub direction: ClipboardTransferDirection,
    /// Current authoritative transfer lifecycle state.
    pub state: ManualTransferState,
    /// Directory selected for the received file, when accepted.
    pub destination: Option<PathBuf>,
    /// Failure detail when the transfer cannot complete.
    pub error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Describes an incoming file awaiting the frontend acceptance decision.
pub struct IncomingFileOffer {
    /// Certificate fingerprint identifying the remote peer.
    pub peer_fingerprint: String,
    /// Transfer identity used to correlate progress and terminal events.
    pub transfer_id: u64,
    /// Name of the file offered or transferred.
    pub file_name: String,
    /// Total size of the offered or transferred file in bytes.
    pub size: u64,
    /// Directory suggested to the frontend for saving the offered file.
    pub suggested_directory: PathBuf,
}

/// The largest history page the daemon will return; requests above this bound are clamped to keep IPC responses bounded.
pub const MAX_HISTORY_PAGE_SIZE: u16 = 50;

#[derive(Debug, Eq, PartialEq, Clone, Hash, Serialize, Deserialize)]
/// Stable wire-protocol identity for a history event, combining its origin device and sequence.
pub struct HistoryEventId {
    /// Stable identifier of the device that created the history event.
    pub origin_device_id: String,
    /// Origin-local sequence number that completes the history event identity.
    pub origin_sequence: u64,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Identifies the content category represented by a history record.
pub enum HistoryKind {
    /// This variant reports or requests the text protocol state.
    Text,
    /// This variant reports or requests the image protocol state.
    Image,
    /// This variant reports or requests the files protocol state.
    Files,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Bounded metadata shown in a history page without transferring the complete payload.
pub enum HistoryPreview {
    /// This variant reports or requests the text protocol state.
    Text {
        /// Carries the preview for this protocol variant.
        preview: String,
        /// Carries the truncated for this protocol variant.
        truncated: bool,
    },
    /// This variant reports or requests the image protocol state.
    Image {
        /// Carries the media type for this protocol variant.
        media_type: Option<String>,
        /// Carries the width for this protocol variant.
        width: Option<u32>,
        /// Carries the height for this protocol variant.
        height: Option<u32>,
        /// Carries the size bytes for this protocol variant.
        size_bytes: u64,
    },
    /// This variant reports or requests the files protocol state.
    Files {
        /// Carries the count for this protocol variant.
        count: u32,
        /// Carries the total size bytes for this protocol variant.
        total_size_bytes: u64,
        /// Carries the names for this protocol variant.
        names: Vec<String>,
        /// Carries the names truncated for this protocol variant.
        names_truncated: bool,
    },
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
/// Compact history record data suitable for a bounded page response.
pub struct HistoryRecordSummary {
    /// Stable identity of the history event.
    pub event_id: HistoryEventId,
    /// Creation timestamp represented as milliseconds since the Unix epoch.
    pub created_at_ms: i64,
    /// Optional human-readable label for the originating device.
    pub origin_label: Option<String>,
    /// Whether the history record is marked for retention or quick access.
    pub pinned: bool,
    /// Content category of the history record.
    pub kind: HistoryKind,
    /// Bounded content summary displayed for a text history record.
    pub preview: HistoryPreview,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
/// A bounded, ordered result page for a history query.
pub struct HistoryPage {
    /// Search text used to produce this page.
    pub query: String,
    /// Starting position of this page in the matching history results.
    pub offset: u64,
    /// Position to request for the next page, when more results exist.
    pub next_offset: Option<u64>,
    /// History summaries included in this page.
    pub records: Vec<HistoryRecordSummary>,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
/// Complete image bytes and metadata fetched for one history event.
pub struct HistoryImage {
    /// Stable identity of the history event.
    pub event_id: HistoryEventId,
    /// Encoded image payload bytes.
    pub bytes: Vec<u8>,
    /// Optional MIME type for the image payload.
    pub media_type: Option<String>,
    /// Image width in pixels when known.
    pub width: Option<u32>,
    /// Image height in pixels when known.
    pub height: Option<u32>,
}

/// An unauthenticated service advertised by a peer through local mDNS.
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
/// Defines the DiscoveredPeer value carried by this public API type.
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

/// Defines the MAX DEVICE PROFILE NAME BYTES value carried by this public API type.
pub const MAX_DEVICE_PROFILE_NAME_BYTES: usize = 128;
/// Defines the MAX PEER AVATAR DIMENSION value carried by this public API type.
pub const MAX_PEER_AVATAR_DIMENSION: u32 = 128;

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
/// RGBA image data used as the authenticated device avatar.
pub struct PeerAvatar {
    /// Image width in pixels when known.
    pub width: u32,
    /// Image height in pixels when known.
    pub height: u32,
    /// Raw RGBA pixels, with four bytes required per pixel.
    pub rgba: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
/// Identity information published by this device to authenticated peers.
pub struct DeviceProfile {
    /// Human-readable name published to authenticated peers.
    pub display_name: String,
    /// Optional image published with the device profile.
    pub avatar: Option<PeerAvatar>,
}

impl DeviceProfile {
    /// Checks profile limits and RGBA dimensions before the daemon publishes the profile.
    pub fn validate(&self) -> Result<(), &'static str> {
        // `len()` is already the UTF-8 byte count, which is the bound the
        // wire format imposes; character count would be the wrong check.
        if self.display_name.len() > MAX_DEVICE_PROFILE_NAME_BYTES {
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
/// Daemon-to-client wire-protocol event; clients render these authoritative updates rather than predicting state.
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
        /// Carries the fingerprint for this protocol variant.
        fingerprint: String,
        /// Carries the profile for this protocol variant.
        profile: DeviceProfile,
    },
    /// Certificate identity associated with an outgoing client route.
    ClientFingerprint {
        /// Carries the handle for this protocol variant.
        handle: ClientHandle,
        /// Carries the fingerprint for this protocol variant.
        fingerprint: String,
    },
    /// new device connected
    DeviceConnected {
        /// Carries the addr for this protocol variant.
        addr: SocketAddr,
        /// Carries the fingerprint for this protocol variant.
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        /// Carries the fingerprint for this protocol variant.
        fingerprint: String,
        /// Carries the addr for this protocol variant.
        addr: SocketAddr,
        /// Carries the pos for this protocol variant.
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt {
        /// Carries the fingerprint for this protocol variant.
        fingerprint: String,
    },
    /// authoritative clipboard capability settings
    ClipboardSettings(ClipboardSettings),
    /// authoritative status for one file in a clipboard transfer
    ClipboardTransferStatus(ClipboardTransferStatus),
    /// Bounded clipboard history query result.
    /// Invalidation emitted after a visible history mutation.
    HistoryChanged,
    /// This variant reports or requests the historypage protocol state.
    HistoryPage(HistoryPage),
    /// Result of changing the local pin state.
    HistoryPinResult {
        /// Carries the event id for this protocol variant.
        event_id: HistoryEventId,
        /// Carries the pinned for this protocol variant.
        pinned: bool,
        /// Carries the updated for this protocol variant.
        updated: bool,
    },
    /// Full image payload fetched explicitly for one visible history record.
    HistoryImageResult {
        /// Carries the event id for this protocol variant.
        event_id: HistoryEventId,
        /// Carries the image for this protocol variant.
        image: Option<HistoryImage>,
        /// Carries the error for this protocol variant.
        error: Option<String>,
    },
    /// History operation failure. Global clear is reported here until peer coordination exists.
    HistoryError(String),
    /// Terminal result of an explicitly coordinated global clear.
    HistoryClearResult {
        /// Carries the operation id for this protocol variant.
        operation_id: String,
        /// Carries the affected for this protocol variant.
        affected: u64,
        /// Carries the peers acknowledged for this protocol variant.
        peers_acknowledged: u32,
        /// Carries the error for this protocol variant.
        error: Option<String>,
    },
    /// This variant reports or requests the filereceivesettingschanged protocol state.
    FileReceiveSettingsChanged(FileReceiveSettings, Option<String>),
    /// This variant reports or requests the incomingfileoffer protocol state.
    IncomingFileOffer(IncomingFileOffer),
    /// This variant reports or requests the manualtransferstatus protocol state.
    ManualTransferStatus(ManualTransferStatus),
    /// This variant reports or requests the manualtransfererror protocol state.
    ManualTransferError(String),
    /// authoritative global input sharing state
    InputSharing(bool),
    /// A replacement certificate was saved; active sessions change only on restart.
    IdentityRegenerated {
        /// Carries the fingerprint for this protocol variant.
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
/// Client-to-daemon wire-protocol request describing an operation for the authoritative daemon.
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
        /// Carries the query for this protocol variant.
        query: String,
        /// Carries the offset for this protocol variant.
        offset: u64,
        /// Carries the limit for this protocol variant.
        limit: u16,
    },
    /// Change local pin state for one stable history event.
    SetHistoryPinned {
        /// Carries the event id for this protocol variant.
        event_id: HistoryEventId,
        /// Carries the pinned for this protocol variant.
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
    /// This variant reports or requests the sendfiles protocol state.
    SendFiles {
        /// Carries the peer fingerprint for this protocol variant.
        peer_fingerprint: String,
        /// Carries the paths for this protocol variant.
        paths: Vec<PathBuf>,
    },
    /// This variant reports or requests the acceptfiletransfer protocol state.
    AcceptFileTransfer {
        /// Carries the peer fingerprint for this protocol variant.
        peer_fingerprint: String,
        /// Carries the transfer id for this protocol variant.
        transfer_id: u64,
        /// Carries the destination directory for this protocol variant.
        destination_directory: PathBuf,
    },
    /// This variant reports or requests the declinefiletransfer protocol state.
    DeclineFileTransfer {
        /// Carries the peer fingerprint for this protocol variant.
        peer_fingerprint: String,
        /// Carries the transfer id for this protocol variant.
        transfer_id: u64,
    },
    /// This variant reports or requests the cancelmanualtransfer protocol state.
    CancelManualTransfer {
        /// Carries the peer fingerprint for this protocol variant.
        peer_fingerprint: String,
        /// Carries the transfer id for this protocol variant.
        transfer_id: u64,
    },
    /// This variant reports or requests the setfilereceivesettings protocol state.
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
/// Boolean-like status used for enabled or disabled daemon capabilities.
pub enum Status {
    #[default]
    /// This variant reports or requests the disabled protocol state.
    Disabled,
    /// This variant reports or requests the enabled protocol state.
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
