//! Framing: a 4-byte little-endian length, then that many bytes of JSON.

use core::fmt;
use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// The longest frame either side accepts (4 MiB): an `IoWrite` of
/// [`MAX_IO_BLOCKS`](crate::MAX_IO_BLOCKS) blocks as hexadecimal is about
/// 2 MiB and fits with room to spare, and a peer that sends more is broken
/// or hostile. The bound is also what one idle connection can make the
/// daemon allocate, so it stays small.
pub const MAX_FRAME_BYTES: u32 = 4 << 20;

/// Why a frame could not be read or written.
#[derive(Debug)]
pub enum WireError {
    /// The stream failed or closed.
    Io(io::Error),
    /// The frame is longer than [`MAX_FRAME_BYTES`].
    TooLong {
        /// The length announced.
        bytes: u32,
    },
    /// The JSON does not decode as the expected message.
    Decode(serde_json::Error),
    /// The message does not encode (never for the protocol's types).
    Encode(serde_json::Error),
    /// The peer closed the stream before a whole frame arrived.
    Closed,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "control stream: {e}"),
            Self::TooLong { bytes } => {
                write!(
                    f,
                    "a {bytes}-byte frame exceeds the {MAX_FRAME_BYTES}-byte bound"
                )
            }
            Self::Decode(e) => write!(f, "control message does not decode: {e}"),
            Self::Encode(e) => write!(f, "control message does not encode: {e}"),
            Self::Closed => f.write_str("the control stream closed"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Self::Closed
        } else {
            Self::Io(e)
        }
    }
}

/// Writes one frame.
pub fn write_frame<T: Serialize, W: Write>(w: &mut W, message: &T) -> Result<(), WireError> {
    let body = serde_json::to_vec(message).map_err(WireError::Encode)?;
    let len = u32::try_from(body.len()).map_err(|_| WireError::TooLong { bytes: u32::MAX })?;
    if len > MAX_FRAME_BYTES {
        return Err(WireError::TooLong { bytes: len });
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Reads one frame. `Ok(None)` when the stream closed cleanly between
/// frames.
pub fn read_frame<T: DeserializeOwned, R: Read>(r: &mut R) -> Result<Option<T>, WireError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r.read(&mut len[got..])?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(WireError::Closed);
        }
        got += n;
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME_BYTES {
        return Err(WireError::TooLong { bytes: len });
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(WireError::Decode)
}

/// Lower-case hexadecimal of `bytes`.
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    out
}

/// The bytes of a hexadecimal string (either case); `None` when a
/// character is not a digit or the length is odd.
#[must_use]
pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        out.push((digit(pair[0])? << 4) | digit(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Request, Response};

    #[test]
    fn frames_round_trip_and_bounds_hold() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::Ping).unwrap();
        write_frame(
            &mut buf,
            &Request::VolumeCreate {
                name: "data".into(),
                size_bytes: 1 << 30,
            },
        )
        .unwrap();
        let mut cursor = io::Cursor::new(buf);
        let a: Option<Request> = read_frame(&mut cursor).unwrap();
        assert_eq!(a, Some(Request::Ping));
        let b: Option<Request> = read_frame(&mut cursor).unwrap();
        assert!(matches!(b, Some(Request::VolumeCreate { .. })));
        let c: Option<Request> = read_frame(&mut cursor).unwrap();
        assert!(c.is_none(), "a clean close between frames");
        // A length past the bound is refused before anything is allocated.
        let mut bad = io::Cursor::new((MAX_FRAME_BYTES + 1).to_le_bytes().to_vec());
        assert!(matches!(
            read_frame::<Request, _>(&mut bad),
            Err(WireError::TooLong { .. })
        ));
        // A truncated frame is a closed stream.
        let mut short = io::Cursor::new(vec![9, 0, 0, 0, b'{']);
        assert!(matches!(
            read_frame::<Request, _>(&mut short),
            Err(WireError::Closed)
        ));
        let mut partial = io::Cursor::new(vec![9, 0]);
        assert!(matches!(
            read_frame::<Request, _>(&mut partial),
            Err(WireError::Closed)
        ));
        // Garbage inside a frame is a decode error.
        let mut garbage = Vec::new();
        garbage.extend_from_slice(&4u32.to_le_bytes());
        garbage.extend_from_slice(b"nope");
        let mut garbage = io::Cursor::new(garbage);
        assert!(matches!(
            read_frame::<Response, _>(&mut garbage),
            Err(WireError::Decode(_))
        ));
    }

    #[test]
    fn hex_round_trips() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let text = encode_hex(&bytes);
        assert_eq!(text.len(), 512);
        assert_eq!(decode_hex(&text).unwrap(), bytes);
        assert_eq!(decode_hex(&text.to_uppercase()).unwrap(), bytes);
        assert!(decode_hex("abc").is_none());
        assert!(decode_hex("zz").is_none());
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
    }
}
