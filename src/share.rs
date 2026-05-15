//! Peer-to-peer env sharing via magic-wormhole. Receiver allocates a code,
//! sender claims it; PAKE-derived encryption protects the payload. After the
//! sender confirms the key list, all values are sent in one JSON blob.

use anyhow::{anyhow, Context, Result};
use magic_wormhole::{AppConfig, AppID, Code, MailboxConnection, Wormhole};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::str::FromStr;

use crate::secrets;

/// Application identifier — this isolates our protocol from other magic-
/// wormhole tools. Bumping the version segment is a breaking protocol change.
const APP_ID: &str = "procpane.dev/secrets-v1";

/// We use 2-word codes (e.g. `12-circus-domino`); the nameplate makes 3.
const CODE_WORD_COUNT: usize = 2;

#[derive(Debug, Serialize, Deserialize)]
struct SecretsPayload {
    /// Free-form note about the sender (host name, repo path, etc.).
    sender: String,
    /// KEY → value.
    secrets: BTreeMap<String, String>,
}

fn app_config() -> AppConfig<()> {
    AppConfig {
        id: AppID::new(APP_ID),
        rendezvous_url: Cow::Borrowed(magic_wormhole::rendezvous::DEFAULT_RENDEZVOUS_SERVER),
        app_version: (),
    }
}

/// Receiver flow: allocate a code, print it, wait for the sender.
pub async fn receive(service: &str) -> Result<()> {
    let mailbox = MailboxConnection::create(app_config(), CODE_WORD_COUNT)
        .await
        .context("allocate wormhole code")?;
    let code = mailbox.code().clone();
    println!("Share this code with the teammate sending secrets:");
    println!();
    println!("    procpane env send {code}");
    println!();
    println!("Waiting for sender... (Ctrl-C to abort)");

    let mut wh = Wormhole::connect(mailbox).await.context("wormhole connect")?;
    let payload: SecretsPayload = wh
        .receive_json()
        .await
        .map_err(|e| anyhow!("wormhole receive: {e}"))?
        .map_err(|e| anyhow!("payload decode: {e}"))?;
    let _ = wh.close().await;

    let count = payload.secrets.len();
    println!();
    println!("✓ Received {count} keys from: {}", payload.sender);
    for (k, v) in &payload.secrets {
        secrets::set(service, k, v).with_context(|| format!("store {k}"))?;
        println!("    + {k}");
    }
    println!();
    println!("Stored in Keychain. You can `procpane up` now.");
    Ok(())
}

/// Sender flow: take a code from the receiver, confirm key list, send.
pub async fn send(service: &str, code: String, keys: Vec<String>) -> Result<()> {
    if keys.is_empty() {
        return Err(anyhow!(
            "no secrets to send; store some with `procpane env set <KEY>` first"
        ));
    }
    let mut payload = SecretsPayload {
        sender: sender_label(),
        secrets: BTreeMap::new(),
    };
    let mut missing = Vec::new();
    for k in &keys {
        match secrets::get(service, k)? {
            Some(v) => {
                payload.secrets.insert(k.clone(), v);
            }
            None => missing.push(k.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(anyhow!(
            "no value stored for: {}",
            missing.join(", ")
        ));
    }

    println!("About to send {} keys to the receiver:", payload.secrets.len());
    for k in payload.secrets.keys() {
        println!("    {k}");
    }
    println!();
    println!("Press Enter to confirm and send, or Ctrl-C to abort.");
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf).ok();

    let code = Code::from_str(&code).map_err(|e| anyhow!("invalid code: {e}"))?;
    let mailbox = MailboxConnection::connect(app_config(), code, false)
        .await
        .context("join wormhole code")?;
    let mut wh = Wormhole::connect(mailbox).await.context("wormhole connect")?;
    wh.send_json(&payload)
        .await
        .map_err(|e| anyhow!("wormhole send: {e}"))?;
    let _ = wh.close().await;

    println!("✓ Sent.");
    Ok(())
}

fn sender_label() -> String {
    let host = hostname().unwrap_or_else(|| "unknown".to_string());
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    format!("{user}@{host}")
}

fn hostname() -> Option<String> {
    use std::ffi::CStr;
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut _, buf.len()) };
    if rc != 0 {
        return None;
    }
    let c = unsafe { CStr::from_ptr(buf.as_ptr() as *const _) };
    c.to_str().ok().map(|s| s.to_string())
}
