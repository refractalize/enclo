use std::path::Path;
use serde::Deserialize;
use insectjar_secret_store::config::StoreConfig;
use crate::access::AccessConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub store: StoreConfig,
    #[serde(default)]
    pub access: AccessConfig,
}

impl Config {
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config {}: {e}", path.display()))?;
        toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("failed to parse config {}: {e}", path.display()))
    }
}
