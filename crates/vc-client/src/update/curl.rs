//! Minimal HTTPS GET for Linux through the system `curl`: the system CA store, curl's
//! proxy environment variables (`https_proxy`, `no_proxy`, ...), TLS 1.2+, no
//! redirects, status 200 only, and a hard cap on body size. No new crate.
//!
//! Transport security is defence in depth only; the manifest signature and the exe's
//! SHA-256 are the trust anchor.

use std::ffi::OsString;
use std::io::{self, Read};
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, bail};

use super::apply::Fetch;

/// Marker line curl writes to stderr after the transfer (`-w`), carrying the status.
const STATUS_MARK: &str = "vc-http-status:";
/// Keep at most this much of curl's stderr for error messages.
const MAX_STDERR: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct Curl {
    /// `https://host` (production) or `http://127.0.0.1:port` (tests).
    base: String,
    /// The only protocol curl may use (`--proto =<proto>`).
    proto: &'static str,
}

impl Curl {
    /// `https://voicechat.owl.gay`. There is deliberately no way to point it elsewhere.
    pub fn production() -> Curl {
        Curl {
            base: format!("https://{}", super::HOST),
            proto: "https",
        }
    }

    /// Plain HTTP on 127.0.0.1, for tests of the curl plumbing.
    #[cfg(test)]
    pub(crate) fn loopback(port: u16) -> Curl {
        Curl {
            base: format!("http://127.0.0.1:{port}"),
            proto: "http",
        }
    }

    /// The exact argv (after `curl`) for a GET of `path`.
    fn args(&self, path: &str, max_len: u64) -> Vec<OsString> {
        curl_args(&format!("{}{path}", self.base), max_len, self.proto)
    }
}

/// curl arguments for one GET of `url`.
///
/// - `-q` must come first: it stops curl from reading `~/.curlrc`, which could add
///   `-L`, `-k` or an output file.
/// - The status must be exactly 200, checked by the caller from the `-w` marker on
///   stderr. `--fail` is deliberately not used: it lets 3xx and 204 through.
///   `--max-redirs 0` is belt and braces, since without `-L` curl follows nothing.
/// - Stall detection, not a short total cap: `--speed-limit 1 --speed-time 60` aborts
///   a transfer below 1 B/s for 60 s, matching WinHTTP's 60 s receive timeout
///   (`http.rs`), so a large binary on a slow link still finishes. `--max-time 3600`
///   is only a backstop.
/// - `--max-filesize` refuses an over-size `Content-Length` early; the caller's
///   streaming cap is the real guard.
/// - stdout carries only the body.
pub(crate) fn curl_args(url: &str, max_len: u64, proto: &str) -> Vec<OsString> {
    [
        "-q".to_string(),
        "-sS".into(),
        "--proto".into(),
        format!("={proto}"),
        "--tlsv1.2".into(),
        "--max-redirs".into(),
        "0".into(),
        "--max-filesize".into(),
        max_len.to_string(),
        "--connect-timeout".into(),
        "15".into(),
        "--speed-limit".into(),
        "1".into(),
        "--speed-time".into(),
        "60".into(),
        "--max-time".into(),
        "3600".into(),
        "-A".into(),
        format!("vc-client-updater/{}", super::RUNNING),
        "-w".into(),
        format!("%{{stderr}}\\n{STATUS_MARK}%{{http_code}}\\n"),
        url.to_string(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

/// The HTTP status from curl's stderr marker (`000` when no response arrived).
fn parse_status(stderr: &str) -> Option<u32> {
    stderr
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(STATUS_MARK))
        .and_then(|c| c.trim().parse().ok())
}

/// curl's own error text, without the status marker.
fn curl_message(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(STATUS_MARK))
        .collect::<Vec<_>>()
        .join("; ")
}

impl Fetch for Curl {
    /// GET `path` and stream the body into `sink`.
    ///
    /// The status is only known once curl exits, so `sink` may already have received
    /// a (non-200 or partial) body when this returns `Err`; callers discard whatever
    /// they collected on `Err` (the updater deletes its download).
    fn get(
        &self,
        path: &str,
        max_len: u64,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Result<u64> {
        let mut child = match Command::new("curl")
            .args(self.args(path, max_len))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            // Shown as a failed check and retried: curl may be installed later.
            Err(e) if e.kind() == io::ErrorKind::NotFound => bail!("curl not found"),
            Err(e) => bail!("run curl: {e}"),
        };
        let mut err_pipe = child.stderr.take().expect("stderr is piped");
        let err_reader = std::thread::spawn(move || {
            // Drain everything (so curl never blocks on a full pipe), keep the start.
            let mut kept = Vec::new();
            let mut b = [0u8; 4096];
            loop {
                match err_pipe.read(&mut b) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = MAX_STDERR.saturating_sub(kept.len());
                        kept.extend_from_slice(&b[..n.min(room)]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            kept
        });

        let mut out = child.stdout.take().expect("stdout is piped");
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        let streamed: Result<()> = loop {
            match out.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    total += n as u64;
                    if total > max_len {
                        break Err(anyhow!("GET {path}: body over limit {max_len}"));
                    }
                    if let Err(e) = sink(&buf[..n]) {
                        break Err(anyhow!("GET {path}: write: {e}"));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(anyhow!("GET {path}: read: {e}")),
            }
        };
        if streamed.is_err() {
            let _ = child.kill();
        }
        drop(out);
        let status = child.wait().map_err(|e| anyhow!("GET {path}: wait: {e}"))?;
        let stderr = err_reader.join().unwrap_or_default();
        streamed?;
        let stderr = String::from_utf8_lossy(&stderr);
        match parse_status(&stderr) {
            Some(200) if status.success() => Ok(total),
            Some(code) if code != 200 && code != 0 => bail!("GET {path}: HTTP {code}"),
            _ if !status.success() => {
                bail!("GET {path}: curl {status}: {}", curl_message(&stderr))
            }
            _ => bail!("GET {path}: no HTTP status from curl"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn production_argv_is_exact() {
        let got = Curl::production().args("/stable/linux/latest.signed", 65_536);
        let want: Vec<OsString> = [
            "-q",
            "-sS",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--max-redirs",
            "0",
            "--max-filesize",
            "65536",
            "--connect-timeout",
            "15",
            "--speed-limit",
            "1",
            "--speed-time",
            "60",
            "--max-time",
            "3600",
            "-A",
            &format!("vc-client-updater/{}", super::super::RUNNING),
            "-w",
            "%{stderr}\\nvc-http-status:%{http_code}\\n",
            "https://voicechat.owl.gay/stable/linux/latest.signed",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(got, want);
        assert_eq!(got[0], "-q", "-q must be first or ~/.curlrc is read");
        assert!(!got.iter().any(|a| a == "--fail" || a == "-f" || a == "-L"));
    }

    #[test]
    fn status_marker_parsing() {
        assert_eq!(parse_status("\nvc-http-status:200\n"), Some(200));
        assert_eq!(
            parse_status("curl: (6) Could not resolve host\nvc-http-status:000\n"),
            Some(0)
        );
        assert_eq!(parse_status("nothing"), None);
        assert_eq!(
            curl_message("curl: (6) Could not resolve host\n\nvc-http-status:000\n"),
            "curl: (6) Could not resolve host"
        );
    }

    /// Serve one canned response on a loopback port; returns the port.
    fn serve(response: Vec<u8>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut req = Vec::new();
                let mut b = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&b[..n]),
                    }
                }
                let _ = s.write_all(&response);
                let _ = s.flush();
            }
        });
        port
    }

    fn get(port: u16, max: u64) -> (Result<u64>, Vec<u8>) {
        let mut body = Vec::new();
        let r = Curl::loopback(port).get("/stable/linux/latest.signed", max, &mut |c| {
            body.extend_from_slice(c);
            Ok(())
        });
        (r, body)
    }

    #[test]
    fn ok_body() {
        let port = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec(),
        );
        let (r, body) = get(port, 100);
        assert_eq!(r.unwrap(), 5);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn redirect_refused() {
        let port = serve(
            b"HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:1/evil\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        );
        let (r, _) = get(port, 100);
        let e = r.unwrap_err().to_string();
        assert!(e.contains("HTTP 301"), "{e}");
    }

    #[test]
    fn not_found_refused() {
        let port = serve(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnop".to_vec(),
        );
        let (r, _) = get(port, 100);
        let e = r.unwrap_err().to_string();
        assert!(e.contains("HTTP 404"), "{e}");
    }

    #[test]
    fn no_content_is_not_ok() {
        let port = serve(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec());
        let (r, _) = get(port, 100);
        let e = r.unwrap_err().to_string();
        assert!(e.contains("HTTP 204"), "{e}");
    }

    #[test]
    fn content_length_over_cap_refused() {
        let port = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world"
                .to_vec(),
        );
        let (r, body) = get(port, 10);
        assert!(r.is_err(), "{r:?}");
        assert!(body.len() <= 10);
    }

    #[test]
    fn chunked_over_cap_aborted() {
        let mut resp =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for _ in 0..4 {
            resp.extend_from_slice(b"8\r\n12345678\r\n");
        }
        resp.extend_from_slice(b"0\r\n\r\n");
        let (r, body) = get(serve(resp), 20);
        assert!(r.is_err(), "{r:?}");
        assert!(body.len() <= 20);
        // The same body under a larger cap is fine.
        let mut resp =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        resp.extend_from_slice(b"8\r\n12345678\r\n0\r\n\r\n");
        let (r, body) = get(serve(resp), 20);
        assert_eq!(r.unwrap(), 8);
        assert_eq!(body, b"12345678");
    }

    #[test]
    fn short_body_is_error() {
        let port = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nonly ten b"
                .to_vec(),
        );
        let (r, _) = get(port, 1000);
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn nothing_listening_is_error() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (r, _) = get(port, 100);
        assert!(r.is_err(), "{r:?}");
    }
}
