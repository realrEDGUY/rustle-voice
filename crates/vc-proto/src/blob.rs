//! Blob streams: one QUIC unidirectional stream per image or screen frame, laid out
//! as `[header len u32 LE][json header][raw body bytes until FIN]`.
//!
//! Only the pure header encoding and format sniffing live here; the stream IO is in
//! the server and client crates.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::MAX_BLOB_HEADER;
use crate::ids::{ChannelId, SessionId};

/// Header a client sends at the start of a blob stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientBlob {
    /// A PNG or JPEG posted to the sender's channel chat.
    Image,
    /// One frame of the sender's screen share: a JPEG/PNG, or an H.264 access unit
    /// that decodes on its own (SPS + PPS + IDR).
    ScreenFrame,
}

/// Header a server sends at the start of a blob stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerBlob {
    Image {
        from: SessionId,
        channel: ChannelId,
        ts_ms: u64,
    },
    ScreenFrame {
        from: SessionId,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob header of {0} bytes exceeds the {MAX_BLOB_HEADER}-byte limit")]
    HeaderTooLarge(usize),
    #[error("empty blob header")]
    EmptyHeader,
    #[error("bad blob header json: {0}")]
    Json(String),
}

/// Encode a header as `[len u32 LE][json]`.
pub fn encode_header<T: Serialize>(header: &T) -> Result<Vec<u8>, BlobError> {
    let json = serde_json::to_vec(header).map_err(|e| BlobError::Json(e.to_string()))?;
    if json.len() > MAX_BLOB_HEADER {
        return Err(BlobError::HeaderTooLarge(json.len()));
    }
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Validate the 4-byte length prefix of a header.
pub fn header_len(prefix: [u8; 4]) -> Result<usize, BlobError> {
    let len = u32::from_le_bytes(prefix) as usize;
    if len == 0 {
        return Err(BlobError::EmptyHeader);
    }
    if len > MAX_BLOB_HEADER {
        return Err(BlobError::HeaderTooLarge(len));
    }
    Ok(len)
}

/// Decode the JSON part of a header (without the length prefix).
pub fn decode_header<T: DeserializeOwned>(json: &[u8]) -> Result<T, BlobError> {
    serde_json::from_slice(json).map_err(|e| BlobError::Json(e.to_string()))
}

/// Image formats accepted in chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
}

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const JPEG_MAGIC: [u8; 3] = [0xFF, 0xD8, 0xFF];

/// Identify a PNG or JPEG by its magic bytes. Never decodes.
pub fn sniff(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&PNG_MAGIC) {
        Some(ImageKind::Png)
    } else if bytes.starts_with(&JPEG_MAGIC) {
        Some(ImageKind::Jpeg)
    } else {
        None
    }
}

/// Screen-share frame formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenKind {
    Png,
    Jpeg,
    H264,
}

/// Identify a screen-share frame: PNG, JPEG, or an Annex-B H.264 access unit whose
/// first NAL is an SPS (7) or access unit delimiter (9). Never decodes; a start-code
/// sniff is not validation.
pub fn sniff_screen(bytes: &[u8]) -> Option<ScreenKind> {
    match sniff(bytes) {
        Some(ImageKind::Png) => return Some(ScreenKind::Png),
        Some(ImageKind::Jpeg) => return Some(ScreenKind::Jpeg),
        None => {}
    }
    let nal = if bytes.starts_with(&[0, 0, 0, 1]) {
        bytes.get(4)
    } else if bytes.starts_with(&[0, 0, 1]) {
        bytes.get(3)
    } else {
        None
    }?;
    matches!(nal & 0x1F, 7 | 9).then_some(ScreenKind::H264)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(bytes: &[u8]) -> ([u8; 4], &[u8]) {
        (bytes[..4].try_into().unwrap(), &bytes[4..])
    }

    #[test]
    fn header_roundtrip() {
        for h in [ClientBlob::Image, ClientBlob::ScreenFrame] {
            let bytes = encode_header(&h).unwrap();
            let (prefix, json) = split(&bytes);
            assert_eq!(header_len(prefix).unwrap(), json.len());
            assert_eq!(decode_header::<ClientBlob>(json).unwrap(), h);
        }
        for h in [
            ServerBlob::Image {
                from: SessionId(3),
                channel: ChannelId(1),
                ts_ms: 1_700_000_000_000,
            },
            ServerBlob::ScreenFrame { from: SessionId(9) },
        ] {
            let bytes = encode_header(&h).unwrap();
            let (prefix, json) = split(&bytes);
            assert_eq!(header_len(prefix).unwrap(), json.len());
            assert_eq!(decode_header::<ServerBlob>(json).unwrap(), h);
        }
    }

    #[test]
    fn header_over_limit_rejected() {
        #[derive(Serialize)]
        struct Big {
            pad: String,
        }
        let big = Big {
            pad: "x".repeat(MAX_BLOB_HEADER),
        };
        assert!(matches!(
            encode_header(&big),
            Err(BlobError::HeaderTooLarge(_))
        ));
        assert!(matches!(
            header_len(((MAX_BLOB_HEADER as u32) + 1).to_le_bytes()),
            Err(BlobError::HeaderTooLarge(_))
        ));
        assert_eq!(
            header_len((MAX_BLOB_HEADER as u32).to_le_bytes()).unwrap(),
            MAX_BLOB_HEADER
        );
    }

    #[test]
    fn header_zero_len_rejected() {
        assert!(matches!(header_len([0; 4]), Err(BlobError::EmptyHeader)));
    }

    #[test]
    fn header_garbage_json() {
        assert!(matches!(
            decode_header::<ClientBlob>(b"{not json"),
            Err(BlobError::Json(_))
        ));
        assert!(matches!(
            decode_header::<ClientBlob>(br#"{"t":"video"}"#),
            Err(BlobError::Json(_))
        ));
    }

    #[test]
    fn sniff_formats() {
        assert_eq!(sniff(&PNG_MAGIC), Some(ImageKind::Png));
        assert_eq!(
            sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10]),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(sniff(b"GIF89a....."), None);
        assert_eq!(sniff(&PNG_MAGIC[..7]), None);
        assert_eq!(sniff(&[]), None);
        // H.264 is a screen format only, never a chat image.
        assert_eq!(sniff(&[0, 0, 0, 1, 0x67, 0x42]), None);
    }

    #[test]
    fn sniff_screen_formats() {
        assert_eq!(sniff_screen(&PNG_MAGIC), Some(ScreenKind::Png));
        assert_eq!(
            sniff_screen(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ScreenKind::Jpeg)
        );
        assert_eq!(
            sniff_screen(&[0, 0, 0, 1, 0x67, 0x42]),
            Some(ScreenKind::H264)
        );
        assert_eq!(sniff_screen(&[0, 0, 1, 0x09, 0xF0]), Some(ScreenKind::H264));
        // Slice NAL first (no SPS / AUD) is not accepted.
        assert_eq!(sniff_screen(&[0, 0, 0, 1, 0x65, 0x88]), None);
        assert_eq!(sniff_screen(&[0, 0, 0, 1]), None);
        assert_eq!(sniff_screen(b"GIF89a"), None);
        assert_eq!(sniff_screen(&[]), None);
    }
}
