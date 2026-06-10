//! Klipper API server framing: JSON objects separated by the `0x03` (ETX) byte.

use anyhow::Result;
use bytes::{Buf, BufMut, BytesMut};
use serde_json::Value;
use tokio_util::codec::{Decoder, Encoder};

const ETX: u8 = 0x03;
/// Guard against unbounded buffer growth from a malformed peer.
const MAX_FRAME: usize = 4 * 1024 * 1024;

pub struct EtxCodec;

impl Decoder for EtxCodec {
    type Item = Value;
    type Error = anyhow::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Value>> {
        loop {
            match buf.iter().position(|&b| b == ETX) {
                Some(pos) => {
                    let frame = buf.split_to(pos);
                    buf.advance(1); // consume the ETX
                    if frame.is_empty() {
                        // Tolerate empty frames (heartbeats); look for the next one.
                        continue;
                    }
                    let value = serde_json::from_slice(&frame)?;
                    return Ok(Some(value));
                }
                None => {
                    if buf.len() > MAX_FRAME {
                        anyhow::bail!("frame exceeds maximum size ({MAX_FRAME} bytes)");
                    }
                    return Ok(None);
                }
            }
        }
    }
}

impl Encoder<Value> for EtxCodec {
    type Error = anyhow::Error;

    fn encode(&mut self, item: Value, dst: &mut BytesMut) -> Result<()> {
        let bytes = serde_json::to_vec(&item)?;
        dst.reserve(bytes.len() + 1);
        dst.extend_from_slice(&bytes);
        dst.put_u8(ETX);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_multiple_frames_in_one_buffer() {
        let mut codec = EtxCodec;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(b"{\"id\":1}\x03{\"id\":2}\x03");
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(json!({"id":1})));
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(json!({"id":2})));
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn waits_for_partial_frame() {
        let mut codec = EtxCodec;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(b"{\"id\":");
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
        buf.extend_from_slice(b"1}\x03");
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(json!({"id":1})));
    }

    #[test]
    fn skips_empty_frames() {
        let mut codec = EtxCodec;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(b"\x03\x03{\"a\":true}\x03");
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(json!({"a":true})));
    }

    #[test]
    fn roundtrip_encode() {
        let mut codec = EtxCodec;
        let mut buf = BytesMut::new();
        codec.encode(json!({"ok":1}), &mut buf).unwrap();
        assert_eq!(&buf[..], b"{\"ok\":1}\x03");
    }
}
