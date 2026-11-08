use ::http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio::{net::TcpStream, sync::Mutex};
use tokio_rustls::rustls::pki_types::ServerName;

use super::plain::BoxResponse;
use crate::config::Config;
use crate::store::StoreStack;
use crate::tls::TlsState;

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
        if let Err(e) = upstream_conn.with_upgrades().await {
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
                    let path = req.uri().path().to_string();
                    let method = req.method().clone();
                    super::inject::inject(
                        &host,
                        &path,
                        true,
                        &method,
                        req.headers_mut(),
                        &config,
                        &store,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;

                    let resp = forward_upgrade(req, &sender).await?;
                    let (parts, body) = resp.into_parts();
                    Ok::<_, anyhow::Error>(Response::from_parts(parts, body))
                }
            }),
        )
        .with_upgrades()
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

async fn forward_upgrade(
    mut req: Request<Incoming>,
    sender: &Mutex<hyper::client::conn::http1::SendRequest<Incoming>>,
) -> anyhow::Result<Response<Incoming>> {
    let upgrading = req
        .headers()
        .get("upgrade")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let upgrade_header = req.headers().get("upgrade").cloned();
    let client_upgrade = upgrading.then(|| hyper::upgrade::on(&mut req));
    super::strip_hop_by_hop(req.headers_mut());
    if let Some(value) = upgrade_header.filter(|_| upgrading) {
        req.headers_mut().insert("upgrade", value);
        req.headers_mut()
            .insert("connection", "upgrade".parse().unwrap());
    }

    let mut s = sender.lock().await;
    s.ready().await.map_err(|e| anyhow::anyhow!(e))?;
    let mut resp = s.send_request(req).await.map_err(|e| anyhow::anyhow!(e))?;
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_upgrade) = client_upgrade {
            let upstream_upgrade = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                if let Ok((client, upstream)) = tokio::try_join!(client_upgrade, upstream_upgrade) {
                    let _ = tokio::io::copy_bidirectional(
                        &mut TokioIo::new(client),
                        &mut TokioIo::new(upstream),
                    )
                    .await;
                }
            });
        }
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn websocket_upgrade_relays_bytes_in_both_directions() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let (upstream, proxy_upstream) = tokio::io::duplex(8192);
            let (mut client, proxy_client) = tokio::io::duplex(8192);
            tokio::spawn(async move {
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(upstream), hyper::service::service_fn(|mut req: Request<Incoming>| async move {
                        assert_eq!(req.headers()["upgrade"], "websocket");
                        assert_eq!(req.headers()["authorization"], "Bearer injected-token");
                        tokio::spawn(async move {
                            let mut io = TokioIo::new(hyper::upgrade::on(&mut req).await.unwrap());
                            let mut bytes = [0; 4];
                            io.read_exact(&mut bytes).await.unwrap();
                            assert_eq!(&bytes, b"ping");
                            io.write_all(b"pong").await.unwrap();
                        });
                        Ok::<_, hyper::Error>(Response::builder().status(101).header("connection", "upgrade").header("upgrade", "websocket").body(Empty::<bytes::Bytes>::new()).unwrap())
                    })).with_upgrades().await.unwrap();
            });
            let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(proxy_upstream)).await.unwrap();
            tokio::spawn(connection.with_upgrades());
            let sender = Arc::new(Mutex::new(sender));
            tokio::spawn(async move {
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(proxy_client), hyper::service::service_fn(move |mut req: Request<Incoming>| {
                        let sender = sender.clone();
                        async move {
                            req.headers_mut().insert("authorization", "Bearer injected-token".parse().unwrap());
                            forward_upgrade(req, &sender).await
                        }
                    })).with_upgrades().await.unwrap();
            });
            client.write_all(b"GET /responses HTTP/1.1\r\nHost: example.invalid\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n").await.unwrap();
            let mut response = Vec::new();
            while !response.ends_with(b"\r\n\r\n") {
                response.push(client.read_u8().await.unwrap());
            }
            assert!(response.starts_with(b"HTTP/1.1 101"));
            client.write_all(b"ping").await.unwrap();
            let mut bytes = [0; 4];
            client.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"pong");
        }).await.unwrap();
    }
}
