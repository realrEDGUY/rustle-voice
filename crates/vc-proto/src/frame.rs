//! Length-prefixed JSON framing for the control stream: `[len u32 LE][json]`.

use serde::{Serialize, de::DeserializeOwned};

use crate::MAX_CONTROL_FRAME;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_CONTROL_FRAME}-byte limit")]
    Oversize(usize),
    #[error("bad frame json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encode one message as a length-prefixed JSON frame.
pub fn encode(msg: &impl Serialize) -> Result<Vec<u8>, FrameError> {
    let json = serde_json::to_vec(msg)?;
    if json.len() > MAX_CONTROL_FRAME {
        return Err(FrameError::Oversize(json.len()));
    }
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Incremental decoder that buffers partial reads.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete message, `Ok(None)` if more bytes are needed.
    #[allow(clippy::should_implement_trait)]
    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_CONTROL_FRAME {
            return Err(FrameError::Oversize(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = serde_json::from_slice(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        Ok(Some(msg?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientMsg;

    #[test]
    fn frame_roundtrip() {
        let m = ClientMsg::Chat {
            text: "hello".into(),
        };
        let bytes = encode(&m).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize,
            bytes.len() - 4
        );
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next::<ClientMsg>().unwrap(), Some(m));
        assert_eq!(d.next::<ClientMsg>().unwrap(), None);
    }

    #[test]
    fn frame_partial_reads() {
        let a = ClientMsg::Ping { nonce: 1 };
        let b = ClientMsg::Chat { text: "two".into() };
        let mut all = encode(&a).unwrap();
        all.extend(encode(&b).unwrap());
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        for byte in &all {
            d.push(std::slice::from_ref(byte));
            while let Some(m) = d.next::<ClientMsg>().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn frame_rejects_oversize() {
        let mut d = FrameDecoder::new();
        d.push(&((MAX_CONTROL_FRAME as u32) + 1).to_le_bytes());
        assert!(matches!(
            d.next::<ClientMsg>(),
            Err(FrameError::Oversize(_))
        ));
        let big = ClientMsg::Chat {
            text: "x".repeat(MAX_CONTROL_FRAME + 1),
        };
        assert!(matches!(encode(&big), Err(FrameError::Oversize(_))));
        // Exactly at the limit is accepted by the decoder's length check.
        let mut d = FrameDecoder::new();
        d.push(&(MAX_CONTROL_FRAME as u32).to_le_bytes());
        assert!(matches!(d.next::<ClientMsg>(), Ok(None)));
    }
}
