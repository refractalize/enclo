use std::sync::Arc;
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use base64::{Engine, engine::general_purpose::STANDARD};
use ::http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use bytes::Bytes;
use crate::config::ProxyAuthConfig;
use super::plain::BoxResponse;

pub struct ProxyAuth {
    username: String,
    password_hash: String,
}

impl ProxyAuth {
    pub fn new(config: &ProxyAuthConfig) -> Arc<Self> {
        Arc::new(Self {
            username: config.username.clone(),
            password_hash: config.password_hash.clone(),
        })
    }


    /// Returns true if the request carries valid proxy credentials.
    pub fn verify<B>(&self, req: &Request<B>) -> bool {
        let Some(header) = req.headers().get("proxy-authorization") else {
            return false;
        };
        let Ok(value) = header.to_str() else { return false; };
        let Some(encoded) = value.strip_prefix("Basic ") else { return false; };
        let Ok(decoded) = STANDARD.decode(encoded.trim()) else { return false; };
        let Ok(credentials) = std::str::from_utf8(&decoded) else { return false; };
        let Some((user, password)) = credentials.split_once(':') else { return false; };

        if user != self.username {
            return false;
        }

        let Ok(parsed) = PasswordHash::new(&self.password_hash) else { return false; };
        Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
    }
}

/// 407 response sent when credentials are missing or wrong.
pub fn proxy_auth_required() -> BoxResponse {
    Response::builder()
        .status(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
        .header("proxy-authenticate", "Basic realm=\"insectjar-proxy\"")
        .body(Full::new(Bytes::from("Proxy authentication required\n"))
            .map_err(|e| match e {})
            .boxed())
        .unwrap()
}
