use std::collections::HashMap;
use std::time::Duration;
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    pub cache: Option<CacheConfig>,
    pub confirm: Option<ConfirmConfig>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    #[serde(with = "humantime_serde")]
    pub idle_ttl: Duration,
    #[serde(with = "humantime_serde")]
    pub max_ttl: Duration,
    #[serde(default)]
    pub mlock: bool,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct ConfirmConfig {
    pub enabled: bool,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_debounce_ms() -> u64 {
    50
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct Route {
    pub patterns: Vec<String>,
    pub backend: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct BackendConfig {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl StoreConfig {
    #[allow(dead_code)]
    pub fn backend(&self, name: &str) -> Option<&BackendConfig> {
        self.backends.iter().find(|b| b.name == name)
    }
}
