pub mod backend;
pub mod cache;
pub mod confirm;
pub mod router;

use std::{collections::HashMap, sync::Arc};
use tokio::sync::Notify;

use crate::config::StoreConfig;
use backend::BackendClient;
use cache::Cache;
use confirm::Confirm;
use router::Router;

pub struct StoreStack {
    cache: Option<Cache>,
    confirm: Option<Confirm>,
    router: Router,
}

impl StoreStack {
    pub async fn build(config: &StoreConfig, shutdown: Arc<Notify>) -> anyhow::Result<Self> {
        let mut backends = HashMap::new();
        for backend_cfg in &config.backends {
            let binary = format!("insectjar-proxy-store-{}", backend_cfg.name);
            let client =
                BackendClient::spawn(&binary, &backend_cfg.args, Arc::clone(&shutdown)).await?;
            backends.insert(backend_cfg.name.clone(), client);
        }

        let router = Router::new(config, backends);
        let cache = config.cache.as_ref().map(|c| Cache::new(c.idle_ttl, c.max_ttl, c.mlock, Arc::clone(&shutdown)));
        let confirm = config
            .confirm
            .as_ref()
            .map(|c| Confirm::new(c.enabled, c.debounce_ms));

        Ok(Self { cache, confirm, router })
    }

    /// Reload mutable config: routes, cache TTLs, confirm settings.
    /// Backend subprocesses are not affected.
    pub async fn reload(&self, config: &StoreConfig) {
        self.router.reload(&config.routes).await;

        if let (Some(cache), Some(cache_cfg)) = (&self.cache, &config.cache) {
            cache.reload(cache_cfg.idle_ttl, cache_cfg.max_ttl).await;
        }

        if let (Some(confirm), Some(confirm_cfg)) = (&self.confirm, &config.confirm) {
            confirm.reload(confirm_cfg.enabled, confirm_cfg.debounce_ms).await;
        }
    }

    pub async fn get(&self, name: &str) -> Result<String, String> {
        if let Some(cache) = &self.cache {
            if let Some(value) = cache.get(name).await {
                return Ok(value);
            }
        }

        if let Some(confirm) = &self.confirm {
            confirm.approve(name.to_string()).await?;
        }

        let value = self.router.get(name).await?;

        if let Some(cache) = &self.cache {
            cache.insert(name, value.clone()).await;
        }

        Ok(value)
    }
}
