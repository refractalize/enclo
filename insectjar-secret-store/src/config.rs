use std::time::Duration;
use serde::Deserialize;
use wildmatch::WildMatch;

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

impl StoreConfig {
    pub fn route(&self, secret_name: &str) -> Option<&str> {
        for route in &self.routes {
            if route.patterns.iter().any(|p| WildMatch::new(p).matches(secret_name)) {
                return Some(&route.backend);
            }
        }
        None
    }

    pub fn backend(&self, name: &str) -> Option<&BackendConfig> {
        self.backends.iter().find(|b| b.name == name)
    }
}
