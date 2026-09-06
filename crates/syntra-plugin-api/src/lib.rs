//! Message vocabulary spoken by out-of-process Syntra plugins.
//!
//! Plugins communicate with the daemon through newline-delimited JSON frames;
//! this crate defines the public wire contract shared by both sides.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

/// Protocol version used by all newline-delimited JSON frames.
pub const PROTOCOL_VERSION: u32 = 1;
/// Standard URI-list MIME type used by desktop file managers.
/// The exact string enables interoperability with applications that consume RFC 2483 URI lists.
pub const URI_LIST_MIME: &str = "text/uri-list";
/// GNOME/Nautilus MIME type carrying copied-file operation metadata.
/// The exact string is required for file managers to recognise the clipboard offer.
pub const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";

/// Errors returned while encoding, decoding or validating the plugin protocol.
#[derive(Debug, Error)]
pub enum ApiError {
    /// A frame was not valid JSON, or did not match the expected shape.
    #[error("invalid JSON line: {0}")]
    Json(#[from] serde_json::Error),
    /// The peer announced a protocol version this build cannot speak.
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u32),
    /// A manifest was well-formed JSON but semantically invalid.
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    /// The underlying stream failed; the plugin is no longer reachable.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A frame exceeded the read limit and was refused.
    ///
    /// The bound exists so a malfunctioning plugin cannot exhaust memory.
    #[error("JSON frame exceeds maximum length")]
    FrameTooLong,
    /// A base64 range payload decoded to more bytes than the caller allowed.
    #[error("decoded range payload exceeds maximum length")]
    PayloadTooLong,
    /// The stream ended mid-frame, so the plugin exited unexpectedly.
    #[error("truncated JSON frame")]
    TruncatedFrame,
}

/// Filesystem operation requested for a transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// Leave the source in place.
    Copy,
    /// Remove the source once the transfer completes successfully.
    Move,
}

/// Filesystem category of a transferred entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A regular file, the only kind whose bytes are streamed.
    File,
    /// A directory, transferred as structure so its children can be placed.
    Directory,
    /// Anything else (symlink, socket, device); carried so the receiver can
    /// report it rather than silently dropping the entry.
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Plugin-to-daemon description of a locally selected entry.
pub struct SourceEntry {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub uri: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub kind: EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Protocol value used by the receiver for this transfer or declaration.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Plugin-to-daemon declaration of the entries selected for transfer.
pub struct CopyManifest {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub operation: Operation,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub entries: Vec<SourceEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Entry exposed by the daemon to a file-consuming plugin.
pub struct RemoteEntry {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub entry_id: u64,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub path: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub kind: EntryKind,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub size: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin manifest that precedes range requests for remote files.
pub struct RemoteManifest {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub operation: Operation,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub entries: Vec<RemoteEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Plugin-to-daemon request for bytes from an entry in a remote manifest.
pub struct RangeRequest {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub request_id: u64,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub entry_id: u64,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub offset: u64,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub length: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin response matched to a preceding range request.
pub struct RangeResponse {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub request_id: u64,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub offset: u64,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub data_base64: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub eof: bool,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub error: Option<String>,
}

impl RangeResponse {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub fn decode_data(&self, max_len: usize) -> Result<Vec<u8>, ApiError> {
        if self.data_base64.len() > max_len.saturating_mul(4).saturating_add(3) / 3 + 4 {
            return Err(ApiError::PayloadTooLong);
        }
        let bytes = STANDARD
            .decode(&self.data_base64)
            .map_err(|_| ApiError::InvalidManifest("invalid range payload".into()))?;
        if bytes.len() > max_len {
            return Err(ApiError::PayloadTooLong);
        }
        Ok(bytes)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin notification that a remote read-only mount is ready.
pub struct MountReady {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub mount_uri: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub uris: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Plugin-to-daemon request to publish selected files through the desktop clipboard.
pub struct PublishFileClipboard {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub operation: Operation,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub uris: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin notification that a clipboard transfer may be released.
pub struct Released {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin result of removing a temporary remote mount.
pub struct Unmounted {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub success: bool,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin destination for a paste operation.
pub struct PasteDestination {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub destination_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin progress update for an active transfer.
pub struct Progress {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub completed_entries: u64,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub total_entries: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Protocol value used by the receiver for this transfer or declaration.
    pub completed_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Protocol value used by the receiver for this transfer or declaration.
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin terminal result for an active transfer.
pub struct Completion {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Protocol value used by the receiver for this transfer or declaration.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Daemon-to-plugin cancellation notification.
pub struct Cancelled {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub transfer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Capability declaration exchanged during plugin startup.
pub struct Capabilities {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub clipboard_read: bool,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub paste: bool,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub cancel: bool,
    #[serde(default)]
    /// Protocol value used by the receiver for this transfer or declaration.
    pub requires_live_mount: bool,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub mime_types: Vec<String>,
}
/// Fingerprint of the source this component was built from.
///
/// A daemon and a plugin that disagree here are from different builds. The
/// protocol version cannot detect that, because both sides can speak the
/// same protocol while disagreeing about everything above it, which is how a
/// stale installed plugin goes unnoticed.
pub const BUILD_FINGERPRINT: &str = env!("SYNTRA_BUILD_FINGERPRINT");

/// What a plugin reports about itself during the handshake.
///
/// Compiled into the plugin, so it can never disagree with the binary the
/// way a file beside it can.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMetadata {
    /// One or two sentences describing the capability this plugin adds.
    #[serde(default)]
    pub description: String,
    /// Plugin version.
    #[serde(default)]
    pub version: String,
    /// Person or organisation responsible for it.
    #[serde(default)]
    pub author: String,
    /// Project or documentation page.
    #[serde(default)]
    pub homepage: Option<String>,
    /// Where the source can be audited.
    #[serde(default)]
    pub source: Option<String>,
    /// Where newer releases are published.
    #[serde(default)]
    pub update_url: Option<String>,
    /// SPDX licence identifier.
    #[serde(default)]
    pub license: Option<String>,
    /// Shipped with Syntra rather than installed by the user.
    #[serde(default)]
    pub bundled: bool,
    /// Started only while something needs it, then exits.
    ///
    /// Reported by the plugin because only it knows how it behaves; a stale
    /// manifest omitting this is what made a one-shot plugin read as broken.
    #[serde(default)]
    pub on_demand: bool,
    /// Fingerprint of the build this plugin came from.
    #[serde(default)]
    pub build_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
/// Wire messages exchanged between an out-of-process plugin and the daemon.
pub enum Message {
    /// Plugin startup declaration; the daemon validates it before routing messages.
    ///
    /// This is the authoritative description of a plugin. A manifest file is
    /// only how an unknown plugin is *discovered*; once the process speaks,
    /// what it says about itself wins. A file beside a binary can be stale,
    /// edited or missing, and a plugin described by a stale file is reported
    /// wrongly for as long as the file survives.
    Hello {
        /// Protocol version advertised by the plugin during startup.
        protocol_version: u32,
        /// Identifier that lets the daemon match the process to its configured adapter.
        adapter_id: String,
        /// Human-readable plugin name shown to users.
        name: String,
        /// Operations and MIME types the plugin can handle.
        capabilities: Capabilities,
        /// Everything else a user is shown about the plugin.
        ///
        /// Optional so a third-party plugin written against the earlier
        /// message shape still completes its handshake.
        #[serde(default)]
        metadata: Option<PluginMetadata>,
    },
    /// Plugin-to-daemon declaration of a local clipboard selection.
    CopyManifest(CopyManifest),
    /// Daemon-to-plugin destination selected for the transfer.
    PasteDestination(PasteDestination),
    /// Daemon-to-plugin progress update.
    Progress(Progress),
    /// Daemon-to-plugin manifest; it precedes plugin range requests.
    RemoteManifest(RemoteManifest),
    /// Plugin-to-daemon request for a remote byte range.
    RangeRequest(RangeRequest),
    /// Daemon-to-plugin response to a range request.
    RangeResponse(RangeResponse),
    /// Daemon-to-plugin notification that a mount is ready.
    MountReady(MountReady),
    /// Plugin-to-daemon request to publish file clipboard data.
    PublishFileClipboard(PublishFileClipboard),
    /// Daemon-to-plugin release notification.
    Released(Released),
    /// Daemon-to-plugin mount cleanup result.
    Unmounted(Unmounted),
    /// Daemon-to-plugin terminal success or failure result.
    Completed(Completion),
    /// Daemon-to-plugin cancellation notification.
    Cancelled(Cancelled),
    /// Daemon-to-plugin request that the plugin stop a transfer.
    Cancel {
        /// Transfer that must no longer be processed.
        transfer_id: String,
    },
    /// Plugin-to-daemon report of an uncorrelated or transfer-specific failure.
    Error {
        /// Human-readable failure reported to the peer.
        message: String,
    },
    /// Plugin-to-daemon clipboard payload for a transfer and MIME type.
    ClipboardData {
        /// Transfer whose clipboard payload is supplied.
        transfer_id: String,
        /// Exact desktop MIME identifier for `value`.
        mime_type: String,
        /// Payload associated with `mime_type`.
        value: String,
    },
}

impl Message {
    /// Encodes this message as one newline-terminated JSON frame.
    pub fn encode_line(&self) -> Result<String, ApiError> {
        Ok(format!("{}\n", serde_json::to_string(self)?))
    }
    /// Decodes one JSON frame and validates any advertised protocol version.
    pub fn decode_line(line: &str) -> Result<Self, ApiError> {
        let message: Self = serde_json::from_str(line)?;
        if let Self::Hello {
            protocol_version, ..
        } = &message
        {
            if *protocol_version != PROTOCOL_VERSION {
                return Err(ApiError::UnsupportedVersion(*protocol_version));
            }
        }
        Ok(message)
    }
}

/// Reads newline-delimited JSON messages from a buffered plugin stream.
pub fn read_messages<R: BufRead>(mut reader: R) -> impl Iterator<Item = Result<Message, ApiError>> {
    std::iter::from_fn(move || {
        const MAX: usize = 2 * 1024 * 1024;
        let mut line = Vec::new();
        let mut oversized = false;
        loop {
            let buf = match reader.fill_buf() {
                // An empty read means end of stream, which is only clean if
                // no partial frame is buffered.
                Ok([]) => {
                    if oversized {
                        return Some(Err(ApiError::FrameTooLong));
                    }
                    if line.is_empty() {
                        return None;
                    }
                    return Some(Err(ApiError::TruncatedFrame));
                }
                Ok(buf) => buf,
                Err(error) => return Some(Err(ApiError::Io(error))),
            };
            let newline = buf.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(buf.len(), |index| index + 1);
            if !oversized {
                let remaining = MAX.saturating_add(1).saturating_sub(line.len());
                let copy = take.min(remaining);
                line.extend_from_slice(&buf[..copy]);
                if line.len() > MAX {
                    oversized = true;
                }
            }
            reader.consume(take);
            if newline.is_some() {
                if oversized {
                    return Some(Err(ApiError::FrameTooLong));
                }
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Some(Message::decode_line(
                    std::str::from_utf8(&line).unwrap_or(""),
                ));
            }
        }
    })
}

/// Serialises one message as a newline-delimited JSON frame and flushes it.
pub fn write_message<W: Write>(writer: &mut W, message: &Message) -> Result<(), ApiError> {
    writer.write_all(message.encode_line()?.as_bytes())?;
    writer.flush()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Manifest describing an installable out-of-process plugin.
pub struct AdapterManifest {
    /// Protocol value used by the receiver for this transfer or declaration.
    pub manifest_version: u32,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub protocol_version: u32,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub id: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub name: String,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub executable: PathBuf,
    #[serde(default)]
    /// Protocol value used by the receiver for this transfer or declaration.
    pub args: Vec<String>,
    /// Protocol value used by the receiver for this transfer or declaration.
    pub capabilities: Capabilities,
}

impl AdapterManifest {
    /// Validates this manifest and resolves its executable relative to the manifest file.
    pub fn resolve_executable(&self, manifest_path: &Path) -> Result<PathBuf, ApiError> {
        if self.manifest_version != PROTOCOL_VERSION || self.protocol_version != PROTOCOL_VERSION {
            return Err(ApiError::InvalidManifest(
                "unsupported manifest or protocol version".into(),
            ));
        }
        if self.id.is_empty()
            || self.name.is_empty()
            || self.executable.as_os_str().is_empty()
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(ApiError::InvalidManifest(
                "id, name, and executable are required and id must be identifier-safe".into(),
            ));
        }
        if self.capabilities.mime_types.is_empty()
            || self
                .capabilities
                .mime_types
                .iter()
                .any(|m| m.trim().is_empty())
            || self
                .capabilities
                .mime_types
                .windows(2)
                .any(|w| w[0] == w[1])
            || (!self.capabilities.clipboard_read && !self.capabilities.paste)
        {
            return Err(ApiError::InvalidManifest(
                "capabilities must advertise unique MIME types and an operation".into(),
            ));
        }
        let path = if self.executable.is_absolute() {
            self.executable.clone()
        } else {
            manifest_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&self.executable)
        };
        let metadata = std::fs::metadata(&path)
            .map_err(|e| ApiError::InvalidManifest(format!("executable unavailable: {e}")))?;
        if !metadata.is_file() {
            return Err(ApiError::InvalidManifest("executable is not a file".into()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(ApiError::InvalidManifest(
                    "executable is not executable".into(),
                ));
            }
        }
        Ok(path)
    }
}
