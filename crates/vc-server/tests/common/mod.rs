#![allow(dead_code)]

pub mod raw_client;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;
use vc_server::ServerConfig;

/// Test config: 127.0.0.1:0, a temp cert dir, short timeouts.
pub fn test_config(dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        cert_dir: dir.to_path_buf(),
        hello_timeout_ms: 200,
        idle_timeout_ms: 3000,
        keep_alive_ms: 500,
        ..ServerConfig::default()
    }
}

/// UDP relay between a client and a server. `stop()` makes it a black hole.
pub struct BlackholeRelay {
    pub addr: SocketAddr,
    stop: tokio::sync::watch::Sender<bool>,
}

impl BlackholeRelay {
    pub async fn start(server: SocketAddr) -> Self {
        let front = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let back = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        back.connect(server).await.unwrap();
        let addr = front.local_addr().unwrap();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let client_addr = std::sync::Arc::new(std::sync::Mutex::new(None::<SocketAddr>));
        {
            let (front, back, ca, stop_rx) = (
                front.clone(),
                back.clone(),
                client_addr.clone(),
                stop_rx.clone(),
            );
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                while let Ok((n, from)) = front.recv_from(&mut buf).await {
                    *ca.lock().unwrap() = Some(from);
                    if !*stop_rx.borrow() {
                        let _ = back.send(&buf[..n]).await;
                    }
                }
            });
        }
        {
            let (front, back, ca) = (front.clone(), back.clone(), client_addr.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = back.recv(&mut buf).await {
                    let to = *ca.lock().unwrap();
                    if let Some(to) = to
                        && !*stop_rx.borrow()
                    {
                        let _ = front.send_to(&buf[..n], to).await;
                    }
                }
            });
        }
        Self { addr, stop }
    }

    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}
