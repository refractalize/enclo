use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    #[serde(default)]
    pub groups: HashMap<String, Vec<String>>,
}

impl ClientConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config {}: {e}", path.display()))?;
        toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("failed to parse config {}: {e}", path.display()))
    }

    /// Load from `path` if given, otherwise from the default XDG location.
    /// Returns an empty config if no file exists at the default location.
    pub fn load_or_default(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            Some(p) => Self::load(p),
            None => {
                let default = default_config_path();
                if default.exists() {
                    Self::load(&default)
                } else {
                    Ok(Self::default())
                }
            }
        }
    }

    /// Expand `--group` args and positional names into a deduplicated ordered list.
    /// Explicit names come first, then group members in the order they were specified.
    /// Errors if any group name is unknown.
    pub fn resolve(&self, groups: &[String], names: &[String]) -> anyhow::Result<Vec<String>> {
        let mut result: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for name in names {
            if seen.insert(name.clone()) {
                result.push(name.clone());
            }
        }

        for group_spec in groups {
            for group_name in group_spec.split(',').map(str::trim) {
                let members = self.groups.get(group_name)
                    .ok_or_else(|| anyhow::anyhow!("unknown group '{group_name}'"))?;
                for member in members {
                    if seen.insert(member.clone()) {
                        result.push(member.clone());
                    }
                }
            }
        }

        Ok(result)
    }
}

pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("enclo").join("client.toml")
}
