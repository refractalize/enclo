use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use wildmatch::WildMatch;
use crate::config::{Route, StoreConfig};
use crate::backend::BackendClient;

pub struct Router {
    routes: RwLock<Vec<CompiledRoute>>,
    backends: HashMap<String, Arc<BackendClient>>,
}

struct CompiledRoute {
    patterns: Vec<WildMatch>,
    backend: String,
}

impl Router {
    pub fn new(config: &StoreConfig, backends: HashMap<String, Arc<BackendClient>>) -> Self {
        Self {
            routes: RwLock::new(compile(&config.routes)),
            backends,
        }
    }

    pub async fn reload(&self, routes: &[Route]) {
        *self.routes.write().await = compile(routes);
    }

    pub async fn get(&self, name: &str) -> Result<String, String> {
        // Match patterns against the path component only (before '?'), but
        // pass the full name including query string to the backend.
        let path = name.split('?').next().unwrap_or(name);
        let routes = self.routes.read().await;
        for route in routes.iter() {
            if route.patterns.iter().any(|p| p.matches(path)) {
                let backend = self.backends.get(&route.backend).ok_or_else(|| {
                    format!("route references unknown backend '{}'", route.backend)
                })?;
                tracing::info!(secret = name, backend = route.backend.as_str(), "fetching secret from backend");
                let result = backend.get(name).await;
                if let Err(ref e) = result {
                    tracing::warn!(secret = name, backend = route.backend.as_str(), error = e.as_str(), "backend failed to fetch secret");
                }
                return result;
            }
        }
        tracing::warn!(secret = name, "no route matched secret");
        Err(format!("no route matched secret '{name}'"))
    }
}

fn compile(routes: &[Route]) -> Vec<CompiledRoute> {
    routes
        .iter()
        .map(|r| CompiledRoute {
            patterns: r.patterns.iter().map(|p| WildMatch::new(p)).collect(),
            backend: r.backend.clone(),
        })
        .collect()
}
