//! `latest.signed`: the signed description of the newest release.
//!
//! Wire format: line 1 is the 128-hex Ed25519 signature, then `\n`, then the manifest
//! JSON bytes exactly as signed. The signed message is `b"vc-update-v1\n" ‖ json`, so a
//! signature over some other kind of document can never be replayed as a manifest.

use serde::{Deserialize, Serialize};

use super::state::State;
use super::{MAX_EXE_SIZE, parse_version};

/// Domain-separation prefix of the signed message.
pub const DOMAIN: &[u8] = b"vc-update-v1\n";
/// Largest `latest.signed` accepted.
pub const MAX_FILE: usize = 64 * 1024;
pub const PRODUCT: &str = "vc-client";
pub const TARGET: &str = "x86_64-pc-windows-msvc";

/// Unknown fields are allowed (forward compatibility); the signature covers every byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub product: String,
    pub channel: String,
    pub target: String,
    pub version: String,
    /// Lowercase hex SHA-256 of the exe.
    pub sha256: String,
    pub size: u64,
    /// Unix seconds this manifest was signed. Never goes backwards (replay ratchet).
    pub issued: u64,
    /// Unix seconds after which the manifest is refused (freeze protection).
    pub expires: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    TooLarge,
    Format,
    BadSignature,
    Json(String),
    Product,
    Target,
    Channel,
    Version,
    Size,
    Hash,
    Expired,
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reject::TooLarge => write!(f, "manifest too large"),
            Reject::Format => write!(f, "manifest malformed"),
            Reject::BadSignature => write!(f, "manifest signature invalid"),
            Reject::Json(e) => write!(f, "manifest JSON invalid: {e}"),
            Reject::Product => write!(f, "manifest is for another product"),
            Reject::Target => write!(f, "manifest is for another target"),
            Reject::Channel => write!(f, "manifest is for another channel"),
            Reject::Version => write!(f, "manifest version invalid"),
            Reject::Size => write!(f, "manifest size out of range"),
            Reject::Hash => write!(f, "manifest sha256 invalid"),
            Reject::Expired => write!(f, "manifest expired"),
        }
    }
}

impl std::error::Error for Reject {}

/// Check `file` (the bytes of `latest.signed`) and return the manifest it carries.
/// The signature is checked before any JSON is parsed.
pub fn verify(file: &[u8], keys: &[[u8; 32]], now: u64, channel: &str) -> Result<Manifest, Reject> {
    if file.len() > MAX_FILE {
        return Err(Reject::TooLarge);
    }
    let nl = file
        .iter()
        .position(|&b| b == b'\n')
        .ok_or(Reject::Format)?;
    let (sig_hex, rest) = (&file[..nl], &file[nl + 1..]);
    if sig_hex.len() != 128 {
        return Err(Reject::Format);
    }
    let sig = std::str::from_utf8(sig_hex)
        .ok()
        .and_then(crate::tofu::hex_decode)
        .ok_or(Reject::Format)?;
    let mut msg = Vec::with_capacity(DOMAIN.len() + rest.len());
    msg.extend_from_slice(DOMAIN);
    msg.extend_from_slice(rest);
    let ok = keys.iter().any(|k| {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, k)
            .verify(&msg, &sig)
            .is_ok()
    });
    if !ok {
        return Err(Reject::BadSignature);
    }
    let m: Manifest = serde_json::from_slice(rest).map_err(|e| Reject::Json(e.to_string()))?;
    if m.product != PRODUCT {
        return Err(Reject::Product);
    }
    if m.target != TARGET {
        return Err(Reject::Target);
    }
    if m.channel != channel {
        return Err(Reject::Channel);
    }
    if parse_version(&m.version).is_none() {
        return Err(Reject::Version);
    }
    if m.size == 0 || m.size > MAX_EXE_SIZE {
        return Err(Reject::Size);
    }
    if m.sha256.len() != 64
        || !m
            .sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(Reject::Hash);
    }
    if now > m.expires {
        return Err(Reject::Expired);
    }
    Ok(m)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Install,
    Skip(&'static str),
}

/// Whether a verified manifest should be installed over what is running and on disk.
pub fn decide(m: &Manifest, running: &str, state: &State) -> Decision {
    let Some(v) = parse_version(&m.version) else {
        return Decision::Skip("bad version");
    };
    let running = parse_version(running).unwrap_or((0, 0, 0));
    let installed = parse_version(&state.installed).unwrap_or((0, 0, 0));
    if v <= running.max(installed) {
        return Decision::Skip("up to date");
    }
    if m.issued < state.highest_issued {
        return Decision::Skip("replayed manifest");
    }
    if state.bad_versions.iter().any(|b| b == &m.version) {
        return Decision::Skip("version blacklisted");
    }
    if state.pending.is_some() {
        // A previous update has not proven healthy yet; keep its rollback copy.
        return Decision::Skip("previous update not confirmed yet");
    }
    Decision::Install
}

/// Name of the exe for `version` on the host, under `/{channel}/`.
pub fn file_name(version: &str) -> String {
    format!("vc-client-{version}-x86_64.exe")
}

/// URL path of a file in `channel`.
pub fn url_path(channel: &str, file: &str) -> String {
    format!("/{channel}/{file}")
}

/// Build `latest.signed` bytes for `manifest_json` (release tool and tests only).
pub fn sign(key: &ring::signature::Ed25519KeyPair, manifest_json: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(DOMAIN.len() + manifest_json.len());
    msg.extend_from_slice(DOMAIN);
    msg.extend_from_slice(manifest_json);
    let sig = key.sign(&msg);
    let mut out = super::to_hex(sig.as_ref()).into_bytes();
    out.push(b'\n');
    out.extend_from_slice(manifest_json);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    pub(crate) fn test_key() -> (Ed25519KeyPair, [u8; 32]) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let pk: [u8; 32] = kp.public_key().as_ref().try_into().unwrap();
        (kp, pk)
    }

    pub(crate) fn manifest(version: &str) -> Manifest {
        Manifest {
            product: PRODUCT.into(),
            channel: "stable".into(),
            target: TARGET.into(),
            version: version.into(),
            sha256: "ab".repeat(32),
            size: 1000,
            issued: 1_000,
            expires: 2_000,
        }
    }

    fn signed(kp: &Ed25519KeyPair, m: &Manifest) -> Vec<u8> {
        sign(kp, &serde_json::to_vec(m).unwrap())
    }

    #[test]
    fn good_signature_with_each_key() {
        let (k1, p1) = test_key();
        let (k2, p2) = test_key();
        let keys = [p1, p2];
        let m = manifest("0.2.0");
        assert_eq!(
            verify(&signed(&k1, &m), &keys, 1500, "stable"),
            Ok(m.clone())
        );
        assert_eq!(
            verify(&signed(&k2, &m), &keys, 1500, "stable"),
            Ok(m.clone())
        );
        // Boundary: still valid at the expiry second.
        assert!(verify(&signed(&k1, &m), &keys, 2000, "stable").is_ok());
    }

    #[test]
    fn tampering_and_unknown_keys_rejected() {
        let (k1, p1) = test_key();
        let (k3, _) = test_key();
        let m = manifest("0.2.0");
        let good = signed(&k1, &m);
        // Flip one byte in the manifest JSON.
        let mut t = good.clone();
        let i = t.len() - 5;
        t[i] ^= 1;
        assert_eq!(verify(&t, &[p1], 1500, "stable"), Err(Reject::BadSignature));
        // Flip one hex digit of the signature (stay valid hex).
        let mut t = good.clone();
        t[0] = if t[0] == b'0' { b'1' } else { b'0' };
        assert_eq!(verify(&t, &[p1], 1500, "stable"), Err(Reject::BadSignature));
        // Unknown key.
        assert_eq!(
            verify(&signed(&k3, &m), &[p1], 1500, "stable"),
            Err(Reject::BadSignature)
        );
        // No keys at all.
        assert_eq!(
            verify(&good, &[], 1500, "stable"),
            Err(Reject::BadSignature)
        );
    }

    #[test]
    fn missing_domain_prefix_rejected() {
        let (k1, p1) = test_key();
        let json = serde_json::to_vec(&manifest("0.2.0")).unwrap();
        let sig = k1.sign(&json);
        let mut f = super::super::to_hex(sig.as_ref()).into_bytes();
        f.push(b'\n');
        f.extend_from_slice(&json);
        assert_eq!(verify(&f, &[p1], 1500, "stable"), Err(Reject::BadSignature));
    }

    #[test]
    fn field_checks() {
        let (k, p) = test_key();
        let check = |m: &Manifest, ch: &str, now: u64| verify(&signed(&k, m), &[p], now, ch);
        let base = manifest("0.2.0");
        let mut m = base.clone();
        m.product = "vc-server".into();
        assert_eq!(check(&m, "stable", 1500), Err(Reject::Product));
        let mut m = base.clone();
        m.target = "aarch64-pc-windows-msvc".into();
        assert_eq!(check(&m, "stable", 1500), Err(Reject::Target));
        assert_eq!(check(&base, "beta", 1500), Err(Reject::Channel));
        assert_eq!(check(&base, "stable", 2001), Err(Reject::Expired));
        let mut m = base.clone();
        m.size = MAX_EXE_SIZE + 1;
        assert_eq!(check(&m, "stable", 1500), Err(Reject::Size));
        let mut m = base.clone();
        m.size = 0;
        assert_eq!(check(&m, "stable", 1500), Err(Reject::Size));
        let mut m = base.clone();
        m.version = "0.2.0-beta".into();
        assert_eq!(check(&m, "stable", 1500), Err(Reject::Version));
        let mut m = base.clone();
        m.sha256 = "AB".repeat(32);
        assert_eq!(check(&m, "stable", 1500), Err(Reject::Hash));
        let mut m = base.clone();
        m.sha256 = "ab".repeat(31);
        assert_eq!(check(&m, "stable", 1500), Err(Reject::Hash));
        // Unknown fields are allowed (and signed).
        let mut v = serde_json::to_value(&base).unwrap();
        v["notes"] = "hi".into();
        let f = sign(&k, &serde_json::to_vec(&v).unwrap());
        assert_eq!(verify(&f, &[p], 1500, "stable"), Ok(base));
    }

    #[test]
    fn bad_hex_and_format_rejected() {
        let (k, p) = test_key();
        let good = signed(&k, &manifest("0.2.0"));
        let mut t = good.clone();
        t[3] = b'z';
        assert_eq!(verify(&t, &[p], 1500, "stable"), Err(Reject::Format));
        assert_eq!(verify(b"abcd", &[p], 1500, "stable"), Err(Reject::Format));
        assert_eq!(
            verify(&good[1..], &[p], 1500, "stable"),
            Err(Reject::Format)
        );
        let big = vec![b'a'; MAX_FILE + 1];
        assert_eq!(verify(&big, &[p], 1500, "stable"), Err(Reject::TooLarge));
        // Valid signature over non-JSON.
        let f = sign(&k, b"not json");
        assert!(matches!(
            verify(&f, &[p], 1500, "stable"),
            Err(Reject::Json(_))
        ));
    }

    #[test]
    fn decide_rules() {
        let st = State {
            installed: "0.1.0".into(),
            highest_issued: 1_000,
            ..State::default()
        };
        assert_eq!(decide(&manifest("0.2.0"), "0.1.0", &st), Decision::Install);
        assert_eq!(
            decide(&manifest("0.1.0"), "0.1.0", &st),
            Decision::Skip("up to date")
        );
        assert_eq!(
            decide(&manifest("0.0.9"), "0.1.0", &st),
            Decision::Skip("up to date")
        );
        // Running process is old but the disk already has 0.2.0: no re-stage loop.
        let st2 = State {
            installed: "0.2.0".into(),
            ..st.clone()
        };
        assert_eq!(
            decide(&manifest("0.2.0"), "0.1.0", &st2),
            Decision::Skip("up to date")
        );
        // Replay of an older manifest.
        let mut m = manifest("0.2.0");
        m.issued = 999;
        assert_eq!(
            decide(&m, "0.1.0", &st),
            Decision::Skip("replayed manifest")
        );
        // Blacklisted.
        let st3 = State {
            bad_versions: vec!["0.2.0".into()],
            ..st.clone()
        };
        assert_eq!(
            decide(&manifest("0.2.0"), "0.1.0", &st3),
            Decision::Skip("version blacklisted")
        );
        assert_eq!(decide(&manifest("0.2.1"), "0.1.0", &st3), Decision::Install);
        assert_eq!(file_name("0.2.0"), "vc-client-0.2.0-x86_64.exe");
        assert_eq!(url_path("stable", "latest.signed"), "/stable/latest.signed");
    }
}
