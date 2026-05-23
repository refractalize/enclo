use std::sync::Arc;
use ::http::{Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt};
use bytes::Bytes;
use hyper::body::Incoming;
use hyper_util::client::legacy::{Client, connect::HttpConnector};

use crate::{config::Config, store::StoreStack};
use super::inject;

pub type BoxResponse = Response<BoxBody<Bytes, hyper::Error>>;

fn bad_gateway(msg: &str) -> BoxResponse {
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

    if let Err(e) = inject::inject(&host_bare, req.headers_mut(), &config, &store).await {
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
