//! Minimal HTTPS GET over WinHTTP: the OS root store, the system proxy, TLS 1.2+,
//! no redirects, status 200 only, and a hard cap on body size.
//!
//! Transport security is defence in depth only; the manifest signature is the trust
//! anchor, so certificate revocation is not checked (it fails hard offline).

use std::ffi::c_void;

use anyhow::{Result, anyhow, bail};
use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR, w};

use super::apply::Fetch;

/// Where requests go. Production is fixed; only tests can build another endpoint.
#[derive(Debug, Clone)]
struct Endpoint {
    host: String,
    port: u16,
    secure: bool,
}

#[derive(Debug, Clone)]
pub struct WinHttp {
    ep: Endpoint,
}

impl WinHttp {
    /// `https://voicechat.owl.gay`. There is deliberately no way to point it elsewhere.
    pub fn production() -> WinHttp {
        WinHttp {
            ep: Endpoint {
                host: super::HOST.to_string(),
                port: 443,
                secure: true,
            },
        }
    }

    /// Plain HTTP on 127.0.0.1, for tests of the WinHTTP plumbing.
    #[cfg(test)]
    pub(crate) fn loopback(port: u16) -> WinHttp {
        WinHttp {
            ep: Endpoint {
                host: "127.0.0.1".to_string(),
                port,
                secure: false,
            },
        }
    }
}

/// Owned `HINTERNET`, closed on drop.
struct Handle(*mut c_void);

impl Handle {
    fn new(h: *mut c_void, what: &str) -> Result<Handle> {
        if h.is_null() {
            Err(anyhow!("{what}: {}", std::io::Error::last_os_error()))
        } else {
            Ok(Handle(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: self.0 is a live handle returned by WinHttpOpen/Connect/OpenRequest,
        // closed exactly once here.
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

fn set_u32(h: &Handle, option: u32, value: u32) -> windows::core::Result<()> {
    let b = value.to_ne_bytes();
    // SAFETY: valid handle; the buffer is a DWORD as these options require.
    unsafe { WinHttpSetOption(Some(h.0 as *const c_void), option, Some(&b)) }
}

impl Fetch for WinHttp {
    fn get(
        &self,
        path: &str,
        max_len: u64,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<u64> {
        let agent = HSTRING::from(format!("vc-client-updater/{}", super::RUNNING));
        // SAFETY: all strings outlive the calls; null proxy strings with AUTOMATIC_PROXY.
        let session = Handle::new(
            unsafe {
                WinHttpOpen(
                    &agent,
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    0,
                )
            },
            "WinHttpOpen",
        )?;
        let tls = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
        if set_u32(&session, WINHTTP_OPTION_SECURE_PROTOCOLS, tls).is_err() {
            // Windows builds without TLS 1.3 in WinHTTP refuse the combined mask.
            set_u32(
                &session,
                WINHTTP_OPTION_SECURE_PROTOCOLS,
                WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2,
            )
            .map_err(|e| anyhow!("set TLS protocols: {e}"))?;
        }
        set_u32(
            &session,
            WINHTTP_OPTION_REDIRECT_POLICY,
            WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        )
        .map_err(|e| anyhow!("set redirect policy: {e}"))?;
        // SAFETY: valid session handle.
        unsafe { WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 60_000) }
            .map_err(|e| anyhow!("set timeouts: {e}"))?;

        let host = HSTRING::from(self.ep.host.as_str());
        // SAFETY: valid session handle, host string outlives the call.
        let connect = Handle::new(
            unsafe { WinHttpConnect(session.0, &host, self.ep.port, 0) },
            "WinHttpConnect",
        )?;
        let object = HSTRING::from(path);
        let flags = if self.ep.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        // SAFETY: valid connect handle; null version/referrer/accept-types mean defaults.
        let request = Handle::new(
            unsafe {
                WinHttpOpenRequest(
                    connect.0,
                    w!("GET"),
                    &object,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    std::ptr::null(),
                    flags,
                )
            },
            "WinHttpOpenRequest",
        )?;
        // SAFETY: valid request handle, no extra headers or body.
        unsafe { WinHttpSendRequest(request.0, None, None, 0, 0, 0) }
            .map_err(|e| anyhow!("GET {path}: {e}"))?;
        // SAFETY: valid request handle; reserved must be null.
        unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }
            .map_err(|e| anyhow!("GET {path}: {e}"))?;

        let mut status: u32 = 0;
        let mut len = 4u32;
        // SAFETY: buffer is a u32 of the length passed.
        unsafe {
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut c_void),
                &mut len,
                std::ptr::null_mut(),
            )
        }
        .map_err(|e| anyhow!("GET {path}: status: {e}"))?;
        if status != 200 {
            bail!("GET {path}: HTTP {status}");
        }

        let mut clen: u64 = 0;
        let mut len = 8u32;
        // SAFETY: buffer is a u64 of the length passed.
        let has_len = unsafe {
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER64,
                PCWSTR::null(),
                Some(&mut clen as *mut u64 as *mut c_void),
                &mut len,
                std::ptr::null_mut(),
            )
        }
        .is_ok();
        if has_len && clen > max_len {
            bail!("GET {path}: Content-Length {clen} over limit {max_len}");
        }

        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let mut n: u32 = 0;
            // SAFETY: buf is valid for buf.len() bytes; n receives the count read.
            unsafe {
                WinHttpReadData(
                    request.0,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    &mut n,
                )
            }
            .map_err(|e| anyhow!("GET {path}: read: {e}"))?;
            if n == 0 {
                break;
            }
            total += u64::from(n);
            if total > max_len {
                bail!("GET {path}: body over limit {max_len}");
            }
            sink(&buf[..n as usize]).map_err(|e| anyhow!("GET {path}: write: {e}"))?;
        }
        if has_len && total != clen {
            bail!("GET {path}: body {total} bytes, Content-Length {clen}");
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

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
        let r = WinHttp::loopback(port).get("/stable/latest.signed", max, &mut |c| {
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
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/evil\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        );
        let (r, body) = get(port, 100);
        assert!(r.unwrap_err().to_string().contains("HTTP 302"));
        assert!(body.is_empty());
    }

    #[test]
    fn not_found_refused() {
        let port = serve(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnop".to_vec(),
        );
        let (r, body) = get(port, 100);
        assert!(r.unwrap_err().to_string().contains("HTTP 404"));
        assert!(body.is_empty());
    }

    #[test]
    fn content_length_over_cap_refused() {
        let port = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world"
                .to_vec(),
        );
        let (r, body) = get(port, 10);
        assert!(r.unwrap_err().to_string().contains("Content-Length"));
        assert!(body.is_empty());
    }

    #[test]
    fn chunked_over_cap_aborted() {
        let mut resp =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for _ in 0..4 {
            resp.extend_from_slice(b"8\r\n12345678\r\n");
        }
        resp.extend_from_slice(b"0\r\n\r\n");
        let port = serve(resp);
        let (r, body) = get(port, 20);
        assert!(r.unwrap_err().to_string().contains("over limit"));
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
}
