//! Voice datagram layouts.
//!
//! Client → server: `[seq u32 LE][payload]`.
//! Server → client: `[speaker u32 LE][seq u32 LE][payload]`.
//! The payload must be exactly `ADPCM_FRAME_BYTES`.
//!
//! Stream sound (a screen sharer's system audio) uses the same layouts, tagged with
//! [`STREAM_BIT`]: the client sets the bit on `seq` (every stream seq carries it, so
//! ordering in the jitter buffer holds), the server forwards `seq` unchanged and sets
//! the bit on `speaker`. Session ids never use the bit, so a tagged speaker is its
//! own mixer source and never collides with the sharer's mic.

use crate::{ADPCM_FRAME_BYTES, SessionId};

/// Tag bit that marks a voice datagram as stream sound rather than mic audio.
pub const STREAM_BIT: u32 = 1 << 31;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VoiceError {
    #[error("voice datagram has the wrong length")]
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientVoice<'a> {
    pub seq: u32,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerVoice<'a> {
    pub speaker: SessionId,
    pub seq: u32,
    pub payload: &'a [u8],
}

impl<'a> ClientVoice<'a> {
    pub const LEN: usize = 4 + ADPCM_FRAME_BYTES;

    /// True when this datagram carries stream sound.
    pub fn is_stream(&self) -> bool {
        self.seq & STREAM_BIT != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + self.payload.len());
        v.extend_from_slice(&self.seq.to_le_bytes());
        v.extend_from_slice(self.payload);
        v
    }

    pub fn parse(bytes: &'a [u8]) -> Result<Self, VoiceError> {
        if bytes.len() != Self::LEN {
            return Err(VoiceError::Short);
        }
        Ok(Self {
            seq: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            payload: &bytes[4..],
        })
    }
}

impl<'a> ServerVoice<'a> {
    pub const LEN: usize = 8 + ADPCM_FRAME_BYTES;

    /// The sharer whose stream sound this is, or `None` for mic audio.
    pub fn stream_of(&self) -> Option<SessionId> {
        (self.speaker.0 & STREAM_BIT != 0).then_some(SessionId(self.speaker.0 & !STREAM_BIT))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + self.payload.len());
        v.extend_from_slice(&self.speaker.0.to_le_bytes());
        v.extend_from_slice(&self.seq.to_le_bytes());
        v.extend_from_slice(self.payload);
        v
    }

    /// Prepend a speaker header to a client datagram that already parsed OK.
    pub fn from_client(speaker: SessionId, client_bytes: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + client_bytes.len());
        v.extend_from_slice(&speaker.0.to_le_bytes());
        v.extend_from_slice(client_bytes);
        v
    }

    pub fn parse(bytes: &'a [u8]) -> Result<Self, VoiceError> {
        if bytes.len() != Self::LEN {
            return Err(VoiceError::Short);
        }
        Ok(Self {
            speaker: SessionId(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
            seq: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            payload: &bytes[8..],
        })
    }
}

/// Parse helpers matching the plan's names.
pub fn parse_client(bytes: &[u8]) -> Result<ClientVoice<'_>, VoiceError> {
    ClientVoice::parse(bytes)
}

pub fn parse_server(bytes: &[u8]) -> Result<ServerVoice<'_>, VoiceError> {
    ServerVoice::parse(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Vec<u8> {
        (0..ADPCM_FRAME_BYTES).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn voice_header_parse() {
        let p = payload();
        let c = ClientVoice {
            seq: 0xDEADBEEF,
            payload: &p,
        }
        .encode();
        assert_eq!(&c[..4], &0xDEADBEEFu32.to_le_bytes());
        let parsed = parse_client(&c).unwrap();
        assert_eq!(parsed.seq, 0xDEADBEEF);
        assert_eq!(parsed.payload, &p[..]);

        let s = ServerVoice::from_client(SessionId(9), &c);
        let sp = parse_server(&s).unwrap();
        assert_eq!(sp.speaker, SessionId(9));
        assert_eq!(sp.seq, 0xDEADBEEF);
        assert_eq!(sp.payload, &p[..]);
        let s2 = ServerVoice {
            speaker: SessionId(9),
            seq: 0xDEADBEEF,
            payload: &p,
        }
        .encode();
        assert_eq!(s, s2);
    }

    #[test]
    fn stream_bit_roundtrip() {
        let p = payload();
        let c = ClientVoice {
            seq: 5 | STREAM_BIT,
            payload: &p,
        }
        .encode();
        let parsed = parse_client(&c).unwrap();
        assert!(parsed.is_stream());
        assert_eq!(parsed.seq, 5 | STREAM_BIT);
        let s = ServerVoice::from_client(SessionId(9 | STREAM_BIT), &c);
        let sp = parse_server(&s).unwrap();
        assert_eq!(sp.stream_of(), Some(SessionId(9)));
        assert_eq!(sp.seq, 5 | STREAM_BIT);

        let plain = ClientVoice {
            seq: 5,
            payload: &p,
        }
        .encode();
        assert!(!parse_client(&plain).unwrap().is_stream());
        let s = ServerVoice::from_client(SessionId(9), &plain);
        assert_eq!(parse_server(&s).unwrap().stream_of(), None);
    }

    #[test]
    fn voice_rejects_short() {
        assert_eq!(parse_client(&[]), Err(VoiceError::Short));
        assert_eq!(parse_client(&[1, 2, 3]), Err(VoiceError::Short));
        assert_eq!(parse_server(&[1, 2, 3, 4, 5, 6, 7]), Err(VoiceError::Short));
        // Header only, no payload.
        assert_eq!(parse_client(&[0; 4]), Err(VoiceError::Short));
        assert_eq!(parse_server(&[0; 8]), Err(VoiceError::Short));
    }

    #[test]
    fn voice_rejects_wrong_payload_len() {
        assert_eq!(
            parse_client(&vec![0; ClientVoice::LEN - 1]),
            Err(VoiceError::Short)
        );
        assert_eq!(
            parse_client(&vec![0; ClientVoice::LEN + 1]),
            Err(VoiceError::Short)
        );
        assert!(parse_client(&vec![0; ClientVoice::LEN]).is_ok());
        assert_eq!(
            parse_server(&vec![0; ServerVoice::LEN + 1]),
            Err(VoiceError::Short)
        );
        assert!(parse_server(&vec![0; ServerVoice::LEN]).is_ok());
    }
}
