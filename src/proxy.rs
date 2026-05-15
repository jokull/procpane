//! TLS reverse proxy. One listener on :8443 for the whole daemon. Each accepted
//! connection sniffs SNI, looks up the registered backend port for that
//! hostname, and bidirectionally copies bytes.
//!
//! Cert chain is a single leaf signed by the local CA covering every declared
//! hostname (and a wildcard for the parent domain when present). For the MVP
//! we sign one cert at daemon startup that names every task's hostname.

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::ca;

/// Default HTTPS port for the reverse proxy. Unprivileged.
pub const PROXY_PORT: u16 = 8443;

/// Shared, mutable registry of hostname → backend port.
/// Procs update it as they become healthy.
#[derive(Default)]
pub struct PortRegistry {
    inner: RwLock<BTreeMap<String, u16>>,
}

impl PortRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn register(&self, host: &str, port: u16) {
        self.inner.write().insert(host.to_string(), port);
    }
    pub fn lookup(&self, host: &str) -> Option<u16> {
        self.inner.read().get(host).copied()
    }
}

/// Build a rustls ServerConfig that presents a single leaf cert covering all
/// of `hostnames`. The cert is signed by the local CA on the fly.
pub fn build_tls_config(hostnames: &[String]) -> Result<Arc<ServerConfig>> {
    if !ca::is_installed() {
        return Err(anyhow!(
            "local CA not generated; run `procpane trust install` first"
        ));
    }
    let (cert_pem, key_pem) = ca::sign_leaf(hostnames).context("sign leaf for proxy")?;

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("parse leaf cert PEM: {e}"))?;
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_bytes()))
            .map_err(|e| anyhow!("parse leaf key PEM: {e}"))?
            .ok_or_else(|| anyhow!("no private key found in leaf PEM"))?;

    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow!("rustls config: {e}"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec(), b"h2".to_vec()];
    Ok(Arc::new(cfg))
}

/// Run the proxy listener forever (or until `stop_rx` fires).
pub async fn run_proxy(
    tls_cfg: Arc<ServerConfig>,
    registry: Arc<PortRegistry>,
    bind: SocketAddr,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    let acceptor = TlsAcceptor::from(tls_cfg);
    eprintln!("procpane reverse proxy listening on https://{bind}");
    loop {
        tokio::select! {
            _ = stop_rx.changed() => { if *stop_rx.borrow() { return Ok(()); } }
            accept = listener.accept() => {
                let (stream, _peer) = match accept {
                    Ok(p) => p,
                    Err(e) => { tracing::warn!(?e, "proxy accept failed"); continue; }
                };
                let acceptor = acceptor.clone();
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(acceptor, registry, stream).await {
                        tracing::debug!(?e, "proxy conn ended");
                    }
                });
            }
        }
    }
}

async fn handle_conn(
    acceptor: TlsAcceptor,
    registry: Arc<PortRegistry>,
    stream: TcpStream,
) -> Result<()> {
    let tls_stream = acceptor.accept(stream).await.context("tls handshake")?;
    let sni = tls_stream
        .get_ref()
        .1
        .server_name()
        .map(|s| s.to_string())
        .unwrap_or_default();
    if sni.is_empty() {
        return Err(anyhow!("client did not send SNI"));
    }
    let port = match registry.lookup(&sni) {
        Some(p) => p,
        None => {
            // 503-style canned response so curl/browser see something useful.
            let mut s = tls_stream;
            let body = format!("procpane: unknown host {sni}\n");
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes()).await;
            return Ok(());
        }
    };
    let backend = TcpStream::connect(("127.0.0.1", port))
        .await
        .with_context(|| format!("connect backend 127.0.0.1:{port}"))?;
    let (mut tls_r, mut tls_w) = tokio::io::split(tls_stream);
    let (mut bk_r, mut bk_w) = backend.into_split();
    let a = tokio::io::copy(&mut tls_r, &mut bk_w);
    let b = tokio::io::copy(&mut bk_r, &mut tls_w);
    let _ = tokio::join!(a, b);
    Ok(())
}

/// Allocate a free TCP port by binding to :0, reading the assigned port, and
/// dropping. Race-prone in principle; in practice fine for dev.
pub fn allocate_port() -> Result<u16> {
    use std::net::TcpListener as StdListener;
    let l = StdListener::bind("127.0.0.1:0").context("bind :0 for port allocation")?;
    Ok(l.local_addr()?.port())
}
