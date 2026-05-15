//! Keychain-backed secret storage. One service namespace per repo.
//!
//! Each (service, account) pair maps to (`procpane:<canonical-repo-path>`, KEY)
//! and stores the UTF-8 secret value. macOS-only for now; Linux returns
//! "unsupported" until libsecret integration lands.

use anyhow::{anyhow, Result};
use std::path::Path;

/// Compute the per-repo service name used as the Keychain namespace.
pub fn service_name(repo_root: &Path) -> String {
    let canon = repo_root.canonicalize().unwrap_or_else(|_| repo_root.to_path_buf());
    format!("procpane:{}", canon.display())
}

#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(not(target_os = "macos"))]
pub use unsupported::*;

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };
    use std::collections::BTreeSet;

    // Magic account name for the per-service index. Listed accounts are
    // tracked here so we don't have to walk every keychain item. The index
    // is hidden from `list_accounts`.
    const INDEX_ACCOUNT: &str = "__procpane_index__";

    pub fn set(service: &str, account: &str, value: &str) -> Result<()> {
        if account == INDEX_ACCOUNT {
            return Err(anyhow!("reserved key name"));
        }
        set_generic_password(service, account, value.as_bytes())
            .map_err(|e| anyhow!("keychain set failed for {account}: {e}"))?;
        index_add(service, account)
    }

    pub fn get(service: &str, account: &str) -> Result<Option<String>> {
        if account == INDEX_ACCOUNT {
            return Err(anyhow!("reserved key name"));
        }
        match get_generic_password(service, account) {
            Ok(bytes) => Ok(Some(
                String::from_utf8(bytes)
                    .map_err(|e| anyhow!("keychain value for {account} is not utf-8: {e}"))?,
            )),
            Err(e) if e.code() == -25300 => Ok(None), // errSecItemNotFound
            Err(e) => Err(anyhow!("keychain get failed for {account}: {e}")),
        }
    }

    pub fn delete(service: &str, account: &str) -> Result<bool> {
        if account == INDEX_ACCOUNT {
            return Err(anyhow!("reserved key name"));
        }
        let removed = match delete_generic_password(service, account) {
            Ok(()) => true,
            Err(e) if e.code() == -25300 => false,
            Err(e) => return Err(anyhow!("keychain delete failed for {account}: {e}")),
        };
        if removed {
            index_remove(service, account)?;
        }
        Ok(removed)
    }

    pub fn list_accounts(service: &str) -> Result<Vec<String>> {
        let mut accts = index_read(service)?;
        // Filter to entries that still resolve — drift-tolerant.
        accts.retain(|a| matches!(get_generic_password(service, a), Ok(_)));
        accts.sort();
        accts.dedup();
        Ok(accts)
    }

    fn index_read(service: &str) -> Result<Vec<String>> {
        match get_generic_password(service, INDEX_ACCOUNT) {
            Ok(bytes) => {
                let s = String::from_utf8(bytes)
                    .map_err(|e| anyhow!("index value is not utf-8: {e}"))?;
                Ok(s.lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect())
            }
            Err(e) if e.code() == -25300 => Ok(Vec::new()),
            Err(e) => Err(anyhow!("index read failed: {e}")),
        }
    }

    fn index_write(service: &str, accounts: &[String]) -> Result<()> {
        let mut set: BTreeSet<String> = accounts.iter().cloned().collect();
        set.remove(INDEX_ACCOUNT);
        let payload = set.into_iter().collect::<Vec<_>>().join("\n");
        set_generic_password(service, INDEX_ACCOUNT, payload.as_bytes())
            .map_err(|e| anyhow!("index write failed: {e}"))
    }

    fn index_add(service: &str, account: &str) -> Result<()> {
        let mut accts = index_read(service)?;
        if !accts.contains(&account.to_string()) {
            accts.push(account.to_string());
            index_write(service, &accts)?;
        }
        Ok(())
    }

    fn index_remove(service: &str, account: &str) -> Result<()> {
        let mut accts = index_read(service)?;
        accts.retain(|a| a != account);
        index_write(service, &accts)
    }
}

#[cfg(not(target_os = "macos"))]
mod unsupported {
    use super::*;
    fn err() -> anyhow::Error {
        anyhow!("procpane env: secret storage requires macOS; Linux libsecret support is not yet implemented")
    }
    pub fn set(_service: &str, _account: &str, _value: &str) -> Result<()> {
        Err(err())
    }
    pub fn get(_service: &str, _account: &str) -> Result<Option<String>> {
        Err(err())
    }
    pub fn delete(_service: &str, _account: &str) -> Result<bool> {
        Err(err())
    }
    pub fn list_accounts(_service: &str) -> Result<Vec<String>> {
        Err(err())
    }
}
