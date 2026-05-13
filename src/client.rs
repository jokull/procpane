use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::proto::{Request, Response};

pub async fn call(socket: &Path, req: Request) -> Result<Response> {
    if !socket.exists() {
        return Err(anyhow!(
            "no procpane daemon running here. Start one with `procpane run <task>`."
        ));
    }
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| "daemon socket exists but connection failed (stale?)")?;
    let (rd, mut wr) = stream.into_split();
    let mut payload = serde_json::to_vec(&req)?;
    payload.push(b'\n');
    wr.write_all(&payload).await?;
    wr.flush().await?;
    drop(wr);
    let mut reader = BufReader::new(rd);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if line.is_empty() {
        return Err(anyhow!("empty response"));
    }
    let resp: Response = serde_json::from_str(line.trim())?;
    Ok(resp)
}

pub async fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if socket.exists() {
            // Try ping.
            if let Ok(Response::Pong) = call(socket, Request::Ping).await {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("daemon did not come up within {:?}", timeout));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
