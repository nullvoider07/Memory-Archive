// /Memory-Archive/ma-core/src/tls.rs

use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose, SanType,
};
use rustls::ServerConfig;
use time::{Duration, OffsetDateTime};
use tokio_rustls::TlsAcceptor;

use crate::config::Config;

/// Generate TLS assets if missing and return a TlsAcceptor for the IPC TCP server.
///
/// On first call (assets missing), generates:
///   - Private CA at ~/.memory-archive/ca/ca-cert.pem + ca-key.pem  (10-year validity)
///   - Server cert signed by the CA at ~/.memory-archive/ipc-cert.pem + ipc-key.pem (1-year validity)
///
/// Private keys are written chmod 600.
/// The CA cert must be distributed once to all annotator machines.
pub fn ensure_tls_assets(config: &Config) -> Result<TlsAcceptor> {
    let base_dir = PathBuf::from(&config.ipc_socket_path)
        .parent()
        .context("ipc_socket_path has no parent directory")?
        .to_path_buf();

    let ca_dir = base_dir.join("ca");
    let ca_cert_path = ca_dir.join("ca-cert.pem");
    let ca_key_path = ca_dir.join("ca-key.pem");
    let server_cert_path = base_dir.join("ipc-cert.pem");
    let server_key_path = base_dir.join("ipc-key.pem");

    if !ca_cert_path.exists()
        || !ca_key_path.exists()
        || !server_cert_path.exists()
        || !server_key_path.exists()
    {
        generate_all(
            &ca_dir,
            &ca_cert_path,
            &ca_key_path,
            &server_cert_path,
            &server_key_path,
            &config.ipc_bind_addr,
        )?;
    }

    build_tls_acceptor(&server_cert_path, &server_key_path)
}

/// Generate the private CA and a server cert signed by it in a single operation.
fn generate_all(
    ca_dir: &Path,
    ca_cert_path: &Path,
    ca_key_path: &Path,
    server_cert_path: &Path,
    server_key_path: &Path,
    bind_addr: &str,
) -> Result<()> {
    fs::create_dir_all(ca_dir).context("Failed to create CA directory")?;

    // CA cert — self-signed, valid 10 years
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::OrganizationName, "Memory Archive");
    ca_params.distinguished_name.push(DnType::CommonName, "Memory Archive Private CA");
    ca_params.not_before = OffsetDateTime::now_utc();
    ca_params.not_after = OffsetDateTime::now_utc() + Duration::days(365 * 10);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];

    let ca_cert = Certificate::from_params(ca_params)
        .context("Failed to generate CA certificate")?;

    // Server cert — signed by CA, valid 1 year
    let mut server_params = CertificateParams::default();
    server_params.is_ca = IsCa::NoCa;
    server_params.distinguished_name.push(DnType::OrganizationName, "Memory Archive");
    server_params.distinguished_name.push(DnType::CommonName, "ma-core IPC Server");
    server_params.not_before = OffsetDateTime::now_utc();
    server_params.not_after = OffsetDateTime::now_utc() + Duration::days(365);
    server_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    // SANs: always include loopback addresses
    server_params.subject_alt_names = vec![
        SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        SanType::IpAddress(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ];

    // If bind_addr is a specific IP (not a wildcard), include it as a SAN so
    // clients connecting to that IP pass cert verification when check_hostname is enabled.
    if bind_addr != "0.0.0.0" && bind_addr != "::" {
        if let Ok(ip) = bind_addr.parse::<IpAddr>() {
            server_params.subject_alt_names.push(SanType::IpAddress(ip));
        }
    }

    let server_cert = Certificate::from_params(server_params)
        .context("Failed to generate server certificate")?;

    // Serialize
    let ca_cert_pem = ca_cert.serialize_pem()
        .context("Failed to serialize CA certificate")?;
    let ca_key_pem = ca_cert.serialize_private_key_pem();

    let server_cert_pem = server_cert
        .serialize_pem_with_signer(&ca_cert)
        .context("Failed to sign server certificate with CA")?;
    let server_key_pem = server_cert.serialize_private_key_pem();

    // Write — certs are world-readable, keys are owner-only (600)
    fs::write(ca_cert_path, ca_cert_pem.as_bytes())
        .context("Failed to write CA certificate")?;
    write_secret(ca_key_path, ca_key_pem.as_bytes())?;
    fs::write(server_cert_path, server_cert_pem.as_bytes())
        .context("Failed to write server certificate")?;
    write_secret(server_key_path, server_key_pem.as_bytes())?;

    tracing::info!(
        ca_cert  = %ca_cert_path.display(),
        ipc_cert = %server_cert_path.display(),
        "TLS 1.3 assets generated"
    );
    tracing::info!(
        "Distribute the CA certificate to annotator machines, then run:\n  \
         memory-archive config --ipc-ca-cert {}\n  \
         Verify with: memory-archive tls fingerprint",
        ca_cert_path.display()
    );

    Ok(())
}

/// Load server cert + key and build a TLS 1.3-only TlsAcceptor.
fn build_tls_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor> {
    let cert_pem = fs::read(cert_path)
        .with_context(|| format!("Failed to read server cert: {}", cert_path.display()))?;
    let key_pem = fs::read(key_path)
        .with_context(|| format!("Failed to read server key: {}", key_path.display()))?;

    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_slice())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("Failed to parse server certificate PEM")?;

    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .context("Failed to read private key PEM")?
        .context("No private key found in key file")?;

    // TLS 1.3 only — no TLS 1.2 fallback
    let server_config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("Failed to build TLS server config")?;

    tracing::debug!("TLS 1.3 acceptor built from {}", cert_path.display());

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// Write a file and immediately restrict it to owner-read/write only (chmod 600).
fn write_secret(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents)
        .with_context(|| format!("Failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to restrict permissions on {}", path.display()))?;
    }

    Ok(())
}