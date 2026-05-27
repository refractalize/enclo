use std::sync::Arc;
use ::http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::{net::TcpStream, sync::Mutex};
use tokio_rustls::rustls::pki_types::ServerName;

use crate::config::Config;
use crate::store::StoreStack;
use crate::tls::TlsState;
use super::plain::BoxResponse;

pub fn accepted() -> BoxResponse {
    Response::builder()
        .status(StatusCode::OK)
        .body(Empty::new().map_err(|e| match e {}).boxed())
        .unwrap()
}

pub async fn tunnel(
    upgraded: hyper::upgrade::Upgraded,
    host: String,
    port: u16,
    config: Arc<Config>,
    store: Arc<StoreStack>,
    tls: Arc<TlsState>,
) {
    if let Err(e) = tunnel_inner(upgraded, &host, port, config, store, tls).await {
        tracing::warn!("CONNECT tunnel error for {host}:{port}: {e}");
    }
}

async fn tunnel_inner(
    upgraded: hyper::upgrade::Upgraded,
    host: &str,
    port: u16,
    config: Arc<Config>,
    store: Arc<StoreStack>,
    tls: Arc<TlsState>,
) -> anyhow::Result<()> {
    let acceptor = tls.acceptor_for(host)?;
    let client_tls = acceptor.accept(TokioIo::new(upgraded)).await?;

    let tcp = TcpStream::connect((host, port)).await?;
    let server_name = ServerName::try_from(host.to_string())?;
    let upstream_tls = tls.connector.connect(server_name, tcp).await?;

    let (upstream_sender, upstream_conn) =
        hyper::client::conn::http1::handshake(TokioIo::new(upstream_tls)).await?;

    tokio::spawn(async move {
        if let Err(e) = upstream_conn.await {
            tracing::warn!("upstream connection error: {e}");
        }
    });

    let upstream_sender = Arc::new(Mutex::new(upstream_sender));
    let host = host.to_string();

    hyper::server::conn::http1::Builder::new()
        .serve_connection(
            TokioIo::new(client_tls),
            hyper::service::service_fn(move |mut req: Request<Incoming>| {
                let host = host.clone();
                let config = Arc::clone(&config);
                let store = Arc::clone(&store);
                let sender = Arc::clone(&upstream_sender);

                async move {
                    super::inject::inject(&host, req.headers_mut(), &config, &store)
                        .await
                        .map_err(|e| anyhow::anyhow!(e))?;

                    super::strip_hop_by_hop(req.headers_mut());

                    let mut s = sender.lock().await;
                    s.ready().await.map_err(|e| anyhow::anyhow!(e))?;
                    let resp = s.send_request(req).await.map_err(|e| anyhow::anyhow!(e))?;
                    let (parts, body) = resp.into_parts();
                    Ok::<_, anyhow::Error>(Response::from_parts(parts, body))
                }
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
}
