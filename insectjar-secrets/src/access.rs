use serde::Deserialize;
use wildmatch::WildMatch;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum AccessMode {
    AllowAll,
    DenyAll,
}

impl Default for AccessMode {
    fn default() -> Self {
        Self::DenyAll
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct AccessConfig {
    #[serde(default)]
    pub mode: AccessMode,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

pub struct AccessPolicy {
    mode: AccessMode,
    allow: Vec<WildMatch>,
    deny: Vec<WildMatch>,
}

impl AccessPolicy {
    pub fn new(config: &AccessConfig) -> Self {
        Self {
            mode: config.mode.clone(),
            allow: config.allow.iter().map(|p| WildMatch::new(p)).collect(),
            deny: config.deny.iter().map(|p| WildMatch::new(p)).collect(),
        }
    }

    pub fn is_permitted(&self, name: &str) -> bool {
        let path = name.split('?').next().unwrap_or(name);
        match self.mode {
            AccessMode::DenyAll => self.allow.iter().any(|p| p.matches(path))
                && !self.deny.iter().any(|p| p.matches(path)),
            AccessMode::AllowAll => !self.deny.iter().any(|p| p.matches(path)),
        }
    }
}
