use std::sync::Arc;
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
            let encoded = base64_encode(format!("{username}:{password}").as_bytes());
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

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) & 0x3F] as char);
        out.push(TABLE[(n >> 12) & 0x3F] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) & 0x3F] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n & 0x3F] as char } else { '=' });
    }
    out
}
