//! `%LOCALAPPDATA%\vc-client\update\<stem>-<key>.update.json`: what is installed and what to never retry.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Version of the exe now on disk (may be newer than the running process).
    pub installed: String,
    /// Highest `issued` of any verified manifest (replay ratchet).
    pub highest_issued: u64,
    /// Versions that failed their self-test or were rolled back. Never fetched again.
    pub bad_versions: Vec<String>,
    /// A swap that has not yet proven healthy.
    pub pending: Option<Pending>,
    /// Unix seconds of the last verified check.
    pub last_check: u64,
    /// Self-tests of one version that crashed without printing anything of ours.
    pub selftest_strikes: Option<Strikes>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pending {
    pub version: String,
    /// Launches of `version` so far without a healthy confirm.
    pub boots: u32,
    /// Version that was on disk before the swap (now `<stem>.old.exe`).
    pub from: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Strikes {
    pub version: String,
    pub count: u32,
}

/// Result of reading the state file.
#[derive(Debug)]
pub enum Loaded {
    Ok(State),
    /// No state file yet.
    Missing,
    /// The file exists but could not be read right now (locked, permissions, IO).
    Busy(std::io::Error),
    /// The file was read but is not valid state JSON.
    Corrupt,
}

impl Loaded {
    /// The state to act on: a missing file is a fresh state; busy or corrupt is `None`.
    pub fn usable(self) -> Option<State> {
        match self {
            Loaded::Ok(s) => Some(s),
            Loaded::Missing => Some(State::default()),
            Loaded::Busy(_) | Loaded::Corrupt => None,
        }
    }
}

/// Read the state. Never errors; see [`Loaded`].
pub fn load(path: &Path) -> Loaded {
    match std::fs::read(path) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(s) => Loaded::Ok(s),
            Err(_) => Loaded::Corrupt,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::Missing,
        Err(e) => Loaded::Busy(e),
    }
}

pub fn save(path: &Path, s: &State) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(s)?;
    crate::profile::write_atomic(path, &text)
}

impl State {
    pub fn mark_bad(&mut self, version: &str) {
        if !self.bad_versions.iter().any(|v| v == version) {
            self.bad_versions.push(version.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_missing_busy_and_corrupt() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        assert!(matches!(load(&p), Loaded::Missing));
        assert_eq!(load(&p).usable(), Some(State::default()));
        let s = State {
            installed: "0.1.0".into(),
            highest_issued: 5,
            bad_versions: vec!["0.0.9".into()],
            pending: Some(Pending {
                version: "0.1.0".into(),
                boots: 1,
                from: "0.0.8".into(),
            }),
            last_check: 9,
            selftest_strikes: Some(Strikes {
                version: "0.2.0".into(),
                count: 2,
            }),
        };
        save(&p, &s).unwrap();
        assert_eq!(load(&p).usable(), Some(s));
        std::fs::write(&p, b"{not json").unwrap();
        assert!(matches!(load(&p), Loaded::Corrupt));
        assert_eq!(load(&p).usable(), None);
        // Unknown and missing fields are fine.
        std::fs::write(&p, br#"{"installed":"0.3.0","future":1}"#).unwrap();
        assert_eq!(load(&p).usable().unwrap().installed, "0.3.0");
        // A path that exists but cannot be read as a file is busy, not corrupt.
        let dir = d.path().join("dir.json");
        std::fs::create_dir(&dir).unwrap();
        assert!(matches!(load(&dir), Loaded::Busy(_)));
    }
}
