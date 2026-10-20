//! Byte-level encoding primitives of the peer protocol.
//!
//! Everything here is big-endian and length-checked: a reader never indexes
//! past the end of its input and reports [`ProtocolError::Truncated`] instead.
//! These helpers define the wire format of scalars; the message codec in the
//! crate root composes them, and the byte layout of every message is covered
//! by its round-trip and fixed-vector tests. Moving or renaming a helper must
//! never change the bytes it produces.

use crate::ProtocolError;

/// Splits `len` bytes off the front of `buf`.
pub(crate) fn take<'a>(buf: &mut &'a [u8], len: usize) -> Result<&'a [u8], ProtocolError> {
    if buf.len() < len {
        return Err(ProtocolError::Truncated);
    }
    let (value, rest) = buf.split_at(len);
    *buf = rest;
    Ok(value)
}

pub(crate) fn get_u8(buf: &mut &[u8]) -> Result<u8, ProtocolError> {
    Ok(take(buf, 1)?[0])
}
pub(crate) fn get_u16(buf: &mut &[u8]) -> Result<u16, ProtocolError> {
    Ok(u16::from_be_bytes(
        take(buf, 2)?.try_into().expect("length checked"),
    ))
}
pub(crate) fn get_u32(buf: &mut &[u8]) -> Result<u32, ProtocolError> {
    Ok(u32::from_be_bytes(
        take(buf, 4)?.try_into().expect("length checked"),
    ))
}
pub(crate) fn get_u64(buf: &mut &[u8]) -> Result<u64, ProtocolError> {
    Ok(u64::from_be_bytes(
        take(buf, 8)?.try_into().expect("length checked"),
    ))
}
pub(crate) fn get_i32(buf: &mut &[u8]) -> Result<i32, ProtocolError> {
    Ok(i32::from_be_bytes(
        take(buf, 4)?.try_into().expect("length checked"),
    ))
}
pub(crate) fn get_f64(buf: &mut &[u8]) -> Result<f64, ProtocolError> {
    Ok(f64::from_be_bytes(
        take(buf, 8)?.try_into().expect("length checked"),
    ))
}
pub(crate) fn get_bool(buf: &mut &[u8]) -> Result<bool, ProtocolError> {
    match get_u8(buf)? {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(ProtocolError::InvalidBoolean(value)),
    }
}

pub(crate) fn put_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_be_bytes());
}
pub(crate) fn put_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_be_bytes());
}
pub(crate) fn put_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_be_bytes());
}
pub(crate) fn put_i32(buf: &mut Vec<u8>, value: i32) {
    buf.extend_from_slice(&value.to_be_bytes());
}
pub(crate) fn put_f64(buf: &mut Vec<u8>, value: f64) {
    buf.extend_from_slice(&value.to_be_bytes());
}

pub(crate) fn put_optional_u64(buf: &mut Vec<u8>, value: Option<u64>) {
    buf.push(value.is_some() as u8);
    if let Some(value) = value {
        put_u64(buf, value);
    }
}

pub(crate) fn get_optional_u64(buf: &mut &[u8]) -> Result<Option<u64>, ProtocolError> {
    Ok(if get_bool(buf)? {
        Some(get_u64(buf)?)
    } else {
        None
    })
}

pub(crate) fn put_optional_string(
    buf: &mut Vec<u8>,
    value: Option<&str>,
) -> Result<(), ProtocolError> {
    match value {
        None => put_u16(buf, 0),
        Some(value) => {
            if value.is_empty() || value.len() > u16::MAX as usize {
                return Err(ProtocolError::InvalidHistory("invalid error string"));
            }
            put_u16(buf, value.len() as u16);
            buf.extend_from_slice(value.as_bytes());
        }
    }
    Ok(())
}

pub(crate) fn get_optional_string(buf: &mut &[u8]) -> Result<Option<String>, ProtocolError> {
    let len = get_u16(buf)? as usize;
    if len == 0 {
        return Ok(None);
    }
    let value = std::str::from_utf8(take(buf, len)?)
        .map_err(|_| ProtocolError::InvalidHistory("non-UTF-8 error string"))?;
    Ok(Some(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scalars are big-endian: the byte order is the wire format.
    #[test]
    fn scalars_are_big_endian() {
        let mut buf = Vec::new();
        put_u16(&mut buf, 0x0102);
        put_u32(&mut buf, 0x0304_0506);
        put_u64(&mut buf, 0x0708_090a_0b0c_0d0e);
        put_i32(&mut buf, -2);
        assert_eq!(
            buf,
            [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 0xff, 0xff, 0xff, 0xfe
            ]
        );
        let mut reader = &buf[..];
        assert_eq!(get_u16(&mut reader).unwrap(), 0x0102);
        assert_eq!(get_u32(&mut reader).unwrap(), 0x0304_0506);
        assert_eq!(get_u64(&mut reader).unwrap(), 0x0708_090a_0b0c_0d0e);
        assert_eq!(get_i32(&mut reader).unwrap(), -2);
        assert!(reader.is_empty());
    }

    #[test]
    fn a_short_read_is_truncation_not_a_panic() {
        let mut reader: &[u8] = &[1, 2, 3];
        assert!(matches!(
            get_u32(&mut reader),
            Err(ProtocolError::Truncated)
        ));
        assert_eq!(reader, [1, 2, 3], "a failed read consumes nothing");
    }

    #[test]
    fn booleans_accept_only_zero_and_one() {
        assert!(!get_bool(&mut &[0u8][..]).unwrap());
        assert!(get_bool(&mut &[1u8][..]).unwrap());
        assert!(matches!(
            get_bool(&mut &[2u8][..]),
            Err(ProtocolError::InvalidBoolean(2))
        ));
    }

    #[test]
    fn optional_values_round_trip_with_their_presence_byte() {
        let mut buf = Vec::new();
        put_optional_u64(&mut buf, None);
        put_optional_u64(&mut buf, Some(9));
        assert_eq!(buf, [0, 1, 0, 0, 0, 0, 0, 0, 0, 9]);
        let mut reader = &buf[..];
        assert_eq!(get_optional_u64(&mut reader).unwrap(), None);
        assert_eq!(get_optional_u64(&mut reader).unwrap(), Some(9));

        let mut text = Vec::new();
        put_optional_string(&mut text, Some("hi")).unwrap();
        put_optional_string(&mut text, None).unwrap();
        assert_eq!(text, [0, 2, b'h', b'i', 0, 0]);
        let mut reader = &text[..];
        assert_eq!(
            get_optional_string(&mut reader).unwrap().as_deref(),
            Some("hi")
        );
        assert_eq!(get_optional_string(&mut reader).unwrap(), None);
        assert!(put_optional_string(&mut Vec::new(), Some("")).is_err());
    }
}
