//! Local Certificate Authority: generate once, sign leaf certs for `*.<domain>`.
//!
//! Files live under `~/.procpane/ca/`:
//!   - `ca-cert.pem` — root certificate (installed into the system trust store
//!     by `procpane trust install`)
//!   - `ca-key.pem`  — root private key (chmod 0600, never leaves the machine)
//!
//! Leaf certs are signed on demand and held in memory by the daemon.

use anyhow::{anyhow, Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
};
use std::fs;
use std::path::{Path, PathBuf};
use time::{Duration, OffsetDateTime};

const CA_DIR_NAME: &str = ".procpane/ca";
const CA_CERT_FILE: &str = "ca-cert.pem";
const CA_KEY_FILE: &str = "ca-key.pem";
pub const CA_COMMON_NAME: &str = "procpane Local Development CA";

pub fn ca_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine $HOME"))?;
    Ok(home.join(CA_DIR_NAME))
}

pub fn ca_cert_path() -> Result<PathBuf> {
    Ok(ca_dir()?.join(CA_CERT_FILE))
}

pub fn ca_key_path() -> Result<PathBuf> {
    Ok(ca_dir()?.join(CA_KEY_FILE))
}

pub fn is_installed() -> bool {
    matches!(ca_cert_path(), Ok(p) if p.is_file())
        && matches!(ca_key_path(), Ok(p) if p.is_file())
}

/// Generate root CA cert + key, write to ca_dir(). Idempotent.
pub fn ensure_ca() -> Result<()> {
    if is_installed() {
        return Ok(());
    }
    let dir = ca_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // Lock down the dir to 0700.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dir)?.permissions();
        perms.set_mode(0o700);
        let _ = fs::set_permissions(&dir, perms);
    }

    let key = KeyPair::generate().context("generate CA keypair")?;
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, CA_COMMON_NAME);
    dn.push(DnType::OrganizationName, "procpane");
    params.distinguished_name = dn;
    // CA validity: 10 years, back-dated 1 day for clock skew tolerance.
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(365 * 10);

    let cert = params.self_signed(&key).context("self-sign CA cert")?;
    write_locked(&ca_cert_path()?, cert.pem().as_bytes())?;
    write_locked(&ca_key_path()?, key.serialize_pem().as_bytes())?;
    Ok(())
}

/// Sign a leaf cert for the given DNS names (must be a non-empty list).
/// Returns (cert PEM, key PEM).
pub fn sign_leaf(dns_names: &[String]) -> Result<(String, String)> {
    let ca_cert_pem = fs::read_to_string(ca_cert_path()?).context("read CA cert")?;
    let ca_key_pem = fs::read_to_string(ca_key_path()?).context("read CA key")?;

    let ca_key = KeyPair::from_pem(&ca_key_pem).context("parse CA key")?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca_cert_pem)
        .context("parse CA cert")?;
    let ca_cert = ca_params.self_signed(&ca_key).context("rebuild CA cert from params")?;

    let leaf_key = KeyPair::generate().context("generate leaf keypair")?;
    let mut leaf_params = CertificateParams::new(dns_names.to_vec())
        .context("build leaf params")?;
    leaf_params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "procpane leaf");
        dn
    };
    leaf_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    // Leaves are re-signed at daemon startup; keep validity short.
    // Browser cap is ~397 days. 90 days is conservative + private CA, so renewals are cheap.
    let now = OffsetDateTime::now_utc();
    leaf_params.not_before = now - Duration::days(1);
    leaf_params.not_after = now + Duration::days(90);
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .context("sign leaf")?;
    Ok((leaf.pem(), leaf_key.serialize_pem()))
}

fn write_locked(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        let _ = fs::set_permissions(path, perms);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_ca_and_leaf() {
        // Use a temp directory by overriding HOME.
        let tmp = tempdir_like();
        std::env::set_var("HOME", &tmp);
        // Generate CA.
        ensure_ca().unwrap();
        assert!(is_installed());
        // Sign a wildcard leaf.
        let (cert, key) = sign_leaf(&["web.test".to_string(), "api.test".to_string()]).unwrap();
        assert!(cert.contains("-----BEGIN CERTIFICATE-----"));
        assert!(key.contains("PRIVATE KEY"));
        // Cleanup.
        let _ = fs::remove_dir_all(&tmp);
    }

    fn tempdir_like() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("procpane-ca-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }
}
