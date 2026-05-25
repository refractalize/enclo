use std::{num::NonZeroUsize, sync::{Arc, Mutex}};
use lru::LruCache;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    ClientConfig, RootCertStore, ServerConfig,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const CERT_CACHE_SIZE: usize = 500;

pub struct TlsState {
    ca_cert: rcgen::Certificate,
    ca_key: KeyPair,
    cert_cache: Mutex<LruCache<String, Arc<ServerConfig>>>,
    pub connector: TlsConnector,
}

impl TlsState {
    pub fn load(ca_cert_path: &std::path::Path, ca_key_path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let ca_cert_pem = std::fs::read_to_string(ca_cert_path)?;
        let ca_key_pem = std::fs::read_to_string(ca_key_path)?;

        let ca_key = KeyPair::from_pem(&ca_key_pem)?;

        // x509-parser feature lets us reconstruct CertificateParams from an existing PEM.
        let ca_params = CertificateParams::from_ca_cert_pem(&ca_cert_pem)?;
        // self_signed re-creates the Certificate object needed for the `signed_by` API.
        let ca_cert = ca_params.self_signed(&ca_key)?;

        let mut root_store = RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs()? {
            root_store.add(cert)?;
        }
        let client_config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));

        Ok(Arc::new(Self {
            ca_cert,
            ca_key,
            cert_cache: Mutex::new(LruCache::new(NonZeroUsize::new(CERT_CACHE_SIZE).unwrap())),
            connector,
        }))
    }

    pub fn acceptor_for(&self, hostname: &str) -> anyhow::Result<TlsAcceptor> {
        if let Some(cfg) = self.cert_cache.lock().unwrap().get(hostname) {
            return Ok(TlsAcceptor::from(Arc::clone(cfg)));
        }
        let cfg = Arc::new(self.build_server_config(hostname)?);
        self.cert_cache.lock().unwrap().put(hostname.to_string(), Arc::clone(&cfg));
        Ok(TlsAcceptor::from(cfg))
    }

    fn build_server_config(&self, hostname: &str) -> anyhow::Result<ServerConfig> {
        let leaf_key = KeyPair::generate()?;

        let mut params = CertificateParams::new(vec![hostname.to_string()])?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, hostname);

        let leaf_cert_der = params.signed_by(&leaf_key, &self.ca_cert, &self.ca_key)?;

        let cert_der = CertificateDer::from(leaf_cert_der.der().to_vec());
        let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));

        let cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)?;

        Ok(cfg)
    }
}
