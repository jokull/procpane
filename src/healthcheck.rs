use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::buffer::SharedBuffer;
use crate::process::{Proc, ProcState};
use crate::sidecar::Healthcheck;

#[derive(Debug, Clone)]
pub enum HealthcheckKind {
    Tcp(u16),
    Http {
        host: String,
        path: String,
    },
    Log(regex::Regex),
    /// Wait for process to exit, treat `expected` exit code as success.
    Exit(i32),
    /// No healthcheck — task is healthy as soon as it's running.
    None,
}

impl HealthcheckKind {
    /// Pick which kind to run from the sidecar. `hostname` (when set) maps the
    /// HTTP healthcheck onto `http://<hostname>:<port>`. Without it, HTTP falls
    /// back to `127.0.0.1`. (TLS-aware variant comes with the reverse proxy.)
    pub fn from_sidecar(hc: &Healthcheck, hostname: Option<&str>) -> anyhow::Result<Self> {
        let count = [hc.tcp.is_some(), hc.http.is_some(), hc.log.is_some(), hc.exit.is_some()]
            .iter()
            .filter(|x| **x)
            .count();
        if count == 0 {
            return Ok(HealthcheckKind::None);
        }
        if count > 1 {
            anyhow::bail!("only one of tcp/http/log/exit may be set per healthcheck");
        }
        if let Some(port) = hc.tcp {
            return Ok(HealthcheckKind::Tcp(port));
        }
        if let Some(path) = &hc.http {
            let host = hostname.unwrap_or("127.0.0.1").to_string();
            return Ok(HealthcheckKind::Http {
                host,
                path: path.clone(),
            });
        }
        if let Some(pat) = &hc.log {
            let re = regex::Regex::new(pat)
                .map_err(|e| anyhow::anyhow!("invalid healthcheck.log regex: {e}"))?;
            return Ok(HealthcheckKind::Log(re));
        }
        if let Some(code) = hc.exit {
            return Ok(HealthcheckKind::Exit(code));
        }
        Ok(HealthcheckKind::None)
    }
}

/// Run a healthcheck loop until the process becomes healthy, terminates, or the
/// stop signal fires. Flips proc state to Healthy when satisfied. Returns when
/// the proc reaches a terminal state OR becomes healthy.
pub async fn run_healthcheck_loop(
    proc: Arc<Proc>,
    buffer: SharedBuffer,
    kind: HealthcheckKind,
    interval: Duration,
    probe_timeout: Duration,
    start_period: Duration,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
) {
    if start_period > Duration::ZERO {
        tokio::select! {
            _ = tokio::time::sleep(start_period) => {},
            _ = stop_rx.changed() => { if *stop_rx.borrow() { return; } }
        }
    }

    // `None` and `Exit` are special: there's no real probe.
    if let HealthcheckKind::None = kind {
        // Flip to Healthy immediately if still alive.
        let mut st = proc.state.lock();
        if matches!(*st, ProcState::Starting) {
            *st = ProcState::Healthy;
        }
        return;
    }

    // For `Exit` kind, we just wait for the process to terminate; the spawn
    // thread already sets Completed/Crashed. Translate Completed → success
    // when exit matches the expected code; otherwise mark crashed.
    if let HealthcheckKind::Exit(expected) = kind {
        loop {
            if *stop_rx.borrow() {
                return;
            }
            let st = *proc.state.lock();
            if st.is_terminal() {
                let code = *proc.exit_code.lock();
                if matches!(st, ProcState::Completed) && code == Some(expected) {
                    // Already Completed and matches expected; nothing to fix.
                    return;
                }
                if code == Some(expected) {
                    *proc.state.lock() = ProcState::Completed;
                }
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                _ = stop_rx.changed() => { if *stop_rx.borrow() { return; } }
            }
        }
    }

    // Real-probe loop (Tcp / Http / Log).
    let mut last_log_seq: u64 = 0;
    loop {
        if *stop_rx.borrow() {
            return;
        }
        let st = *proc.state.lock();
        if st.is_terminal() {
            return;
        }
        if matches!(st, ProcState::Healthy) {
            return;
        }

        let ok = match &kind {
            HealthcheckKind::Tcp(port) => probe_tcp(*port, probe_timeout).await,
            HealthcheckKind::Http { host, path } => probe_http(host, path, probe_timeout).await,
            HealthcheckKind::Log(re) => {
                let (hit, last) = probe_log(&buffer, re, last_log_seq);
                last_log_seq = last;
                hit
            }
            _ => false,
        };

        if ok {
            let mut st = proc.state.lock();
            if matches!(*st, ProcState::Starting) {
                *st = ProcState::Healthy;
            }
            return;
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = stop_rx.changed() => { if *stop_rx.borrow() { return; } }
        }
    }
}

async fn probe_tcp(port: u16, t: Duration) -> bool {
    let addr = format!("127.0.0.1:{port}");
    matches!(
        timeout(t, TcpStream::connect(&addr)).await,
        Ok(Ok(_))
    )
}

async fn probe_http(host: &str, path: &str, t: Duration) -> bool {
    // Strip a leading scheme if user wrote a full URL.
    let stripped = host
        .strip_prefix("http://")
        .or_else(|| host.strip_prefix("https://"))
        .unwrap_or(host);
    let (h, port) = match stripped.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(80)),
        None => (stripped.to_string(), 80),
    };
    let target = format!("{h}:{port}");
    let path_norm = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let req = format!(
        "GET {path_norm} HTTP/1.1\r\nHost: {h}\r\nConnection: close\r\nUser-Agent: procpane-healthcheck/1\r\n\r\n"
    );

    let res = timeout(t, async {
        let mut s = TcpStream::connect(&target).await.ok()?;
        s.write_all(req.as_bytes()).await.ok()?;
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).await.ok()?;
        Some(buf[..n].to_vec())
    })
    .await;

    match res {
        Ok(Some(bytes)) => {
            let head = String::from_utf8_lossy(&bytes);
            // Accept any 2xx.
            head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.0 2")
        }
        _ => false,
    }
}

/// Scan buffer lines >= last_seq for a regex hit. Returns (hit?, new_cursor).
fn probe_log(buffer: &SharedBuffer, re: &regex::Regex, last_seq: u64) -> (bool, u64) {
    let buf = buffer.lock();
    let (lines, next) = buf.since(last_seq);
    for l in &lines {
        if re.is_match(&l.text) {
            return (true, next);
        }
    }
    (false, next)
}
