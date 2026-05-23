use std::{collections::HashMap, sync::Arc};
use tokio::sync::Notify;

use crate::config::StoreConfig;
use crate::backend::BackendClient;
use crate::cache::Cache;
use crate::confirm::Confirm;
use crate::router::Router;

pub struct StoreStack {
    cache: Option<Cache>,
    confirm: Option<Confirm>,
    router: Router,
}

impl StoreStack {
    pub async fn build(
        config: &StoreConfig,
        shutdown: Arc<Notify>,
        app_name: &str,
    ) -> anyhow::Result<Self> {
        let mut backends = HashMap::new();
        for backend_cfg in &config.backends {
            let binary = format!("insectjar-secret-store-{}", backend_cfg.name);
            let client =
                BackendClient::spawn(&binary, &backend_cfg.args, &backend_cfg.env, Arc::clone(&shutdown)).await?;
            backends.insert(backend_cfg.name.clone(), client);
        }

        let router = Router::new(config, backends);
        let cache = config.cache.as_ref().map(|c| Cache::new(c.idle_ttl, c.max_ttl, c.mlock, Arc::clone(&shutdown)));
        let confirm = config
            .confirm
            .as_ref()
            .map(|c| Confirm::new(app_name, c.enabled, c.debounce_ms));

        Ok(Self { cache, confirm, router })
    }

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
        tracing::info!(secret = name, "secret requested");

        if let Some(cache) = &self.cache {
            if let Some(value) = cache.get(name).await {
                tracing::debug!(secret = name, "secret served from cache");
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
