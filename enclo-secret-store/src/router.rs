use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use wildmatch::WildMatch;
use enclo_proxy_store_common::ResponseResult;
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

    pub async fn get(&self, name: &str) -> ResponseResult {
        // Match patterns against the path component only (before '?'), but
        // pass the full name including query string to the backend.
        let path = name.split('?').next().unwrap_or(name);
        let routes = self.routes.read().await;
        for route in routes.iter() {
            if route.patterns.iter().any(|p| p.matches(path)) {
                let backend = match self.backends.get(&route.backend) {
                    Some(b) => b,
                    None => {
                        tracing::error!(secret = name, backend = route.backend.as_str(), "route references unknown backend");
                        return ResponseResult::Error {
                            error: format!("route references unknown backend '{}'", route.backend),
                        };
                    }
                };
                tracing::info!(secret = name, backend = route.backend.as_str(), "fetching secret from backend");
                let result = backend.get(name).await;
                match &result {
                    ResponseResult::NotFound => tracing::warn!(secret = name, backend = route.backend.as_str(), "backend reported secret not found"),
                    ResponseResult::Error { error } => tracing::error!(secret = name, backend = route.backend.as_str(), error, "backend failed to fetch secret"),
                    ResponseResult::Ok { .. } => {}
                }
                return result;
            }
        }
        tracing::warn!(secret = name, "no route matched secret");
        ResponseResult::NotFound
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
