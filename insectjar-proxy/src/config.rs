use std::{net::SocketAddr, path::PathBuf};
use serde::Deserialize;
use wildmatch::WildMatch;

pub use insectjar_secret_store::config::StoreConfig;

#[derive(Debug, Deserialize)]
struct RawConfig {
    listen: SocketAddr,
    tls: TlsConfig,
    store: Option<StoreConfig>,
    store_config: Option<PathBuf>,
    #[serde(default)]
    rules: Vec<Rule>,
    proxy_auth: Option<ProxyAuthConfig>,
}

#[derive(Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub tls: TlsConfig,
    pub store: StoreConfig,
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
        let raw: RawConfig = toml::from_str(&text)?;
        let base = path.parent().unwrap_or(std::path::Path::new("."));
        let store = match (raw.store, raw.store_config) {
            (Some(s), None) => s,
            (None, Some(p)) => StoreConfig::from_file(&resolve_path(base, &p))?,
            (Some(_), Some(_)) => anyhow::bail!(
                "cannot specify both [store] and store_config in {}",
                path.display()
            ),
            (None, None) => anyhow::bail!(
                "one of [store] or store_config is required in {}",
                path.display()
            ),
        };
        Ok(Config {
            listen: raw.listen,
            tls: TlsConfig {
                ca_cert: resolve_path(base, &raw.tls.ca_cert),
                ca_key: resolve_path(base, &raw.tls.ca_key),
            },
            store,
            rules: raw.rules,
            proxy_auth: raw.proxy_auth,
        })
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

