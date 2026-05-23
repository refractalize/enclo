use std::sync::Arc;
use base64::{Engine, engine::general_purpose::STANDARD};
use ::http::HeaderMap;
use crate::{config::AuthConfig, store::StoreStack};

pub async fn inject(
    host: &str,
    headers: &mut HeaderMap,
    config: &crate::config::Config,
    store: &Arc<StoreStack>,
) -> Result<(), String> {
    let Some(rule) = config.match_rule(host) else {
        return Ok(());
    };

    match &rule.auth {
        AuthConfig::Bearer { secret } => {
            let value = store.get(secret).await?;
            headers.insert(
                ::http::header::AUTHORIZATION,
                format!("Bearer {value}").parse().map_err(|e| format!("invalid token: {e}"))?,
            );
        }
        AuthConfig::Basic { username, secret } => {
            let password = store.get(secret).await?;
            let encoded = STANDARD.encode(format!("{username}:{password}").as_bytes());
            headers.insert(
                ::http::header::AUTHORIZATION,
                format!("Basic {encoded}").parse().map_err(|e| format!("invalid basic auth: {e}"))?,
            );
        }
        AuthConfig::Header { name, secret } => {
            let value = store.get(secret).await?;
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
