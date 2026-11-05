use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};

use anyhow::Context;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
};

pub fn generate(output_dir: &Path) -> anyhow::Result<()> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "enclo Local Proxy CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(3650);
    let cert = params.self_signed(&key)?;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    let key_path = output_dir.join("ca.key");
    let cert_path = output_dir.join("ca.crt");
    let mut key_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&key_path)
        .with_context(|| {
            format!(
                "cannot create {} (existing files are never overwritten)",
                key_path.display()
            )
        })?;
    let mut cert_file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&cert_path)
    {
        Ok(file) => file,
        Err(error) => {
            drop(key_file);
            let _ = fs::remove_file(&key_path);
            return Err(error).with_context(|| {
                format!(
                    "cannot create {} (existing files are never overwritten)",
                    cert_path.display()
                )
            });
        }
    };
    let result = (|| -> std::io::Result<()> {
        key_file.write_all(key.serialize_pem().as_bytes())?;
        cert_file.write_all(cert.pem().as_bytes())?;
        key_file.sync_all()?;
        cert_file.sync_all()
    })();
    if result.is_err() {
        drop(key_file);
        drop(cert_file);
        let _ = fs::remove_file(&key_path);
        let _ = fs::remove_file(&cert_path);
    }
    result.context("failed to write CA certificate and private key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn generated_ca_loads_and_existing_files_are_preserved() {
        let dir = std::env::temp_dir().join(format!(
            "enclo-ca-test-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        generate(&dir).unwrap();
        let key_path = dir.join("ca.key");
        let cert_path = dir.join("ca.crt");
        let key = fs::read(&key_path).unwrap();
        let cert = fs::read(&cert_path).unwrap();
        assert_eq!(
            fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let tls = crate::tls::TlsState::load(&cert_path, &key_path).unwrap();
        tls.acceptor_for("example.com").unwrap();
        let params =
            CertificateParams::from_ca_cert_pem(std::str::from_utf8(&cert).unwrap()).unwrap();
        assert!(matches!(params.is_ca, IsCa::Ca(_)));
        assert!(params.key_usages.contains(&KeyUsagePurpose::KeyCertSign));
        assert!(generate(&dir).is_err());
        assert_eq!(fs::read(&key_path).unwrap(), key);
        assert_eq!(fs::read(&cert_path).unwrap(), cert);
        fs::remove_file(&key_path).unwrap();
        assert!(generate(&dir).is_err());
        assert!(!key_path.exists());
        assert_eq!(fs::read(&cert_path).unwrap(), cert);
        fs::remove_dir_all(dir).unwrap();
    }
}
