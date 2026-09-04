use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use std::fmt::{Debug, Display, Formatter};
use thiserror::Error;

/// Largest datagram accepted by the application protocol.
pub const MAX_DATAGRAM_SIZE: usize = 16 * 1024;
/// Largest clipboard payload accepted by the protocol.
pub const MAX_CLIPBOARD_SIZE: usize = 64 * 1024 * 1024;
const CLIPBOARD_CHUNK_HEADER_SIZE: usize = 1 + 8 + 4;
pub const MAX_CLIPBOARD_CHUNK_SIZE: usize = MAX_DATAGRAM_SIZE - CLIPBOARD_CHUNK_HEADER_SIZE;
pub const MAX_CLIPBOARD_PATH_SIZE: usize = 1024;
pub const MAX_CLIPBOARD_MANIFEST_ENTRIES: usize = 256;
const CLIPBOARD_FILE_CHUNK_HEADER_SIZE: usize = 1 + 8 + 8 + 8 + 8;
/// File chunks must survive a single DTLS datagram on a normal 1500 byte
/// path MTU: larger payloads rely on IP fragmentation and are dropped,
/// which stalls a paste after the first request.
const SAFE_DATAGRAM_SIZE: usize = 1200;
pub const MAX_CLIPBOARD_FILE_CHUNK_SIZE: usize =
    SAFE_DATAGRAM_SIZE - CLIPBOARD_FILE_CHUNK_HEADER_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardManifestEntry {
    pub file_id: u64,
    pub path: String,
    pub kind: ClipboardEntryKind,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardCapabilities {
    pub text: bool,
    pub image: bool,
    pub files: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardCancelReason {
    User,
    IoError,
    Protocol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardFileChunkValidator {
    transfer_id: u64,
    file_id: u64,
    request_id: u64,
    offset: u64,
    length: u64,
    chunk_received: bool,
    chunk_end: Option<u64>,
    eof: bool,
    complete: bool,
}

impl ClipboardFileChunkValidator {
    pub fn new(
        transfer_id: u64,
        file_id: u64,
        request_id: u64,
        offset: u64,
        length: u64,
    ) -> Result<Self, ProtocolError> {
        if file_id == 0 {
            return Err(ProtocolError::InvalidFileChunk("zero file id"));
        }
        validate_request(offset, length)?;
        if request_id == 0 {
            return Err(ProtocolError::InvalidFileChunk("zero request id"));
        }
        Ok(Self {
            transfer_id,
            file_id,
            request_id,
            offset,
            length,
            chunk_received: false,
            chunk_end: None,
            eof: false,
            complete: false,
        })
    }

    pub fn validate_chunk(
        &mut self,
        transfer_id: u64,
        file_id: u64,
        request_id: u64,
        offset: u64,
        data_len: usize,
    ) -> Result<(), ProtocolError> {
        if self.complete || self.chunk_received {
            return Err(ProtocolError::InvalidFileChunk("duplicate chunk"));
        }
        if (transfer_id, file_id, request_id) != (self.transfer_id, self.file_id, self.request_id) {
            return Err(ProtocolError::InvalidFileChunk(
                "unexpected transfer identity",
            ));
        }
        if offset != self.offset {
            return Err(ProtocolError::InvalidFileChunk("unexpected offset"));
        }
        validate_file_chunk(offset, data_len)?;
        if u64::try_from(data_len).map_err(|_| ProtocolError::LengthOverflow)? > self.length {
            return Err(ProtocolError::InvalidFileChunk(
                "chunk exceeds requested length",
            ));
        }
        let data_len = u64::try_from(data_len).map_err(|_| ProtocolError::LengthOverflow)?;
        let chunk_end = offset
            .checked_add(data_len)
            .ok_or(ProtocolError::LengthOverflow)?;
        self.chunk_received = true;
        self.chunk_end = Some(chunk_end);
        self.eof = data_len < self.length;
        Ok(())
    }

    pub fn validate_complete(
        &mut self,
        transfer_id: u64,
        file_id: u64,
        request_id: u64,
        size: u64,
    ) -> Result<(), ProtocolError> {
        if self.complete {
            return Err(ProtocolError::InvalidFileChunk("duplicate completion"));
        }
        if (transfer_id, file_id, request_id) != (self.transfer_id, self.file_id, self.request_id) {
            return Err(ProtocolError::InvalidFileChunk(
                "unexpected transfer identity",
            ));
        }
        if self.chunk_received {
            if !self.eof || self.chunk_end != Some(size) {
                return Err(ProtocolError::InvalidFileChunk(
                    "inconsistent completion size",
                ));
            }
        } else if size != self.offset {
            return Err(ProtocolError::InvalidFileChunk(
                "completion before end of file",
            ));
        }
        validate_file_position(size)?;
        self.complete = true;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    #[error("invalid position: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    #[error("truncated event")]
    Truncated,
    #[error("event exceeds protocol limit: {actual} > {limit}")]
    TooLarge { actual: usize, limit: usize },
    #[error("invalid clipboard chunk count: expected {expected}, got {actual}")]
    InvalidChunkCount { expected: u32, actual: u32 },
    #[error("invalid clipboard path: {0}")]
    InvalidPath(String),
    #[error("invalid clipboard manifest: {0}")]
    InvalidManifest(String),
    #[error("integer overflow")]
    LengthOverflow,
    #[error("invalid progress: {completed} > {total}")]
    InvalidProgress { completed: u64, total: u64 },
    #[error("trailing bytes: {0}")]
    TrailingBytes(usize),
    #[error("invalid boolean value: {0}")]
    InvalidBoolean(u8),
    #[error("invalid clipboard cancel reason: {0}")]
    InvalidCancelReason(u8),
    #[error("invalid clipboard file chunk: {0}")]
    InvalidFileChunk(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
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
#[derive(Clone, Debug)]
pub enum ProtoEvent {
    Enter(Position),
    Leave(u32),
    Ack(u32),
    Input(InputEvent),
    Ping,
    Pong(bool),
    Hello {
        commit: [u8; 8],
    },
    ClipboardStart {
        transfer_id: u64,
        total_len: u32,
        chunks: u32,
    },
    ClipboardImageStart {
        transfer_id: u64,
        width: u32,
        height: u32,
        total_len: u32,
        chunks: u32,
    },
    ClipboardChunk {
        transfer_id: u64,
        index: u32,
        data: Vec<u8>,
    },
    ClipboardCapabilities(ClipboardCapabilities),
    ClipboardManifest {
        transfer_id: u64,
        entries: Vec<ClipboardManifestEntry>,
    },
    ClipboardFileRequest {
        transfer_id: u64,
        file_id: u64,
        request_id: u64,
        offset: u64,
        length: u64,
    },
    ClipboardFileChunk {
        transfer_id: u64,
        file_id: u64,
        request_id: u64,
        offset: u64,
        data: Vec<u8>,
    },
    ClipboardFileComplete {
        transfer_id: u64,
        file_id: u64,
        request_id: u64,
        size: u64,
        digest: [u8; 32],
    },
    ClipboardTransferCancel {
        transfer_id: u64,
        file_id: Option<u64>,
        reason: ClipboardCancelReason,
    },
    ClipboardTransferProgress {
        transfer_id: u64,
        file_id: u64,
        completed: u64,
        total: u64,
    },
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Enter(position) => write!(f, "Enter({position})"),
            Self::Leave(serial) => write!(f, "Leave({serial})"),
            Self::Ack(serial) => write!(f, "Ack({serial})"),
            Self::Input(event) => write!(f, "{event}"),
            Self::Ping => write!(f, "ping"),
            Self::Pong(alive) => write!(
                f,
                "pong: {}",
                if *alive { "alive" } else { "not available" }
            ),
            Self::Hello { commit } => write!(
                f,
                "Hello({})",
                std::str::from_utf8(commit).unwrap_or("????????")
            ),
            Self::ClipboardStart {
                transfer_id,
                total_len,
                chunks,
            } => {
                write!(
                    f,
                    "ClipboardStart({transfer_id}, {total_len} bytes, {chunks} chunks)"
                )
            }
            Self::ClipboardImageStart {
                transfer_id,
                width,
                height,
                total_len,
                chunks,
            } => write!(
                f,
                "ClipboardImageStart({transfer_id}, {width}x{height}, {total_len} bytes, {chunks} chunks)"
            ),
            Self::ClipboardChunk {
                transfer_id,
                index,
                data,
            } => {
                write!(
                    f,
                    "ClipboardChunk({transfer_id}, {index}, {} bytes)",
                    data.len()
                )
            }
            Self::ClipboardFileRequest {
                transfer_id,
                file_id,
                request_id,
                offset,
                length,
            } => write!(
                f,
                "ClipboardFileRequest({transfer_id}, {file_id}, {request_id}, {offset}, {length} bytes)"
            ),
            Self::ClipboardFileChunk {
                transfer_id,
                file_id,
                request_id,
                offset,
                data,
            } => write!(
                f,
                "ClipboardFileChunk({transfer_id}, {file_id}, {request_id}, {offset}, {} bytes)",
                data.len()
            ),
            Self::ClipboardFileComplete {
                transfer_id,
                file_id,
                request_id,
                size,
                ..
            } => write!(
                f,
                "ClipboardFileComplete({transfer_id}, {file_id}, {request_id}, {size} bytes)"
            ),
            Self::ClipboardCapabilities(_)
            | Self::ClipboardManifest { .. }
            | Self::ClipboardTransferCancel { .. }
            | Self::ClipboardTransferProgress { .. } => write!(f, "clipboard file transfer"),
        }
    }
}

#[derive(Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
    ClipboardStart,
    ClipboardChunk,
    ClipboardImageStart,
    ClipboardCapabilities,
    ClipboardManifest,
    ClipboardFileRequest,
    ClipboardFileChunk,
    ClipboardFileComplete,
    ClipboardTransferCancel,
    ClipboardTransferProgress,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            Self::Input(InputEvent::Pointer(PointerEvent::Motion { .. })) => {
                EventType::PointerMotion
            }
            Self::Input(InputEvent::Pointer(PointerEvent::Button { .. })) => {
                EventType::PointerButton
            }
            Self::Input(InputEvent::Pointer(PointerEvent::Axis { .. })) => EventType::PointerAxis,
            Self::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 { .. })) => {
                EventType::PointerAxisValue120
            }
            Self::Input(InputEvent::Keyboard(KeyboardEvent::Key { .. })) => EventType::KeyboardKey,
            Self::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers { .. })) => {
                EventType::KeyboardModifiers
            }
            Self::Ping => EventType::Ping,
            Self::Pong(_) => EventType::Pong,
            Self::ClipboardCapabilities(_) => EventType::ClipboardCapabilities,
            Self::ClipboardManifest { .. } => EventType::ClipboardManifest,
            Self::ClipboardFileRequest { .. } => EventType::ClipboardFileRequest,
            Self::ClipboardFileChunk { .. } => EventType::ClipboardFileChunk,
            Self::ClipboardFileComplete { .. } => EventType::ClipboardFileComplete,
            Self::ClipboardTransferCancel { .. } => EventType::ClipboardTransferCancel,
            Self::ClipboardTransferProgress { .. } => EventType::ClipboardTransferProgress,
            Self::Enter(_) => EventType::Enter,
            Self::Leave(_) => EventType::Leave,
            Self::Ack(_) => EventType::Ack,
            Self::Hello { .. } => EventType::Hello,
            Self::ClipboardStart { .. } => EventType::ClipboardStart,
            Self::ClipboardImageStart { .. } => EventType::ClipboardImageStart,
            Self::ClipboardChunk { .. } => EventType::ClipboardChunk,
        }
    }

    pub fn encode(self) -> Result<Vec<u8>, ProtocolError> {
        let mut buf = Vec::with_capacity(32);
        buf.push(self.event_type() as u8);
        match self {
            Self::Input(InputEvent::Pointer(PointerEvent::Motion { time, dx, dy })) => {
                put_u32(&mut buf, time);
                put_f64(&mut buf, dx);
                put_f64(&mut buf, dy);
            }
            Self::Input(InputEvent::Pointer(PointerEvent::Button {
                time,
                button,
                state,
            })) => {
                put_u32(&mut buf, time);
                put_u32(&mut buf, button);
                put_u32(&mut buf, state);
            }
            Self::Input(InputEvent::Pointer(PointerEvent::Axis { time, axis, value })) => {
                put_u32(&mut buf, time);
                buf.push(axis);
                put_f64(&mut buf, value);
            }
            Self::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 { axis, value })) => {
                buf.push(axis);
                put_i32(&mut buf, value);
            }
            Self::Input(InputEvent::Keyboard(KeyboardEvent::Key { time, key, state })) => {
                put_u32(&mut buf, time);
                put_u32(&mut buf, key);
                buf.push(state);
            }
            Self::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            })) => {
                put_u32(&mut buf, depressed);
                put_u32(&mut buf, latched);
                put_u32(&mut buf, locked);
                put_u32(&mut buf, group);
            }
            Self::Ping => {}
            Self::Pong(alive) => buf.push(u8::from(alive)),
            Self::Enter(position) => buf.push(position as u8),
            Self::Leave(serial) | Self::Ack(serial) => put_u32(&mut buf, serial),
            Self::Hello { commit } => buf.extend_from_slice(&commit),
            Self::ClipboardStart {
                transfer_id,
                total_len,
                chunks,
            } => {
                validate_clipboard_start(total_len, chunks)?;
                put_u64(&mut buf, transfer_id);
                put_u32(&mut buf, total_len);
                put_u32(&mut buf, chunks);
            }
            Self::ClipboardImageStart {
                transfer_id,
                width,
                height,
                total_len,
                chunks,
            } => {
                validate_clipboard_image(width, height, total_len, chunks)?;
                put_u64(&mut buf, transfer_id);
                put_u32(&mut buf, width);
                put_u32(&mut buf, height);
                put_u32(&mut buf, total_len);
                put_u32(&mut buf, chunks);
            }
            Self::ClipboardChunk {
                transfer_id,
                index,
                data,
            } => {
                let actual = CLIPBOARD_CHUNK_HEADER_SIZE + data.len();
                if actual > MAX_DATAGRAM_SIZE {
                    return Err(ProtocolError::TooLarge {
                        actual,
                        limit: MAX_DATAGRAM_SIZE,
                    });
                }
                put_u64(&mut buf, transfer_id);
                put_u32(&mut buf, index);
                buf.extend_from_slice(&data);
            }
            Self::ClipboardCapabilities(c) => {
                buf.push(c.text as u8);
                buf.push(c.image as u8);
                buf.push(c.files as u8);
            }
            Self::ClipboardManifest {
                transfer_id,
                entries,
            } => encode_manifest(&mut buf, transfer_id, &entries)?,
            Self::ClipboardFileRequest {
                transfer_id,
                file_id,
                request_id,
                offset,
                length,
            } => {
                validate_ids(file_id, request_id)?;
                validate_request(offset, length)?;
                put_u64(&mut buf, transfer_id);
                put_u64(&mut buf, file_id);
                put_u64(&mut buf, request_id);
                put_u64(&mut buf, offset);
                put_u64(&mut buf, length);
            }
            Self::ClipboardFileChunk {
                transfer_id,
                file_id,
                request_id,
                offset,
                data,
            } => {
                validate_ids(file_id, request_id)?;
                validate_file_chunk(offset, data.len())?;
                put_u64(&mut buf, transfer_id);
                put_u64(&mut buf, file_id);
                put_u64(&mut buf, request_id);
                put_u64(&mut buf, offset);
                buf.extend_from_slice(&data);
            }
            Self::ClipboardFileComplete {
                transfer_id,
                file_id,
                request_id,
                size,
                digest,
            } => {
                validate_ids(file_id, request_id)?;
                validate_file_position(size)?;
                put_u64(&mut buf, transfer_id);
                put_u64(&mut buf, file_id);
                put_u64(&mut buf, request_id);
                put_u64(&mut buf, size);
                buf.extend_from_slice(&digest);
            }
            Self::ClipboardTransferCancel {
                transfer_id,
                file_id,
                reason,
            } => {
                put_u64(&mut buf, transfer_id);
                put_u64(&mut buf, file_id.unwrap_or(0));
                buf.push(match reason {
                    ClipboardCancelReason::User => 0,
                    ClipboardCancelReason::IoError => 1,
                    ClipboardCancelReason::Protocol => 2,
                });
            }
            Self::ClipboardTransferProgress {
                transfer_id,
                file_id,
                completed,
                total,
            } => {
                if completed > total {
                    return Err(ProtocolError::InvalidProgress { completed, total });
                }
                put_u64(&mut buf, transfer_id);
                put_u64(&mut buf, file_id);
                put_u64(&mut buf, completed);
                put_u64(&mut buf, total);
            }
        }
        Ok(buf)
    }

    pub fn decode(mut buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() > MAX_DATAGRAM_SIZE {
            return Err(ProtocolError::TooLarge {
                actual: buf.len(),
                limit: MAX_DATAGRAM_SIZE,
            });
        }
        let event = match EventType::try_from(get_u8(&mut buf)?)? {
            EventType::PointerMotion => Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                time: get_u32(&mut buf)?,
                dx: get_f64(&mut buf)?,
                dy: get_f64(&mut buf)?,
            })),
            EventType::PointerButton => Self::Input(InputEvent::Pointer(PointerEvent::Button {
                time: get_u32(&mut buf)?,
                button: get_u32(&mut buf)?,
                state: get_u32(&mut buf)?,
            })),
            EventType::PointerAxis => Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: get_u32(&mut buf)?,
                axis: get_u8(&mut buf)?,
                value: get_f64(&mut buf)?,
            })),
            EventType::PointerAxisValue120 => {
                Self::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
                    axis: get_u8(&mut buf)?,
                    value: get_i32(&mut buf)?,
                }))
            }
            EventType::KeyboardKey => Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: get_u32(&mut buf)?,
                key: get_u32(&mut buf)?,
                state: get_u8(&mut buf)?,
            })),
            EventType::KeyboardModifiers => {
                Self::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                    depressed: get_u32(&mut buf)?,
                    latched: get_u32(&mut buf)?,
                    locked: get_u32(&mut buf)?,
                    group: get_u32(&mut buf)?,
                }))
            }
            EventType::Ping => Self::Ping,
            EventType::Pong => Self::Pong(get_u8(&mut buf)? != 0),
            EventType::Enter => Self::Enter(get_u8(&mut buf)?.try_into()?),
            EventType::Leave => Self::Leave(get_u32(&mut buf)?),
            EventType::Ack => Self::Ack(get_u32(&mut buf)?),
            EventType::Hello => {
                let mut commit = [0; 8];
                commit.copy_from_slice(take(&mut buf, 8)?);
                Self::Hello { commit }
            }
            EventType::ClipboardStart => {
                let transfer_id = get_u64(&mut buf)?;
                let total_len = get_u32(&mut buf)?;
                let chunks = get_u32(&mut buf)?;
                validate_clipboard_start(total_len, chunks)?;
                Self::ClipboardStart {
                    transfer_id,
                    total_len,
                    chunks,
                }
            }
            EventType::ClipboardImageStart => {
                let transfer_id = get_u64(&mut buf)?;
                let width = get_u32(&mut buf)?;
                let height = get_u32(&mut buf)?;
                let total_len = get_u32(&mut buf)?;
                let chunks = get_u32(&mut buf)?;
                validate_clipboard_image(width, height, total_len, chunks)?;
                Self::ClipboardImageStart {
                    transfer_id,
                    width,
                    height,
                    total_len,
                    chunks,
                }
            }
            EventType::ClipboardChunk => {
                return Ok(Self::ClipboardChunk {
                    transfer_id: get_u64(&mut buf)?,
                    index: get_u32(&mut buf)?,
                    data: buf.to_vec(),
                });
            }
            EventType::ClipboardCapabilities => {
                Self::ClipboardCapabilities(ClipboardCapabilities {
                    text: get_bool(&mut buf)?,
                    image: get_bool(&mut buf)?,
                    files: get_bool(&mut buf)?,
                })
            }
            EventType::ClipboardManifest => decode_manifest(&mut buf)?,
            EventType::ClipboardFileRequest => {
                let transfer_id = get_u64(&mut buf)?;
                let file_id = get_nonzero_file_id(&mut buf)?;
                let request_id = get_nonzero_request_id(&mut buf)?;
                let offset = get_u64(&mut buf)?;
                let length = get_u64(&mut buf)?;
                validate_request(offset, length)?;
                Self::ClipboardFileRequest {
                    transfer_id,
                    file_id,
                    request_id,
                    offset,
                    length,
                }
            }
            EventType::ClipboardFileChunk => {
                let transfer_id = get_u64(&mut buf)?;
                let file_id = get_nonzero_file_id(&mut buf)?;
                let request_id = get_nonzero_request_id(&mut buf)?;
                let offset = get_u64(&mut buf)?;
                validate_file_chunk(offset, buf.len())?;
                Self::ClipboardFileChunk {
                    transfer_id,
                    file_id,
                    request_id,
                    offset,
                    data: std::mem::take(&mut buf).to_vec(),
                }
            }
            EventType::ClipboardFileComplete => {
                let transfer_id = get_u64(&mut buf)?;
                let file_id = get_nonzero_file_id(&mut buf)?;
                let request_id = get_nonzero_request_id(&mut buf)?;
                let size = get_u64(&mut buf)?;
                validate_file_position(size)?;
                let digest = take(&mut buf, 32)?.try_into().expect("length checked");
                Self::ClipboardFileComplete {
                    transfer_id,
                    file_id,
                    request_id,
                    size,
                    digest,
                }
            }
            EventType::ClipboardTransferCancel => {
                let transfer_id = get_u64(&mut buf)?;
                let file_id = match get_u64(&mut buf)? {
                    0 => None,
                    file_id => Some(file_id),
                };
                let reason = match get_u8(&mut buf)? {
                    0 => ClipboardCancelReason::User,
                    1 => ClipboardCancelReason::IoError,
                    2 => ClipboardCancelReason::Protocol,
                    value => return Err(ProtocolError::InvalidCancelReason(value)),
                };
                Self::ClipboardTransferCancel {
                    transfer_id,
                    file_id,
                    reason,
                }
            }
            EventType::ClipboardTransferProgress => {
                let transfer_id = get_u64(&mut buf)?;
                let file_id = get_nonzero_file_id(&mut buf)?;
                let completed = get_u64(&mut buf)?;
                let total = get_u64(&mut buf)?;
                validate_progress(completed, total)?;
                Self::ClipboardTransferProgress {
                    transfer_id,
                    file_id,
                    completed,
                    total,
                }
            }
        };
        if !buf.is_empty() {
            return Err(ProtocolError::TrailingBytes(buf.len()));
        }
        Ok(event)
    }
}

fn validate_clipboard_start(total_len: u32, chunks: u32) -> Result<(), ProtocolError> {
    if total_len as usize > MAX_CLIPBOARD_SIZE {
        return Err(ProtocolError::TooLarge {
            actual: total_len as usize,
            limit: MAX_CLIPBOARD_SIZE,
        });
    }
    let expected = if total_len == 0 {
        0
    } else {
        total_len.div_ceil(MAX_CLIPBOARD_CHUNK_SIZE as u32)
    };
    if chunks != expected {
        return Err(ProtocolError::InvalidChunkCount {
            expected,
            actual: chunks,
        });
    }
    Ok(())
}

fn encode_manifest(
    buf: &mut Vec<u8>,
    transfer_id: u64,
    entries: &[ClipboardManifestEntry],
) -> Result<(), ProtocolError> {
    if entries.is_empty() || entries.len() > MAX_CLIPBOARD_MANIFEST_ENTRIES {
        return Err(ProtocolError::InvalidManifest("entry count".into()));
    }
    put_u64(buf, transfer_id);
    put_u32(buf, entries.len() as u32);
    let mut previous = 0;
    let mut total = 0u64;
    for entry in entries {
        validate_path(&entry.path)?;
        if entry.file_id == 0 || entry.file_id <= previous {
            return Err(ProtocolError::InvalidManifest(
                "file ids must increase".into(),
            ));
        }
        previous = entry.file_id;
        total = total
            .checked_add(entry.size)
            .ok_or(ProtocolError::LengthOverflow)?;
        if total > MAX_CLIPBOARD_SIZE as u64 {
            return Err(ProtocolError::TooLarge {
                actual: usize::MAX,
                limit: MAX_CLIPBOARD_SIZE,
            });
        }
        if matches!(entry.kind, ClipboardEntryKind::Directory) && entry.size != 0 {
            return Err(ProtocolError::InvalidManifest("directory size".into()));
        }
        put_u64(buf, entry.file_id);
        buf.push(matches!(entry.kind, ClipboardEntryKind::File) as u8);
        put_u16(buf, entry.path.len() as u16);
        buf.extend_from_slice(entry.path.as_bytes());
        put_u64(buf, entry.size);
    }
    if buf.len() > MAX_DATAGRAM_SIZE {
        return Err(ProtocolError::TooLarge {
            actual: buf.len(),
            limit: MAX_DATAGRAM_SIZE,
        });
    }
    Ok(())
}

fn decode_manifest(buf: &mut &[u8]) -> Result<ProtoEvent, ProtocolError> {
    let transfer_id = get_u64(buf)?;
    let count = get_u32(buf)? as usize;
    if count == 0 || count > MAX_CLIPBOARD_MANIFEST_ENTRIES {
        return Err(ProtocolError::InvalidManifest("entry count".into()));
    }
    let mut entries = Vec::with_capacity(count);
    let mut previous = 0;
    let mut total = 0u64;
    for _ in 0..count {
        let file_id = get_u64(buf)?;
        if file_id == 0 || file_id <= previous {
            return Err(ProtocolError::InvalidManifest(
                "file ids must increase".into(),
            ));
        }
        previous = file_id;
        let kind = match get_u8(buf)? {
            0 => ClipboardEntryKind::Directory,
            1 => ClipboardEntryKind::File,
            value => return Err(ProtocolError::InvalidBoolean(value)),
        };
        let path_len = get_u16(buf)? as usize;
        if path_len > MAX_CLIPBOARD_PATH_SIZE {
            return Err(ProtocolError::TooLarge {
                actual: path_len,
                limit: MAX_CLIPBOARD_PATH_SIZE,
            });
        }
        let path_bytes = take(buf, path_len)?;
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| ProtocolError::InvalidPath("non-UTF-8 path".into()))?
            .to_owned();
        validate_path(&path)?;
        let size = get_u64(buf)?;
        if matches!(kind, ClipboardEntryKind::Directory) && size != 0 {
            return Err(ProtocolError::InvalidManifest("directory size".into()));
        }
        total = total
            .checked_add(size)
            .ok_or(ProtocolError::LengthOverflow)?;
        validate_file_position(total)?;
        entries.push(ClipboardManifestEntry {
            file_id,
            path,
            kind,
            size,
        });
    }
    Ok(ProtoEvent::ClipboardManifest {
        transfer_id,
        entries,
    })
}

fn validate_path(path: &str) -> Result<(), ProtocolError> {
    if path.len() > MAX_CLIPBOARD_PATH_SIZE {
        return Err(ProtocolError::TooLarge {
            actual: path.len(),
            limit: MAX_CLIPBOARD_PATH_SIZE,
        });
    }
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .bytes()
            .any(|byte| byte == b'\\' || byte == b':' || byte == 0)
    {
        return Err(ProtocolError::InvalidPath(path.into()));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ProtocolError::InvalidPath(path.into()));
    }
    Ok(())
}

fn validate_file_position(value: u64) -> Result<(), ProtocolError> {
    if value > MAX_CLIPBOARD_SIZE as u64 {
        return Err(ProtocolError::TooLarge {
            actual: usize::try_from(value).unwrap_or(usize::MAX),
            limit: MAX_CLIPBOARD_SIZE,
        });
    }
    Ok(())
}

fn validate_file_chunk(offset: u64, data_len: usize) -> Result<(), ProtocolError> {
    if data_len == 0 {
        return Err(ProtocolError::InvalidFileChunk("empty chunk"));
    }
    if data_len > MAX_CLIPBOARD_FILE_CHUNK_SIZE {
        return Err(ProtocolError::TooLarge {
            actual: data_len,
            limit: MAX_CLIPBOARD_FILE_CHUNK_SIZE,
        });
    }
    validate_file_position(offset)?;
    let len = u64::try_from(data_len).map_err(|_| ProtocolError::LengthOverflow)?;
    let end = offset
        .checked_add(len)
        .ok_or(ProtocolError::LengthOverflow)?;
    validate_file_position(end)
}

fn validate_progress(completed: u64, total: u64) -> Result<(), ProtocolError> {
    validate_file_position(total)?;
    if completed > total {
        return Err(ProtocolError::InvalidProgress { completed, total });
    }
    Ok(())
}

fn get_nonzero_file_id(buf: &mut &[u8]) -> Result<u64, ProtocolError> {
    match get_u64(buf)? {
        0 => Err(ProtocolError::InvalidFileChunk("zero file id")),
        file_id => Ok(file_id),
    }
}

fn get_bool(buf: &mut &[u8]) -> Result<bool, ProtocolError> {
    match get_u8(buf)? {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(ProtocolError::InvalidBoolean(value)),
    }
}

fn put_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_be_bytes());
}

fn validate_clipboard_image(
    width: u32,
    height: u32,
    total_len: u32,
    chunks: u32,
) -> Result<(), ProtocolError> {
    let expected_len = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ProtocolError::TooLarge {
            actual: usize::MAX,
            limit: MAX_CLIPBOARD_SIZE,
        })?;
    if expected_len != total_len as usize {
        return Err(ProtocolError::TooLarge {
            actual: total_len as usize,
            limit: expected_len,
        });
    }
    validate_clipboard_start(total_len, chunks)
}

fn get_nonzero_request_id(buf: &mut &[u8]) -> Result<u64, ProtocolError> {
    match get_u64(buf)? {
        0 => Err(ProtocolError::InvalidFileChunk("zero request id")),
        request_id => Ok(request_id),
    }
}

fn validate_ids(file_id: u64, request_id: u64) -> Result<(), ProtocolError> {
    if file_id == 0 {
        return Err(ProtocolError::InvalidFileChunk("zero file id"));
    }
    if request_id == 0 {
        return Err(ProtocolError::InvalidFileChunk("zero request id"));
    }
    Ok(())
}

fn validate_request(offset: u64, length: u64) -> Result<(), ProtocolError> {
    validate_file_position(offset)?;
    if length == 0 || length > MAX_CLIPBOARD_FILE_CHUNK_SIZE as u64 {
        return Err(ProtocolError::InvalidFileChunk("invalid request length"));
    }
    let end = offset
        .checked_add(length)
        .ok_or(ProtocolError::LengthOverflow)?;
    validate_file_position(end)
}

fn take<'a>(buf: &mut &'a [u8], len: usize) -> Result<&'a [u8], ProtocolError> {
    if buf.len() < len {
        return Err(ProtocolError::Truncated);
    }
    let (value, rest) = buf.split_at(len);
    *buf = rest;
    Ok(value)
}

fn get_u8(buf: &mut &[u8]) -> Result<u8, ProtocolError> {
    Ok(take(buf, 1)?[0])
}
fn get_u16(buf: &mut &[u8]) -> Result<u16, ProtocolError> {
    Ok(u16::from_be_bytes(
        take(buf, 2)?.try_into().expect("length checked"),
    ))
}
fn get_u32(buf: &mut &[u8]) -> Result<u32, ProtocolError> {
    Ok(u32::from_be_bytes(
        take(buf, 4)?.try_into().expect("length checked"),
    ))
}
fn get_u64(buf: &mut &[u8]) -> Result<u64, ProtocolError> {
    Ok(u64::from_be_bytes(
        take(buf, 8)?.try_into().expect("length checked"),
    ))
}
fn get_i32(buf: &mut &[u8]) -> Result<i32, ProtocolError> {
    Ok(i32::from_be_bytes(
        take(buf, 4)?.try_into().expect("length checked"),
    ))
}
fn get_f64(buf: &mut &[u8]) -> Result<f64, ProtocolError> {
    Ok(f64::from_be_bytes(
        take(buf, 8)?.try_into().expect("length checked"),
    ))
}
fn put_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_be_bytes());
}
fn put_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_be_bytes());
}
fn put_i32(buf: &mut Vec<u8>, value: i32) {
    buf.extend_from_slice(&value.to_be_bytes());
}
fn put_f64(buf: &mut Vec<u8>, value: f64) {
    buf.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_round_trip(event: ProtoEvent) {
        let encoded = event.encode().unwrap();
        let decoded = ProtoEvent::decode(&encoded).unwrap();
        assert_eq!(decoded.encode().unwrap(), encoded);
    }

    #[test]
    fn fixed_events_round_trip() {
        assert_round_trip(ProtoEvent::Ping);
        assert_round_trip(ProtoEvent::Pong(true));
        assert_round_trip(ProtoEvent::Enter(Position::Right));
        assert_round_trip(ProtoEvent::Leave(u32::MAX));
        assert_round_trip(ProtoEvent::Ack(17));
        assert_round_trip(ProtoEvent::Hello {
            commit: *b"deadbeef",
        });
        assert_round_trip(ProtoEvent::Input(InputEvent::Pointer(
            PointerEvent::Motion {
                time: 9,
                dx: -1.5,
                dy: 2.25,
            },
        )));
    }

    #[test]
    fn clipboard_events_round_trip() {
        assert_round_trip(ProtoEvent::ClipboardStart {
            transfer_id: 42,
            total_len: 5,
            chunks: 1,
        });
        assert_round_trip(ProtoEvent::ClipboardImageStart {
            transfer_id: 43,
            width: 2,
            height: 1,
            total_len: 8,
            chunks: 1,
        });
        assert_round_trip(ProtoEvent::ClipboardChunk {
            transfer_id: 42,
            index: 0,
            data: b"hello".to_vec(),
        });
    }

    #[test]
    fn truncated_events_are_rejected() {
        assert!(matches!(
            ProtoEvent::decode(&[EventType::Ack as u8]),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            ProtoEvent::decode(&[EventType::ClipboardStart as u8]),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            ProtoEvent::decode(&[EventType::ClipboardChunk as u8]),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn clipboard_limits_are_enforced() {
        let chunks = (MAX_CLIPBOARD_SIZE as u32).div_ceil(MAX_CLIPBOARD_CHUNK_SIZE as u32);
        assert_round_trip(ProtoEvent::ClipboardStart {
            transfer_id: 1,
            total_len: MAX_CLIPBOARD_SIZE as u32,
            chunks,
        });
        assert!(matches!(
            ProtoEvent::ClipboardStart {
                transfer_id: 1,
                total_len: MAX_CLIPBOARD_SIZE as u32 + 1,
                chunks,
            }
            .encode(),
            Err(ProtocolError::TooLarge { .. })
        ));
        assert_round_trip(ProtoEvent::ClipboardChunk {
            transfer_id: 1,
            index: 0,
            data: vec![0; MAX_CLIPBOARD_CHUNK_SIZE],
        });
        assert!(matches!(
            ProtoEvent::ClipboardChunk {
                transfer_id: 1,
                index: 0,
                data: vec![0; MAX_CLIPBOARD_CHUNK_SIZE + 1],
            }
            .encode(),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn unknown_events_remain_forward_compatible_errors() {
        assert!(matches!(
            ProtoEvent::decode(&[u8::MAX]),
            Err(ProtocolError::InvalidEventId(_))
        ));
    }

    fn sample_manifest() -> ProtoEvent {
        ProtoEvent::ClipboardManifest {
            transfer_id: 77,
            entries: vec![
                ClipboardManifestEntry {
                    file_id: 1,
                    path: "photos".into(),
                    kind: ClipboardEntryKind::Directory,
                    size: 0,
                },
                ClipboardManifestEntry {
                    file_id: 2,
                    path: "photos/cat.png".into(),
                    kind: ClipboardEntryKind::File,
                    size: 123_456,
                },
                ClipboardManifestEntry {
                    file_id: 3,
                    path: "notes.txt".into(),
                    kind: ClipboardEntryKind::File,
                    size: 7,
                },
            ],
        }
    }

    #[test]
    fn file_transfer_events_round_trip() {
        assert_round_trip(ProtoEvent::ClipboardCapabilities(ClipboardCapabilities {
            text: true,
            image: false,
            files: true,
        }));
        assert_round_trip(sample_manifest());
        assert_round_trip(ProtoEvent::ClipboardFileRequest {
            transfer_id: 77,
            file_id: 2,
            request_id: 9,
            offset: 4096,
            length: 1024,
        });
        assert_round_trip(ProtoEvent::ClipboardFileChunk {
            transfer_id: 77,
            file_id: 2,
            request_id: 9,
            offset: 4096,
            data: b"streamed bytes".to_vec(),
        });
        assert_round_trip(ProtoEvent::ClipboardFileComplete {
            transfer_id: 77,
            file_id: 2,
            request_id: 9,
            digest: [7; 32],
            size: 123_456,
        });
        assert_round_trip(ProtoEvent::ClipboardTransferCancel {
            transfer_id: 77,
            file_id: Some(2),
            reason: ClipboardCancelReason::IoError,
        });
        assert_round_trip(ProtoEvent::ClipboardTransferProgress {
            transfer_id: 77,
            file_id: 2,
            completed: 4096,
            total: 123_456,
        });
    }

    #[test]
    fn manifest_rejects_unsafe_paths_and_invalid_ordering() {
        for path in [
            "",
            ".",
            "..",
            "../secret",
            "safe/../secret",
            "/absolute",
            "double//slash",
            "windows\\path",
            "C:/drive",
            "trailing/",
        ] {
            let event = ProtoEvent::ClipboardManifest {
                transfer_id: 1,
                entries: vec![ClipboardManifestEntry {
                    file_id: 1,
                    path: path.into(),
                    kind: ClipboardEntryKind::File,
                    size: 1,
                }],
            };
            assert!(
                matches!(event.encode(), Err(ProtocolError::InvalidPath(_))),
                "{path:?}"
            );
        }

        for ids in [[2, 1], [1, 1], [0, 1]] {
            let event = ProtoEvent::ClipboardManifest {
                transfer_id: 1,
                entries: ids
                    .into_iter()
                    .map(|file_id| ClipboardManifestEntry {
                        file_id,
                        path: format!("{file_id}.txt"),
                        kind: ClipboardEntryKind::File,
                        size: 1,
                    })
                    .collect(),
            };
            assert!(matches!(
                event.encode(),
                Err(ProtocolError::InvalidManifest(_))
            ));
        }
    }

    #[test]
    fn file_metadata_limits_and_overflow_are_rejected() {
        let oversized_path = "a".repeat(MAX_CLIPBOARD_PATH_SIZE + 1);
        assert!(matches!(
            ProtoEvent::ClipboardManifest {
                transfer_id: 1,
                entries: vec![ClipboardManifestEntry {
                    file_id: 1,
                    path: oversized_path,
                    kind: ClipboardEntryKind::File,
                    size: 1,
                }],
            }
            .encode(),
            Err(ProtocolError::TooLarge { .. })
        ));

        assert!(matches!(
            ProtoEvent::ClipboardManifest {
                transfer_id: 1,
                entries: (1..=MAX_CLIPBOARD_MANIFEST_ENTRIES as u64 + 1)
                    .map(|file_id| ClipboardManifestEntry {
                        file_id,
                        path: format!("{file_id}"),
                        kind: ClipboardEntryKind::File,
                        size: 1,
                    })
                    .collect(),
            }
            .encode(),
            Err(ProtocolError::InvalidManifest(_))
        ));

        assert!(matches!(
            ProtoEvent::ClipboardFileChunk {
                transfer_id: 1,
                file_id: 1,
                request_id: 1,
                offset: u64::MAX,
                data: vec![1],
            }
            .encode(),
            Err(ProtocolError::TooLarge { .. })
        ));
        assert!(matches!(
            ProtoEvent::ClipboardTransferProgress {
                transfer_id: 1,
                file_id: 1,
                completed: 2,
                total: 1,
            }
            .encode(),
            Err(ProtocolError::InvalidProgress { .. })
        ));
    }

    #[test]
    fn malformed_file_events_and_trailing_bytes_are_rejected() {
        let mut encoded = sample_manifest().encode().unwrap();
        encoded.push(0);
        assert!(matches!(
            ProtoEvent::decode(&encoded),
            Err(ProtocolError::TrailingBytes(1))
        ));

        let malformed = [
            EventType::ClipboardManifest as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            1, // transfer id
            0,
            0,
            0,
            1, // one entry, but entry absent
        ];
        assert!(matches!(
            ProtoEvent::decode(&malformed),
            Err(ProtocolError::Truncated)
        ));

        let over_datagram = vec![0; MAX_DATAGRAM_SIZE + 1];
        assert!(matches!(
            ProtoEvent::decode(&over_datagram),
            Err(ProtocolError::TooLarge { .. })
        ));
    }
    #[test]
    fn strict_file_decoder_fields_and_ordering() {
        let bad_bool = [EventType::ClipboardCapabilities as u8, 2, 0, 1];
        assert!(matches!(
            ProtoEvent::decode(&bad_bool),
            Err(ProtocolError::InvalidBoolean(2))
        ));
        let mut cancel = vec![EventType::ClipboardTransferCancel as u8];
        cancel.extend_from_slice(&1u64.to_be_bytes());
        cancel.extend_from_slice(&1u64.to_be_bytes());
        cancel.push(9);
        assert!(matches!(
            ProtoEvent::decode(&cancel),
            Err(ProtocolError::InvalidCancelReason(9))
        ));
        let mut manifest = vec![EventType::ClipboardManifest as u8];
        manifest.extend_from_slice(&1u64.to_be_bytes());
        manifest.extend_from_slice(&1u32.to_be_bytes());
        manifest.extend_from_slice(&1u64.to_be_bytes());
        manifest.push(1);
        manifest.extend_from_slice(&1u16.to_be_bytes());
        manifest.push(0xff);
        assert!(matches!(
            ProtoEvent::decode(&manifest),
            Err(ProtocolError::InvalidPath(_))
        ));
        let mut progress = vec![EventType::ClipboardTransferProgress as u8];
        progress.extend_from_slice(&1u64.to_be_bytes());
        progress.extend_from_slice(&1u64.to_be_bytes());
        progress.extend_from_slice(&2u64.to_be_bytes());
        progress.extend_from_slice(&1u64.to_be_bytes());
        assert!(matches!(
            ProtoEvent::decode(&progress),
            Err(ProtocolError::InvalidProgress { .. })
        ));
        let mut chunk = vec![EventType::ClipboardFileChunk as u8];
        chunk.extend_from_slice(&1u64.to_be_bytes());
        chunk.extend_from_slice(&1u64.to_be_bytes());
        chunk.extend_from_slice(&1u64.to_be_bytes());
        chunk.extend_from_slice(&0u64.to_be_bytes());
        chunk.extend(std::iter::repeat_n(0, MAX_CLIPBOARD_FILE_CHUNK_SIZE + 1));
        assert!(matches!(
            ProtoEvent::decode(&chunk),
            Err(ProtocolError::TooLarge { .. })
        ));
        let mut validator = ClipboardFileChunkValidator::new(7, 1, 9, 20, 3).unwrap();
        assert!(validator.validate_chunk(7, 1, 8, 20, 1).is_err());
        assert!(validator.validate_chunk(7, 2, 9, 20, 1).is_err());
        assert!(validator.validate_chunk(8, 1, 9, 20, 1).is_err());
        assert!(validator.validate_chunk(7, 1, 9, 21, 1).is_err());
        assert!(validator.validate_chunk(7, 1, 9, 20, 4).is_err());
        assert!(validator.validate_chunk(7, 1, 9, 20, 3).is_ok());
        assert!(validator.validate_chunk(7, 1, 9, 20, 1).is_err());
        assert!(validator.validate_complete(7, 1, 9, 123_456).is_err());
        let mut terminal = ClipboardFileChunkValidator::new(7, 1, 9, 20, 3).unwrap();
        assert!(terminal.validate_chunk(7, 1, 9, 20, 2).is_ok());
        assert!(terminal.validate_complete(7, 1, 9, 22).is_ok());
        assert!(terminal.validate_complete(7, 1, 9, 22).is_err());
        assert!(ClipboardFileChunkValidator::new(7, 1, 0, 20, 3).is_err());
        assert!(ClipboardFileChunkValidator::new(7, 1, 9, 20, 0).is_err());
        assert!(
            ClipboardFileChunkValidator::new(7, 1, 9, 20, MAX_CLIPBOARD_FILE_CHUNK_SIZE as u64 + 1)
                .is_err()
        );
        assert!(
            ProtoEvent::ClipboardFileChunk {
                transfer_id: 1,
                file_id: 1,
                request_id: 1,
                offset: 0,
                data: vec![0; MAX_CLIPBOARD_FILE_CHUNK_SIZE]
            }
            .encode()
            .is_ok()
        );
        assert!(
            ProtoEvent::ClipboardFileChunk {
                transfer_id: 1,
                file_id: 1,
                request_id: 1,
                offset: 0,
                data: vec![0; MAX_CLIPBOARD_FILE_CHUNK_SIZE + 1]
            }
            .encode()
            .is_err()
        );
        let mut complete = vec![EventType::ClipboardFileComplete as u8];
        complete.extend_from_slice(&1u64.to_be_bytes());
        complete.extend_from_slice(&1u64.to_be_bytes());
        complete.extend_from_slice(&1u64.to_be_bytes());
        complete.extend_from_slice(&1u64.to_be_bytes());
        complete.extend(std::iter::repeat_n(0, 31));
        assert!(matches!(
            ProtoEvent::decode(&complete),
            Err(ProtocolError::Truncated)
        ));
        complete.push(0);
        assert!(ProtoEvent::decode(&complete).is_ok());
        complete.push(0);
        assert!(matches!(
            ProtoEvent::decode(&complete),
            Err(ProtocolError::TrailingBytes(1))
        ));
    }
    #[test]
    fn file_request_wire_validation_is_strict() {
        let event = ProtoEvent::ClipboardFileRequest {
            transfer_id: 7,
            file_id: 1,
            request_id: 9,
            offset: 20,
            length: 3,
        };
        let encoded = event.encode().unwrap();
        assert_eq!(
            ProtoEvent::decode(&encoded).unwrap().encode().unwrap(),
            encoded
        );
        assert!(
            ProtoEvent::ClipboardFileRequest {
                transfer_id: 7,
                file_id: 1,
                request_id: 0,
                offset: 20,
                length: 3
            }
            .encode()
            .is_err()
        );
        assert!(
            ProtoEvent::ClipboardFileRequest {
                transfer_id: 7,
                file_id: 1,
                request_id: 9,
                offset: 20,
                length: 0
            }
            .encode()
            .is_err()
        );
        assert!(
            ProtoEvent::ClipboardFileRequest {
                transfer_id: 7,
                file_id: 1,
                request_id: 9,
                offset: 20,
                length: MAX_CLIPBOARD_FILE_CHUNK_SIZE as u64 + 1
            }
            .encode()
            .is_err()
        );
        assert!(
            ProtoEvent::ClipboardFileRequest {
                transfer_id: 7,
                file_id: 1,
                request_id: 9,
                offset: u64::MAX,
                length: 1
            }
            .encode()
            .is_err()
        );
        for (slot, value) in [
            (9, 0),
            (17, 0),
            (33, 0),
            (33, MAX_CLIPBOARD_FILE_CHUNK_SIZE as u64 + 1),
            (25, u64::MAX),
        ] {
            let mut malformed = encoded.clone();
            malformed[slot..slot + 8].copy_from_slice(&value.to_be_bytes());
            assert!(ProtoEvent::decode(&malformed).is_err());
        }
        for n in 0..encoded.len() {
            assert!(matches!(
                ProtoEvent::decode(&encoded[..n]),
                Err(ProtocolError::Truncated)
            ));
        }
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(matches!(
            ProtoEvent::decode(&trailing),
            Err(ProtocolError::TrailingBytes(1))
        ));
    }
}
