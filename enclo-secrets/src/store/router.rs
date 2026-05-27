use std::{collections::HashMap, sync::Arc};

use enclo_backend_protocol::ResponseResult;
use wildmatch::WildMatch;

use crate::store::backend::BackendClient;
use crate::store::config::Route;

pub struct Router {
    routes: Vec<CompiledRoute>,
    backends: HashMap<String, Arc<BackendClient>>,
}

struct CompiledRoute {
    patterns: Vec<WildMatch>,
    backend: String,
}

impl Router {
    pub fn new(routes: &[Route], backends: HashMap<String, Arc<BackendClient>>) -> Self {
        Self {
            routes: compile(routes),
            backends,
        }
    }

    pub fn backend(&self, name: &str) -> Option<Arc<BackendClient>> {
        self.backends.get(name).map(Arc::clone)
    }

    pub async fn get(&self, name: &str) -> ResponseResult {
        let path = name.split('?').next().unwrap_or(name);
        for route in &self.routes {
            if route.patterns.iter().any(|p| p.matches(path)) {
                let backend = match self.backends.get(&route.backend) {
                    Some(backend) => backend,
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
        .map(|route| CompiledRoute {
            patterns: route.patterns.iter().map(|pattern| WildMatch::new(pattern)).collect(),
            backend: route.backend.clone(),
        })
        .collect()
}
