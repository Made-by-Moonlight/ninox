//! Frame codec for [`crate::protocol`]:
//! `u32 LE frame_len | u32 LE header_len | header JSON | payload bytes`,
//! where `frame_len` counts everything after itself.
//!
//! Both ends enforce [`MAX_FRAME_BYTES`] before allocating, so a corrupt or
//! hostile length prefix can never make the host allocate unbounded memory.

use std::io;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::MAX_FRAME_BYTES;

/// A decoded frame whose header has not been interpreted yet.
pub struct RawFrame {
    pub header: Vec<u8>,
    pub payload: Vec<u8>,
}

impl RawFrame {
    pub fn parse_header<T: DeserializeOwned>(&self) -> io::Result<T> {
        serde_json::from_slice(&self.header).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Encode one frame into a contiguous buffer (one `write_all`, so frames from
/// different writers sharing a socket can never interleave mid-frame).
pub fn encode_frame<H: Serialize>(header: &H, payload: &[u8]) -> io::Result<Vec<u8>> {
    let header = serde_json::to_vec(header).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let frame_len = 4 + header.len() + payload.len();
    if frame_len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {frame_len} bytes exceeds MAX_FRAME_BYTES ({MAX_FRAME_BYTES})"),
        ));
    }
    let mut buf = Vec::with_capacity(4 + frame_len);
    buf.extend_from_slice(&(frame_len as u32).to_le_bytes());
    buf.extend_from_slice(&(header.len() as u32).to_le_bytes());
    buf.extend_from_slice(&header);
    buf.extend_from_slice(payload);
    Ok(buf)
}

pub async fn write_frame<W, H>(w: &mut W, header: &H, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    H: Serialize,
{
    let buf = encode_frame(header, payload)?;
    w.write_all(&buf).await?;
    w.flush().await
}

/// `Ok(None)` on a clean EOF at a frame boundary; EOF mid-frame is an error.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<RawFrame>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let frame_len = u32::from_le_bytes(len) as usize;
    validate_frame_len(frame_len)?;
    let mut body = vec![0u8; frame_len];
    r.read_exact(&mut body).await?;
    split_body(body).map(Some)
}

/// Blocking variant for the handoff path, which runs on a std socket.
pub fn read_frame_blocking<R: io::Read>(r: &mut R) -> io::Result<Option<RawFrame>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let frame_len = u32::from_le_bytes(len) as usize;
    validate_frame_len(frame_len)?;
    let mut body = vec![0u8; frame_len];
    r.read_exact(&mut body)?;
    split_body(body).map(Some)
}

pub fn write_frame_blocking<W: io::Write, H: Serialize>(w: &mut W, header: &H, payload: &[u8]) -> io::Result<()> {
    let buf = encode_frame(header, payload)?;
    w.write_all(&buf)?;
    w.flush()
}

fn validate_frame_len(frame_len: usize) -> io::Result<()> {
    if !(4..=MAX_FRAME_BYTES).contains(&frame_len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid frame length {frame_len}"),
        ));
    }
    Ok(())
}

fn split_body(mut body: Vec<u8>) -> io::Result<RawFrame> {
    let header_len = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if header_len > body.len() - 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("header length {header_len} exceeds frame"),
        ));
    }
    let payload = body.split_off(4 + header_len);
    body.drain(..4);
    Ok(RawFrame { header: body, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_header_and_payload() {
        let buf = encode_frame(&serde_json::json!({"a": 1}), b"\x1b[0mhi").unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let f = read_frame(&mut cur).await.unwrap().unwrap();
        assert_eq!(f.header, br#"{"a":1}"#);
        assert_eq!(f.payload, b"\x1b[0mhi");
        assert!(read_frame(&mut cur).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_oversized_and_inconsistent_lengths() {
        let huge = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        assert!(read_frame(&mut &huge[..]).await.is_err());

        let mut bad = Vec::new();
        bad.extend_from_slice(&8u32.to_le_bytes());
        bad.extend_from_slice(&100u32.to_le_bytes());
        bad.extend_from_slice(b"abcd");
        assert!(read_frame(&mut &bad[..]).await.is_err());

        // EOF mid-frame is an error, not a clean close.
        let mut short = Vec::new();
        short.extend_from_slice(&20u32.to_le_bytes());
        short.extend_from_slice(&2u32.to_le_bytes());
        assert!(read_frame(&mut &short[..]).await.is_err());
    }
}
