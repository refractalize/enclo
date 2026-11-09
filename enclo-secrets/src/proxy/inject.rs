use crate::config::{AuthConfig, Config};
use crate::store::StoreStack;
use ::http::HeaderMap;
use base64::{engine::general_purpose::STANDARD, Engine};
use enclo_backend_protocol::ResponseResult;
use std::sync::Arc;

pub async fn inject(
    host: &str,
    path: &str,
    tls: bool,
    method: &::http::Method,
    headers: &mut HeaderMap,
    config: &Config,
    store: &Arc<StoreStack>,
) -> Result<(), String> {
    let Some(rule) = config.proxy.match_rule(host) else {
        return Ok(());
    };

    match &rule.auth {
        AuthConfig::Codex { secret } => {
            // Never attach a ChatGPT login to plaintext HTTP, another hostname,
            // or unrelated ChatGPT endpoints, even if a broad rule is configured.
            if !codex_destination(host, path, tls, method) {
                return Ok(());
            }
            let value = fetch(store, secret).await?;
            inject_codex(headers, &value)?;
        }
        AuthConfig::Bearer { secret } => {
            let value = fetch(store, secret).await?;
            headers.insert(
                ::http::header::AUTHORIZATION,
                format!("Bearer {value}")
                    .parse()
                    .map_err(|e| format!("invalid token: {e}"))?,
            );
        }
        AuthConfig::Basic { username, secret } => {
            let password = fetch(store, secret).await?;
            let encoded = STANDARD.encode(format!("{username}:{password}").as_bytes());
            headers.insert(
                ::http::header::AUTHORIZATION,
                format!("Basic {encoded}")
                    .parse()
                    .map_err(|e| format!("invalid basic auth: {e}"))?,
            );
        }
        AuthConfig::Header { name, secret } => {
            let value = fetch(store, secret).await?;
            let header_name: ::http::header::HeaderName = name
                .parse()
                .map_err(|e| format!("invalid header name '{name}': {e}"))?;
            headers.insert(
                header_name,
                value
                    .parse()
                    .map_err(|e| format!("invalid header value: {e}"))?,
            );
        }
    }

    Ok(())
}

async fn fetch(store: &Arc<StoreStack>, secret: &str) -> Result<String, String> {
    match store.get(secret).await {
        ResponseResult::Ok { value, .. } => Ok(value),
        ResponseResult::NotFound => Err(format!("secret '{secret}' not found")),
        ResponseResult::Error { error } => {
            Err(format!("error fetching secret '{secret}': {error}"))
        }
    }
}

fn codex_destination(host: &str, path: &str, tls: bool, method: &::http::Method) -> bool {
    tls && host.eq_ignore_ascii_case("chatgpt.com")
        && !path.contains('%')
        && !path.split('/').any(|part| part == "." || part == "..")
        && (path.starts_with("/backend-api/codex/")
            || (method == ::http::Method::GET
                && matches!(
                    path,
                    "/backend-api/wham/accounts/check" | "/backend-api/wham/settings/user"
                )))
}

fn inject_codex(headers: &mut HeaderMap, value: &str) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    struct Credentials {
        access_token: String,
        account_id: String,
    }
    let credentials: Credentials =
        serde_json::from_str(value).map_err(|_| "invalid Codex credential response".to_string())?;
    if credentials.access_token.is_empty() || credentials.account_id.is_empty() {
        return Err("incomplete Codex credentials".into());
    }
    let bearer = format!("Bearer {}", credentials.access_token)
        .parse()
        .map_err(|_| "invalid Codex bearer header".to_string())?;
    let account = credentials
        .account_id
        .parse()
        .map_err(|_| "invalid Codex account header".to_string())?;
    headers.insert(::http::header::AUTHORIZATION, bearer);
    headers.insert("chatgpt-account-id", account);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_injection_is_scoped_to_https_codex_api() {
        assert!(codex_destination(
            "chatgpt.com",
            "/backend-api/codex/responses",
            true,
            &::http::Method::GET
        ));
        for (host, path, tls) in [
            ("chatgpt.com", "/backend-api/codex/responses", false),
            ("other.example", "/backend-api/codex/responses", true),
            ("chatgpt.com", "/backend-api/accounts", true),
            ("chatgpt.com", "/backend-api/codex/../accounts", true),
            ("chatgpt.com", "/backend-api/codex/%2e%2e/accounts", true),
            ("chatgpt.com", "/backend-api/codex-evil/responses", true),
        ] {
            assert!(!codex_destination(host, path, tls, &::http::Method::GET));
        }
    }
    #[test]
    fn native_discovery_is_get_only() {
        for path in [
            "/backend-api/wham/accounts/check",
            "/backend-api/wham/settings/user",
        ] {
            assert!(codex_destination(
                "chatgpt.com",
                path,
                true,
                &::http::Method::GET
            ));
            assert!(!codex_destination(
                "chatgpt.com",
                path,
                true,
                &::http::Method::POST
            ));
        }
    }
    #[test]
    fn codex_replaces_both_headers_from_one_response() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer placeholder".parse().unwrap());
        headers.insert("chatgpt-account-id", "wrong-account".parse().unwrap());
        inject_codex(
            &mut headers,
            r#"{"access_token":"test-token","account_id":"test-account"}"#,
        )
        .unwrap();
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert_eq!(headers["chatgpt-account-id"], "test-account");
    }
    #[test]
    fn malformed_credentials_never_leak_into_errors_or_partially_replace_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer placeholder".parse().unwrap());
        let result = inject_codex(
            &mut headers,
            r#"{"access_token":"SECRET","account_id":"bad\naccount"}"#,
        );
        assert!(result.is_err());
        assert!(!result.unwrap_err().contains("SECRET"));
        assert_eq!(headers["authorization"], "Bearer placeholder");
    }
}
