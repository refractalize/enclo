use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::sync::{Notify, RwLock};

use enclo_proxy_store_common::ResponseResult;

use crate::backend::BackendClient;
use crate::cache::Cache;
use crate::config::{BackendConfig, CacheConfig, ConfirmConfig, StoreConfig};
use crate::confirm::Confirm;
use crate::router::Router;

const RUNTIME_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

pub struct StoreStack {
    runtime: RwLock<Arc<StoreRuntime>>,
    shutdown: Arc<Notify>,
    app_name: String,
}

struct StoreRuntime {
    router: Router,
    backend_configs: HashMap<String, BackendConfig>,
    cache: Option<Arc<Cache>>,
    cache_config: Option<CacheConfig>,
    confirm: Option<Arc<Confirm>>,
    active_requests: AtomicUsize,
    idle_notify: Notify,
}

struct RuntimeGuard {
    runtime: Arc<StoreRuntime>,
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        if self.runtime.active_requests.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.runtime.idle_notify.notify_waiters();
        }
    }
}

impl StoreRuntime {
    fn enter(self: &Arc<Self>) -> RuntimeGuard {
        self.active_requests.fetch_add(1, Ordering::AcqRel);
        RuntimeGuard {
            runtime: Arc::clone(self),
        }
    }

    async fn wait_for_idle(&self) {
        loop {
            if self.active_requests.load(Ordering::Acquire) == 0 {
                return;
            }
            self.idle_notify.notified().await;
        }
    }
}

impl StoreStack {
    pub async fn build(
        config: &StoreConfig,
        shutdown: Arc<Notify>,
        app_name: &str,
    ) -> anyhow::Result<Self> {
        let runtime = build_runtime(config, None, Arc::clone(&shutdown), app_name).await?;
        Ok(Self {
            runtime: RwLock::new(runtime),
            shutdown,
            app_name: app_name.to_string(),
        })
    }

    pub async fn reload(&self, config: &StoreConfig) -> anyhow::Result<()> {
        let old_runtime = self.runtime.read().await.clone();
        let new_runtime = build_runtime(
            config,
            Some(&old_runtime),
            Arc::clone(&self.shutdown),
            &self.app_name,
        )
        .await?;

        let retired_backends = collect_retired_backends(&old_runtime, &new_runtime);

        {
            let mut runtime = self.runtime.write().await;
            *runtime = Arc::clone(&new_runtime);
        }

        if !drain_runtime(&old_runtime, RUNTIME_DRAIN_TIMEOUT).await {
            tracing::warn!(
                timeout_secs = RUNTIME_DRAIN_TIMEOUT.as_secs(),
                "timed out draining old store runtime; forcing retired backends to stop"
            );
        }

        for backend in retired_backends {
            backend.shutdown().await;
        }

        Ok(())
    }

    pub async fn get(&self, name: &str) -> ResponseResult {
        tracing::info!(secret = name, "secret requested");

        let runtime = self.runtime.read().await.clone();
        let _guard = runtime.enter();

        if let Some(cache) = &runtime.cache {
            if let Some(value) = cache.get(name).await {
                tracing::debug!(secret = name, "secret served from cache");
                return ResponseResult::Ok { value };
            }
        }

        if let Some(confirm) = &runtime.confirm {
            if let Err(error) = confirm.approve(name.to_string()).await {
                tracing::warn!(secret = name, reason = error.as_str(), "secret access denied by confirm");
                return ResponseResult::Error { error };
            }
        }

        let result = runtime.router.get(name).await;

        if let ResponseResult::Ok { ref value } = result {
            if let Some(cache) = &runtime.cache {
                cache.insert(name, value.clone()).await;
            }
        }

        result
    }
}

async fn build_runtime(
    config: &StoreConfig,
    old_runtime: Option<&Arc<StoreRuntime>>,
    shutdown: Arc<Notify>,
    app_name: &str,
) -> anyhow::Result<Arc<StoreRuntime>> {
    let mut backend_configs = HashMap::new();
    let mut backends = HashMap::new();

    for backend_config in &config.backends {
        let backend = match old_runtime
            .and_then(|runtime| runtime.backend_configs.get(&backend_config.name))
            .filter(|existing| *existing == backend_config)
            .and_then(|_| old_runtime.and_then(|runtime| runtime.router.backend(&backend_config.name)))
        {
            Some(backend) => backend,
            None => {
                let binary = format!("enclo-secret-store-{}", backend_config.name);
                BackendClient::spawn(&binary, &backend_config.args, &backend_config.env, Arc::clone(&shutdown)).await?
            }
        };

        backend_configs.insert(backend_config.name.clone(), backend_config.clone());
        backends.insert(backend_config.name.clone(), backend);
    }

    let cache = build_cache(config.cache.as_ref(), old_runtime, Arc::clone(&shutdown)).await;
    let confirm = build_confirm(config.confirm.as_ref(), old_runtime, app_name).await;

    Ok(Arc::new(StoreRuntime {
        router: Router::new(&config.routes, backends),
        backend_configs,
        cache: cache.map(|(cache, _)| cache),
        cache_config: config.cache.clone(),
        confirm: confirm.map(|(confirm, _)| confirm),
        active_requests: AtomicUsize::new(0),
        idle_notify: Notify::new(),
    }))
}

async fn build_cache(
    config: Option<&CacheConfig>,
    old_runtime: Option<&Arc<StoreRuntime>>,
    shutdown: Arc<Notify>,
) -> Option<(Arc<Cache>, CacheConfig)> {
    let config = config?.clone();

    if let Some(runtime) = old_runtime {
        if let (Some(cache), Some(existing)) = (&runtime.cache, &runtime.cache_config) {
            if existing.mlock == config.mlock {
                cache.reload(config.idle_ttl, config.max_ttl).await;
                return Some((Arc::clone(cache), config));
            }
        }
    }

    Some((
        Arc::new(Cache::new(
            config.idle_ttl,
            config.max_ttl,
            config.mlock,
            shutdown,
        )),
        config,
    ))
}

async fn build_confirm(
    config: Option<&ConfirmConfig>,
    old_runtime: Option<&Arc<StoreRuntime>>,
    app_name: &str,
) -> Option<(Arc<Confirm>, ConfirmConfig)> {
    let config = config?.clone();

    if let Some(runtime) = old_runtime {
        if let Some(confirm) = &runtime.confirm {
            confirm.reload(config.enabled, config.debounce_ms).await;
            return Some((Arc::clone(confirm), config));
        }
    }

    Some((
        Arc::new(Confirm::new(app_name, config.enabled, config.debounce_ms)),
        config,
    ))
}

fn collect_retired_backends(
    old_runtime: &Arc<StoreRuntime>,
    new_runtime: &Arc<StoreRuntime>,
) -> Vec<Arc<BackendClient>> {
    let mut retired = Vec::new();

    for (name, backend) in &old_runtime.router_backends() {
        let keep = new_runtime
            .router
            .backend(name)
            .map(|current| Arc::ptr_eq(&current, backend))
            .unwrap_or(false);
        if !keep {
            retired.push(Arc::clone(backend));
        }
    }

    retired
}

trait RouterBackends {
    fn router_backends(&self) -> Vec<(String, Arc<BackendClient>)>;
}

impl RouterBackends for StoreRuntime {
    fn router_backends(&self) -> Vec<(String, Arc<BackendClient>)> {
        self.backend_configs
            .keys()
            .filter_map(|name| self.router.backend(name).map(|backend| (name.clone(), backend)))
            .collect()
    }
}

async fn drain_runtime(runtime: &Arc<StoreRuntime>, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, runtime.wait_for_idle()).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runtime() -> Arc<StoreRuntime> {
        Arc::new(StoreRuntime {
            router: Router::new(&[], HashMap::new()),
            backend_configs: HashMap::new(),
            cache: None,
            cache_config: None,
            confirm: None,
            active_requests: AtomicUsize::new(0),
            idle_notify: Notify::new(),
        })
    }

    #[tokio::test]
    async fn drain_runtime_returns_immediately_when_idle() {
        let runtime = test_runtime();
        assert!(drain_runtime(&runtime, Duration::from_millis(10)).await);
    }

    #[tokio::test]
    async fn drain_runtime_waits_for_in_flight_request() {
        let runtime = test_runtime();
        let guard = runtime.enter();
        let release_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(guard);
            // keep clone alive until after guard drops
            drop(release_runtime);
        });

        assert!(drain_runtime(&runtime, Duration::from_millis(100)).await);
    }

    #[tokio::test]
    async fn drain_runtime_times_out_when_request_never_finishes() {
        let runtime = test_runtime();
        let guard = runtime.enter();

        assert!(!drain_runtime(&runtime, Duration::from_millis(10)).await);

        drop(guard);
    }
}
