use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;
pub const URI_LIST_MIME: &str = "text/uri-list";
pub const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("invalid JSON line: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON frame exceeds maximum length")]
    FrameTooLong,
    #[error("decoded range payload exceeds maximum length")]
    PayloadTooLong,
    #[error("truncated JSON frame")]
    TruncatedFrame,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Copy,
    Move,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEntry {
    pub uri: String,
    pub kind: EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyManifest {
    pub transfer_id: String,
    pub operation: Operation,
    pub entries: Vec<SourceEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub entry_id: u64,
    pub path: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteManifest {
    pub transfer_id: String,
    pub operation: Operation,
    pub entries: Vec<RemoteEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeRequest {
    pub transfer_id: String,
    pub request_id: u64,
    pub entry_id: u64,
    pub offset: u64,
    pub length: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeResponse {
    pub transfer_id: String,
    pub request_id: u64,
    pub offset: u64,
    pub data_base64: String,
    pub eof: bool,
    pub error: Option<String>,
}

impl RangeResponse {
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
pub struct MountReady {
    pub transfer_id: String,
    pub mount_uri: String,
    pub uris: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishFileClipboard {
    pub transfer_id: String,
    pub operation: Operation,
    pub uris: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Released {
    pub transfer_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unmounted {
    pub transfer_id: String,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasteDestination {
    pub transfer_id: String,
    pub destination_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub transfer_id: String,
    pub completed_entries: u64,
    pub total_entries: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    pub transfer_id: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cancelled {
    pub transfer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub clipboard_read: bool,
    pub paste: bool,
    pub cancel: bool,
    #[serde(default)]
    pub requires_live_mount: bool,
    pub mime_types: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Message {
    Hello {
        protocol_version: u32,
        adapter_id: String,
        name: String,
        capabilities: Capabilities,
    },
    CopyManifest(CopyManifest),
    PasteDestination(PasteDestination),
    Progress(Progress),
    RemoteManifest(RemoteManifest),
    RangeRequest(RangeRequest),
    RangeResponse(RangeResponse),
    MountReady(MountReady),
    PublishFileClipboard(PublishFileClipboard),
    Released(Released),
    Unmounted(Unmounted),
    Completed(Completion),
    Cancelled(Cancelled),
    Cancel {
        transfer_id: String,
    },
    Error {
        message: String,
    },
    ClipboardData {
        transfer_id: String,
        mime_type: String,
        value: String,
    },
}

impl Message {
    pub fn encode_line(&self) -> Result<String, ApiError> {
        Ok(format!("{}\n", serde_json::to_string(self)?))
    }
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

pub fn read_messages<R: BufRead>(mut reader: R) -> impl Iterator<Item = Result<Message, ApiError>> {
    std::iter::from_fn(move || {
        const MAX: usize = 2 * 1024 * 1024;
        let mut line = Vec::new();
        let mut oversized = false;
        loop {
            let buf = match reader.fill_buf() {
                Ok(buf) if buf.is_empty() => {
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

pub fn write_message<W: Write>(writer: &mut W, message: &Message) -> Result<(), ApiError> {
    writer.write_all(message.encode_line()?.as_bytes())?;
    writer.flush()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterManifest {
    pub manifest_version: u32,
    pub protocol_version: u32,
    pub id: String,
    pub name: String,
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub capabilities: Capabilities,
}

impl AdapterManifest {
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
