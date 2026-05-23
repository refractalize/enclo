use std::{net::SocketAddr, path::PathBuf};
use serde::Deserialize;
use wildmatch::WildMatch;

pub use insectjar_secret_store::config::StoreConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub listen: SocketAddr,
    pub tls: TlsConfig,
    pub store: StoreConfig,
    #[serde(default)]
    pub rules: Vec<Rule>,
    pub proxy_auth: Option<ProxyAuthConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ProxyAuthConfig {
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, Deserialize)]
pub struct TlsConfig {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct Rule {
    pub domain: String,
    pub auth: AuthConfig,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    Bearer { secret: String },
    Basic { username: String, secret: String },
    Header { name: String, secret: String },
}

impl Config {
    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&text)?;
        let base = path.parent().unwrap_or(std::path::Path::new("."));
        config.tls.ca_cert = resolve_path(base, &config.tls.ca_cert);
        config.tls.ca_key = resolve_path(base, &config.tls.ca_key);
        Ok(config)
    }

    /// Returns the first matching rule for the given hostname.
    pub fn match_rule(&self, host: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| WildMatch::new(&r.domain).matches(host))
    }
}

fn resolve_path(base: &std::path::Path, path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

