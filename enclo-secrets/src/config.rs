use std::path::{Path, PathBuf};
use serde::Deserialize;
use enclo_secret_store::config::StoreConfig;
use crate::access::AccessConfig;

#[derive(Debug, Deserialize)]
struct RawConfig {
    store: Option<StoreConfig>,
    store_config: Option<PathBuf>,
    #[serde(default)]
    access: AccessConfig,
}

#[derive(Debug)]
pub struct Config {
    pub store: StoreConfig,
    pub access: AccessConfig,
}

impl Config {
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config {}: {e}", path.display()))?;
        let raw: RawConfig = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("failed to parse config {}: {e}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
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
        Ok(Config { store, access: raw.access })
    }
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}
