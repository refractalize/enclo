pub mod auth;
pub mod connect;
pub mod inject;
pub mod plain;

use std::sync::Arc;
use ::http::{Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use bytes::Bytes;
use hyper::body::Incoming;
use hyper_util::client::legacy::{Client, connect::HttpConnector};

use crate::config::Config;
use crate::store::StoreStack;
use crate::tls::TlsState;
use auth::ProxyAuth;
use plain::BoxResponse;

pub async fn handle_request(
    req: Request<Incoming>,
    config: Arc<Config>,
    store: Arc<StoreStack>,
    tls: Arc<TlsState>,
    http_client: Arc<Client<HttpConnector, Incoming>>,
    proxy_auth: Option<Arc<ProxyAuth>>,
) -> Result<BoxResponse, hyper::Error> {
    if let Some(auth) = &proxy_auth {
        if !auth.verify(&req) {
            return Ok(auth::proxy_auth_required());
        }
    }

    if req.method() == Method::CONNECT {
        Ok(handle_connect(req, config, store, tls).await)
    } else {
        Ok(plain::handle(req, config, store, http_client).await)
    }
}

async fn handle_connect(
    req: Request<Incoming>,
    config: Arc<Config>,
    store: Arc<StoreStack>,
    tls: Arc<TlsState>,
) -> BoxResponse {
    let Some((host, port)) = parse_connect_authority(req.uri()) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid CONNECT authority");
    };

    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                connect::tunnel(upgraded, host, port, config, store, tls).await;
            }
            Err(e) => tracing::warn!("upgrade error: {e}"),
        }
    });

    connect::accepted()
}

fn parse_connect_authority(uri: &::http::Uri) -> Option<(String, u16)> {
    let authority = uri.authority()?;
    let host = authority.host().to_string();
    let port = authority.port_u16().unwrap_or(443);
    Some((host, port))
}

pub(super) fn strip_hop_by_hop(headers: &mut ::http::HeaderMap) {
    for name in &[
        ::http::header::CONNECTION,
        ::http::header::TRANSFER_ENCODING,
        ::http::header::TE,
        ::http::header::TRAILER,
        ::http::header::UPGRADE,
        ::http::header::HeaderName::from_static("proxy-connection"),
        ::http::header::HeaderName::from_static("keep-alive"),
    ] {
        headers.remove(name);
    }
}

fn error_response(status: StatusCode, msg: &str) -> BoxResponse {
    let body = http_body_util::Full::new(Bytes::from(format!("{msg}\n")))
        .map_err(|e| match e {})
        .boxed();
    Response::builder().status(status).body(body).unwrap()
}
