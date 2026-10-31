//! TLS certificate generation and management

use anyhow::{anyhow, Context, Result};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{config_dir, default_config_dir};

/// Generate a self-signed certificate and write PEM files
pub fn generate_self_signed_cert(cert_path: &Path, key_path: &Path) -> Result<()> {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "macrdp");
    dn.push(rcgen::DnType::OrganizationName, "macrdp");
    params.distinguished_name = dn;

    params.not_after = rcgen::date_time_ymd(2027, 3, 24);

    params.subject_alt_names = vec![
        rcgen::SanType::DnsName("localhost".try_into()?),
        rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    ];

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(cert_path, cert.pem())?;
    fs::write(key_path, key_pair.serialize_pem())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))?;
    }

    tracing::info!(?cert_path, ?key_path, "Generated self-signed TLS certificate");
    Ok(())
}

/// Ensure TLS cert and key files exist. Generate if missing.
///
/// With explicit paths, those paths are used (and must be writable). Otherwise
/// an existing pair is reused from any location macrdp previously wrote to,
/// and a new pair is generated in the first writable directory. The working
/// directory is deliberately not a write target: apps launched from Finder
/// have cwd `/`, which is read-only.
pub fn ensure_tls_files(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
) -> Result<(PathBuf, PathBuf)> {
    if cert_path.is_none() && key_path.is_none() {
        return ensure_tls_files_from(&tls_reuse_candidates(), &tls_write_candidates());
    }

    let fallback_dir = config_dir().join("tls");
    let cert = cert_path
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_dir.join("cert.pem"));
    let key = key_path
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_dir.join("key.pem"));

    if !cert.exists() || !key.exists() {
        generate_self_signed_cert(&cert, &key).with_context(|| {
            format!(
                "Failed to generate self-signed certificate at {}",
                cert.display()
            )
        })?;
    }

    Ok((cert, key))
}

/// Directories where a previously generated cert pair may exist, newest first.
fn tls_reuse_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Older builds wrote next to the config or the working directory.
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("tls"));
    }
    dirs.push(config_dir().join("tls"));
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".config").join("macrdp").join("tls"));
    }
    dirs.push(default_config_dir().join("tls"));
    dirs.push(std::env::temp_dir().join("macrdp-tls"));
    dedup_paths(dirs)
}

/// Directories to generate a new cert pair in, most preferred first.
fn tls_write_candidates() -> Vec<PathBuf> {
    dedup_paths(vec![
        config_dir().join("tls"),
        default_config_dir().join("tls"),
        std::env::temp_dir().join("macrdp-tls"),
    ])
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

fn ensure_tls_files_from(
    reuse_candidates: &[PathBuf],
    write_candidates: &[PathBuf],
) -> Result<(PathBuf, PathBuf)> {
    for dir in reuse_candidates {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        if cert.exists() && key.exists() {
            return Ok((cert, key));
        }
    }

    let mut last_error = None;
    for dir in write_candidates {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        match generate_self_signed_cert(&cert, &key) {
            Ok(()) => return Ok((cert, key)),
            Err(error) => {
                // A read-only location must not stop the server; try the next one.
                tracing::warn!(
                    dir = %dir.display(),
                    %error,
                    "Could not write TLS certificate here, trying next location"
                );
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no TLS directory candidates")))
        .with_context(|| {
            format!(
                "Failed to generate self-signed certificate (tried {write_candidates:?})"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_generate_self_signed_cert() {
        let dir = TempDir::new().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        generate_self_signed_cert(&cert_path, &key_path).unwrap();

        assert!(cert_path.exists());
        assert!(key_path.exists());

        let cert_content = fs::read_to_string(&cert_path).unwrap();
        assert!(cert_content.contains("BEGIN CERTIFICATE"));

        let key_content = fs::read_to_string(&key_path).unwrap();
        assert!(key_content.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn test_ensure_tls_files_creates_when_missing() {
        let dir = TempDir::new().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        let (c, k) = ensure_tls_files(Some(&cert_path), Some(&key_path)).unwrap();
        assert_eq!(c, cert_path);
        assert_eq!(k, key_path);
        assert!(cert_path.exists());
    }

    #[test]
    fn test_default_tls_files_skip_unwritable_location() {
        let dir = TempDir::new().unwrap();
        let writable = dir.path().join("writable");
        // A regular file where the TLS directory should be: create_dir_all fails.
        let blocked = dir.path().join("blocked");
        fs::write(&blocked, b"").unwrap();

        let (cert, key) = ensure_tls_files_from(
            &[blocked.join("tls")],
            &[blocked.join("tls"), writable.clone()],
        )
        .unwrap();

        assert_eq!(cert, writable.join("cert.pem"));
        assert_eq!(key, writable.join("key.pem"));
        assert!(cert.exists());
        assert!(key.exists());
    }

    #[test]
    fn test_default_tls_files_reuse_existing_pair() {
        let dir = TempDir::new().unwrap();
        let existing = dir.path().join("existing");
        let fresh = dir.path().join("fresh");
        generate_self_signed_cert(&existing.join("cert.pem"), &existing.join("key.pem")).unwrap();

        let (cert, key) =
            ensure_tls_files_from(&[existing.clone()], &[fresh.clone()]).unwrap();

        assert_eq!(cert, existing.join("cert.pem"));
        assert_eq!(key, existing.join("key.pem"));
        assert!(!fresh.exists());
    }
}
