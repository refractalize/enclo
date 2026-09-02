use std::{collections::HashMap, time::Duration};

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    pub cache: Option<CacheConfig>,
    pub confirm: Option<ConfirmConfig>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    #[serde(with = "humantime_serde")]
    pub idle_ttl: Duration,
    #[serde(with = "humantime_serde")]
    pub max_ttl: Duration,
    #[serde(default)]
    pub mlock: bool,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfirmConfig {
    pub enabled: bool,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_debounce_ms() -> u64 {
    50
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub patterns: Vec<String>,
    pub backend: String,
    pub cache: Option<bool>,
    #[serde(default, with = "humantime_serde::option")]
    pub idle_ttl: Option<Duration>,
    #[serde(default, with = "humantime_serde::option")]
    pub max_ttl: Option<Duration>,
    pub mlock: Option<bool>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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

    pub fn route_cache(&self, route: &Route) -> anyhow::Result<Option<CacheConfig>> {
        if route.cache == Some(false) {
            if route.idle_ttl.is_some() || route.max_ttl.is_some() || route.mlock.is_some() {
                anyhow::bail!(
                    "route for backend '{}' disables caching but also sets cache options",
                    route.backend
                );
            }
            return Ok(None);
        }

        let enabled = route.cache == Some(true)
            || route.idle_ttl.is_some()
            || route.max_ttl.is_some()
            || route.mlock.is_some()
            || self.cache.is_some();
        if !enabled {
            return Ok(None);
        }

        let idle_ttl = route
            .idle_ttl
            .or_else(|| self.cache.as_ref().map(|cache| cache.idle_ttl))
            .ok_or_else(|| anyhow::anyhow!(
                "route for backend '{}' enables caching but has no idle_ttl",
                route.backend
            ))?;
        let max_ttl = route
            .max_ttl
            .or_else(|| self.cache.as_ref().map(|cache| cache.max_ttl))
            .ok_or_else(|| anyhow::anyhow!(
                "route for backend '{}' enables caching but has no max_ttl",
                route.backend
            ))?;

        if idle_ttl.is_zero() || max_ttl.is_zero() {
            anyhow::bail!(
                "cache TTLs must be greater than zero for backend '{}'",
                route.backend
            );
        }

        Ok(Some(CacheConfig {
            idle_ttl,
            max_ttl,
            mlock: route
                .mlock
                .or_else(|| self.cache.as_ref().map(|cache| cache.mlock))
                .unwrap_or(false),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(input: &str) -> StoreConfig {
        toml::from_str(input).unwrap()
    }

    #[test]
    fn route_cache_inherits_defaults_and_overrides_individual_fields() {
        let config = config(r#"
            [cache]
            idle_ttl = "10m"
            max_ttl = "1h"
            mlock = true

            [[routes]]
            patterns = ["*"]
            backend = "pass"
            max_ttl = "5m"
        "#);

        let cache = config.route_cache(&config.routes[0]).unwrap().unwrap();
        assert_eq!(cache.idle_ttl, Duration::from_secs(600));
        assert_eq!(cache.max_ttl, Duration::from_secs(300));
        assert!(cache.mlock);
    }

    #[test]
    fn route_can_enable_cache_without_defaults() {
        let config = config(r#"
            [[routes]]
            patterns = ["*"]
            backend = "pass"
            idle_ttl = "10s"
            max_ttl = "1m"
        "#);

        assert!(config.route_cache(&config.routes[0]).unwrap().is_some());
    }

    #[test]
    fn route_can_disable_default_cache() {
        let config = config(r#"
            [cache]
            idle_ttl = "10m"
            max_ttl = "1h"

            [[routes]]
            patterns = ["*"]
            backend = "pass"
            cache = false
        "#);

        assert!(config.route_cache(&config.routes[0]).unwrap().is_none());
    }

    #[test]
    fn partial_cache_without_defaults_is_rejected() {
        let config = config(r#"
            [[routes]]
            patterns = ["*"]
            backend = "pass"
            max_ttl = "1m"
        "#);

        assert!(config.route_cache(&config.routes[0]).is_err());
    }
}
