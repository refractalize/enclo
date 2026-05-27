use std::{net::SocketAddr, path::{Path, PathBuf}};
use serde::Deserialize;
use wildmatch::WildMatch;

use crate::access::AccessConfig;
use crate::store::config::StoreConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub store: StoreConfig,

    #[serde(default)]
    pub secrets: SecretsConfig,

    #[serde(default)]
    pub proxy: ProxyConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct SecretsConfig {
    #[serde(default)]
    pub socket: SocketSecretsConfig,
    #[serde(default)]
    pub access: AccessConfig,
}

#[derive(Debug, Deserialize)]
pub struct SocketSecretsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub path: Option<PathBuf>,
}

impl Default for SocketSecretsConfig {
    fn default() -> Self {
        Self { enabled: true, path: None }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ProxyConfig {
    #[serde(default)]
    pub enabled: bool,
    pub listen: Option<SocketAddr>,
    pub tls: Option<TlsConfig>,
    pub auth: Option<ProxyAuthConfig>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl ProxyConfig {
    pub fn match_rule(&self, host: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| WildMatch::new(&r.domain).matches(host))
    }
}

#[derive(Debug, Deserialize)]
pub struct TlsConfig {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProxyAuthConfig {
    pub username: String,
    pub password_hash: String,
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
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config {}: {e}", path.display()))?;
        let mut config: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("failed to parse config {}: {e}", path.display()))?;

        let base = path.parent().unwrap_or(Path::new("."));
        config.resolve_paths(base);
        config.validate(path)?;

        Ok(config)
    }

    fn resolve_paths(&mut self, base: &Path) {
        if let Some(tls) = &mut self.proxy.tls {
            tls.ca_cert = resolve_path(base, &tls.ca_cert);
            tls.ca_key = resolve_path(base, &tls.ca_key);
        }
    }

    fn validate(&self, path: &Path) -> anyhow::Result<()> {
        if self.proxy.enabled {
            if self.proxy.listen.is_none() {
                anyhow::bail!(
                    "proxy.listen is required when proxy.enabled is true in {}",
                    path.display()
                );
            }
            if self.proxy.tls.is_none() {
                anyhow::bail!(
                    "proxy.tls is required when proxy.enabled is true in {}",
                    path.display()
                );
            }
        }

        if !self.secrets.socket.enabled && !self.proxy.enabled {
            anyhow::bail!(
                "at least one of secrets.socket or proxy must be enabled in {}",
                path.display()
            );
        }

        Ok(())
    }
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn default_true() -> bool {
    true
}
