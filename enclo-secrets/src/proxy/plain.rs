use ::http::{Request, Response, StatusCode};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt};
use hyper::body::Incoming;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use std::sync::Arc;

use super::inject;
use crate::config::Config;
use crate::store::StoreStack;

pub type BoxResponse = Response<BoxBody<Bytes, hyper::Error>>;

pub fn bad_gateway(msg: &str) -> BoxResponse {
    let body = http_body_util::Full::new(Bytes::from(format!("{msg}\n")))
        .map_err(|e| match e {})
        .boxed();
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(body)
        .unwrap()
}

pub async fn handle(
    mut req: Request<Incoming>,
    config: Arc<Config>,
    store: Arc<StoreStack>,
    client: Arc<Client<HttpConnector, Incoming>>,
) -> BoxResponse {
    let host = req
        .headers()
        .get(::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let host_bare = host.split(':').next().unwrap_or(&host).to_string();

    let path = req.uri().path().to_string();
    let method = req.method().clone();
    if let Err(e) = inject::inject(
        &host_bare,
        &path,
        false,
        &method,
        req.headers_mut(),
        &config,
        &store,
    )
    .await
    {
        return bad_gateway(&format!("credential injection failed: {e}"));
    }

    super::strip_hop_by_hop(req.headers_mut());

    match client.request(req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            Response::from_parts(parts, body.boxed())
        }
        Err(e) => bad_gateway(&format!("upstream error: {e}")),
    }
}
