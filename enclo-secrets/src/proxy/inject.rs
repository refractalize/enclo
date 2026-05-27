use std::sync::Arc;
use base64::{Engine, engine::general_purpose::STANDARD};
use ::http::HeaderMap;
use enclo_backend_protocol::ResponseResult;
use crate::config::{AuthConfig, Config};
use crate::store::StoreStack;

pub async fn inject(
    host: &str,
    headers: &mut HeaderMap,
    config: &Config,
    store: &Arc<StoreStack>,
) -> Result<(), String> {
    let Some(rule) = config.proxy.match_rule(host) else {
        return Ok(());
    };

    match &rule.auth {
        AuthConfig::Bearer { secret } => {
            let value = fetch(store, secret).await?;
            headers.insert(
                ::http::header::AUTHORIZATION,
                format!("Bearer {value}").parse().map_err(|e| format!("invalid token: {e}"))?,
            );
        }
        AuthConfig::Basic { username, secret } => {
            let password = fetch(store, secret).await?;
            let encoded = STANDARD.encode(format!("{username}:{password}").as_bytes());
            headers.insert(
                ::http::header::AUTHORIZATION,
                format!("Basic {encoded}").parse().map_err(|e| format!("invalid basic auth: {e}"))?,
            );
        }
        AuthConfig::Header { name, secret } => {
            let value = fetch(store, secret).await?;
            let header_name: ::http::header::HeaderName = name
                .parse()
                .map_err(|e| format!("invalid header name '{name}': {e}"))?;
            headers.insert(
                header_name,
                value.parse().map_err(|e| format!("invalid header value: {e}"))?,
            );
        }
    }

    Ok(())
}

async fn fetch(store: &Arc<StoreStack>, secret: &str) -> Result<String, String> {
    match store.get(secret).await {
        ResponseResult::Ok { value } => Ok(value),
        ResponseResult::NotFound => Err(format!("secret '{secret}' not found")),
        ResponseResult::Error { error } => Err(format!("error fetching secret '{secret}': {error}")),
    }
}
