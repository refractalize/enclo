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
        let routes = self.routes.read().await;
        for route in routes.iter() {
            if route.patterns.iter().any(|p| p.matches(name)) {
                let backend = self.backends.get(&route.backend).ok_or_else(|| {
                    format!("route references unknown backend '{}'", route.backend)
                })?;
                return backend.get(name).await;
            }
        }
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
