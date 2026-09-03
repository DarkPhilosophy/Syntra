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
        match EventType::try_from(get_u8(&mut buf)?)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: get_u32(&mut buf)?,
                    dx: get_f64(&mut buf)?,
                    dy: get_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: get_u32(&mut buf)?,
                    button: get_u32(&mut buf)?,
                    state: get_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: get_u32(&mut buf)?,
                axis: get_u8(&mut buf)?,
                value: get_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: get_u8(&mut buf)?,
                    value: get_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: get_u32(&mut buf)?,
                key: get_u32(&mut buf)?,
                state: get_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: get_u32(&mut buf)?,
                    latched: get_u32(&mut buf)?,
                    locked: get_u32(&mut buf)?,
                    group: get_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(get_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(get_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(get_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(get_u32(&mut buf)?)),
            EventType::Hello => {
                let mut commit = [0; 8];
                commit.copy_from_slice(take(&mut buf, 8)?);
                Ok(Self::Hello { commit })
            }
            EventType::ClipboardStart => {
                let transfer_id = get_u64(&mut buf)?;
                let total_len = get_u32(&mut buf)?;
                let chunks = get_u32(&mut buf)?;
                validate_clipboard_start(total_len, chunks)?;
                Ok(Self::ClipboardStart {
                    transfer_id,
                    total_len,
                    chunks,
                })
            }
            EventType::ClipboardImageStart => {
                let transfer_id = get_u64(&mut buf)?;
                let width = get_u32(&mut buf)?;
                let height = get_u32(&mut buf)?;
                let total_len = get_u32(&mut buf)?;
                let chunks = get_u32(&mut buf)?;
                validate_clipboard_image(width, height, total_len, chunks)?;
                Ok(Self::ClipboardImageStart {
                    transfer_id,
                    width,
                    height,
                    total_len,
                    chunks,
                })
            }
            EventType::ClipboardChunk => Ok(Self::ClipboardChunk {
                transfer_id: get_u64(&mut buf)?,
                index: get_u32(&mut buf)?,
                data: buf.to_vec(),
            }),
        }
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
}
