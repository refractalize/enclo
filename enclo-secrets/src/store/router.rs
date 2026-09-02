use std::{collections::HashMap, sync::Arc};

use wildmatch::WildMatch;

use crate::store::backend::BackendClient;
use crate::store::cache::Cache;
use crate::store::config::Route;

pub struct Router {
    routes: Vec<CompiledRoute>,
    backends: HashMap<String, Arc<BackendClient>>,
}

pub struct CompiledRoute {
    patterns: Vec<CompiledPattern>,
    backend: String,
    cache: Option<Arc<Cache>>,
}

struct CompiledPattern {
    secret: WildMatch,
    parameters: Vec<ParameterPattern>,
}

struct ParameterPattern {
    name: String,
    value: WildMatch,
}

pub struct ResolvedRoute {
    pub backend_name: String,
    pub backend: Arc<BackendClient>,
    pub cache: Option<Arc<Cache>>,
}

impl Router {
    pub fn new(
        routes: Vec<CompiledRoute>,
        backends: HashMap<String, Arc<BackendClient>>,
    ) -> Self {
        Self { routes, backends }
    }

    pub fn backend(&self, name: &str) -> Option<Arc<BackendClient>> {
        self.backends.get(name).map(Arc::clone)
    }

    pub fn resolve(&self, name: &str) -> Result<Option<ResolvedRoute>, String> {
        let (secret, query) = name.split_once('?').unwrap_or((name, ""));
        let mut parameters = None;

        for route in &self.routes {
            let mut matched = false;
            for pattern in &route.patterns {
                if !pattern.secret.matches(secret) {
                    continue;
                }
                if pattern.parameters.is_empty() {
                    matched = true;
                    break;
                }
                let request_parameters = match &parameters {
                    Some(parameters) => parameters,
                    None => parameters.insert(parse_request_parameters(query)?),
                };
                if pattern.parameters.iter().all(|constraint| {
                    request_parameters.iter().any(|(name, value)| {
                        name == &constraint.name && constraint.value.matches(value)
                    })
                }) {
                    matched = true;
                    break;
                }
            }

            if matched {
                let backend = match self.backends.get(&route.backend) {
                    Some(backend) => Arc::clone(backend),
                    None => {
                        tracing::error!(secret = name, backend = route.backend.as_str(), "route references unknown backend");
                        return Err(format!("route references unknown backend '{}'", route.backend));
                    }
                };
                return Ok(Some(ResolvedRoute {
                    backend_name: route.backend.clone(),
                    backend,
                    cache: route.cache.as_ref().map(Arc::clone),
                }));
            }
        }

        tracing::warn!(secret = name, "no route matched secret");
        Ok(None)
    }
}

pub fn compile(
    routes: &[Route],
    route_caches: Vec<Option<Arc<Cache>>>,
) -> anyhow::Result<Vec<CompiledRoute>> {
    if routes.len() != route_caches.len() {
        anyhow::bail!("route cache count does not match route count");
    }

    routes
        .iter()
        .zip(route_caches)
        .map(|(route, cache)| {
            let patterns = route
                .patterns
                .iter()
                .map(|pattern| compile_pattern(pattern))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(CompiledRoute {
                patterns,
                backend: route.backend.clone(),
                cache,
            })
        })
        .collect()
}

fn compile_pattern(pattern: &str) -> anyhow::Result<CompiledPattern> {
    let (secret, query) = pattern.split_once('?').unwrap_or((pattern, ""));
    if secret.is_empty() {
        anyhow::bail!("route pattern '{pattern}' has an empty secret name");
    }

    let parameters = if pattern.contains('?') {
        if query.is_empty() {
            anyhow::bail!("route pattern '{pattern}' has an empty query string");
        }
        parse_pattern_parameters(pattern, query)?
    } else {
        Vec::new()
    };

    Ok(CompiledPattern {
        secret: WildMatch::new(secret),
        parameters,
    })
}

fn parse_pattern_parameters(
    pattern: &str,
    query: &str,
) -> anyhow::Result<Vec<ParameterPattern>> {
    query
        .split('&')
        .map(|pair| {
            let (name, value) = pair.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("route pattern '{pattern}' has parameter '{pair}' without a value")
            })?;
            if name.is_empty() {
                anyhow::bail!("route pattern '{pattern}' has an empty parameter name");
            }
            let name = decode(name).map_err(|error| {
                anyhow::anyhow!("route pattern '{pattern}' has an invalid parameter name: {error}")
            })?;
            let value = decode(value).map_err(|error| {
                anyhow::anyhow!("route pattern '{pattern}' has an invalid parameter value: {error}")
            })?;
            Ok(ParameterPattern {
                name,
                value: WildMatch::new(&value),
            })
        })
        .collect()
}

fn parse_request_parameters(query: &str) -> Result<Vec<(String, String)>, String> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    query
        .split('&')
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let name = decode(name)
                .map_err(|error| format!("invalid query parameter name '{name}': {error}"))?;
            let value = decode(value)
                .map_err(|error| format!("invalid query parameter value for '{name}': {error}"))?;
            Ok((name, value))
        })
        .collect()
}

fn decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(format!("invalid percent encoding in '{value}'"));
            }
            index += 3;
        } else {
            index += 1;
        }
    }

    urlencoding::decode(value)
        .map(|value| value.into_owned())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, request: &str) -> bool {
        let pattern = compile_pattern(pattern).unwrap();
        let (secret, query) = request.split_once('?').unwrap_or((request, ""));
        pattern.secret.matches(secret)
            && (pattern.parameters.is_empty()
                || {
                    let parameters = parse_request_parameters(query).unwrap();
                    pattern.parameters.iter().all(|constraint| {
                        parameters.iter().any(|(name, value)| {
                            name == &constraint.name && constraint.value.matches(value)
                        })
                    })
                })
    }

    #[test]
    fn parameterless_pattern_matches_any_query() {
        assert!(matches("AWS_CREDENTIALS", "AWS_CREDENTIALS?profile=production-eu"));
    }

    #[test]
    fn exact_and_globbed_parameter_values_match() {
        assert!(matches("AWS_CREDENTIALS?profile=x", "AWS_CREDENTIALS?profile=x"));
        assert!(matches(
            "AWS_CREDENTIALS?profile=production-*",
            "AWS_CREDENTIALS?profile=production-eu"
        ));
        assert!(!matches(
            "AWS_CREDENTIALS?profile=production-*",
            "AWS_CREDENTIALS?profile=staging"
        ));
    }

    #[test]
    fn secret_name_and_parameter_globs_are_combined() {
        assert!(matches(
            "AWS_*?profile=production-*",
            "AWS_CREDENTIALS?profile=production-eu"
        ));
        assert!(!matches(
            "GITHUB_*?profile=production-*",
            "AWS_CREDENTIALS?profile=production-eu"
        ));
    }

    #[test]
    fn parameter_order_and_extra_parameters_do_not_matter() {
        assert!(matches(
            "AWS_CREDENTIALS?profile=production-*&role=readonly",
            "AWS_CREDENTIALS?region=eu&role=readonly&profile=production-eu"
        ));
    }

    #[test]
    fn every_constraint_is_required() {
        assert!(!matches(
            "AWS_CREDENTIALS?profile=production-*&role=readonly",
            "AWS_CREDENTIALS?profile=production-eu"
        ));
    }

    #[test]
    fn decoded_and_repeated_parameter_values_are_matched() {
        assert!(matches(
            "AWS_CREDENTIALS?profile=production-*",
            "AWS_CREDENTIALS?profile=staging&profile=production%2Deu"
        ));
    }

    #[test]
    fn malformed_patterns_are_rejected() {
        assert!(compile_pattern("AWS_CREDENTIALS?").is_err());
        assert!(compile_pattern("AWS_CREDENTIALS?profile").is_err());
        assert!(compile_pattern("AWS_CREDENTIALS?=production").is_err());
        assert!(compile_pattern("AWS_CREDENTIALS?profile=%GG").is_err());
    }
}
