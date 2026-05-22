use std::{net::SocketAddr, path::PathBuf, time::Duration};
use serde::Deserialize;
use wildmatch::WildMatch;

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
pub struct StoreConfig {
    pub cache: Option<CacheConfig>,
    pub confirm: Option<ConfirmConfig>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Deserialize)]
pub struct CacheConfig {
    #[serde(with = "humantime_serde")]
    pub idle_ttl: Duration,
    #[serde(with = "humantime_serde")]
    pub max_ttl: Duration,
    #[serde(default)]
    pub mlock: bool,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmConfig {
    pub enabled: bool,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_debounce_ms() -> u64 {
    50
}

#[derive(Debug, Deserialize)]
pub struct Route {
    pub patterns: Vec<String>,
    pub backend: String,
}

#[derive(Debug, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
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
        let config: Config = toml::from_str(&text)?;
        Ok(config)
    }

    /// Returns the first matching rule for the given hostname.
    pub fn match_rule(&self, host: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| WildMatch::new(&r.domain).matches(host))
    }
}

impl StoreConfig {
    #[allow(dead_code)]
    pub fn route(&self, secret_name: &str) -> Option<&str> {
        for route in &self.routes {
            if route.patterns.iter().any(|p| WildMatch::new(p).matches(secret_name)) {
                return Some(&route.backend);
            }
        }
        None
    }

    #[allow(dead_code)]
    pub fn backend(&self, name: &str) -> Option<&BackendConfig> {
        self.backends.iter().find(|b| b.name == name)
    }
}
