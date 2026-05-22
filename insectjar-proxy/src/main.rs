mod config;
mod proxy;
mod store;
mod tls;

use std::{path::PathBuf, sync::Arc};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use tokio::{net::TcpListener, sync::{watch, Notify}};
use tokio::signal::unix::{signal, SignalKind};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("generate-basic-auth") => {
            let username = args.next()
                .ok_or_else(|| anyhow::anyhow!("usage: insectjar-proxy generate-basic-auth <username>"))?;
            return cmd_generate_basic_auth(&username);
        }
        _ => {}
    }

    run_proxy().await
}

fn cmd_generate_basic_auth(username: &str) -> anyhow::Result<()> {
    use argon2::{
        password_hash::{PasswordHasher, SaltString},
        Argon2,
    };
    use rand_core::OsRng;

    let password = match std::env::var("INSECTJAR_PROXY_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password("Password: ")
            .map_err(|e| anyhow::anyhow!("failed to read password: {e}"))?,
    };

    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("failed to hash password: {e}"))?
        .to_string();

    println!("[proxy_auth]");
    println!("username      = \"{username}\"");
    println!("password_hash = \"{hash}\"");

    Ok(())
}

async fn run_proxy() -> anyhow::Result<()> {
    let config_path = std::env::var("INSECTJAR_PROXY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs_next::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("insectjar-proxy")
                .join("config.toml")
        });

    let config = config::Config::from_file(&config_path)
        .map_err(|e| anyhow::anyhow!("failed to load config {}: {e}", config_path.display()))?;

    let shutdown = Arc::new(Notify::new());
    let store = Arc::new(
        store::StoreStack::build(&config.store, Arc::clone(&shutdown))
            .await
            .map_err(|e| anyhow::anyhow!("failed to start store: {e}"))?,
    );
    let tls = tls::TlsState::load(&config.tls.ca_cert, &config.tls.ca_key)
        .map_err(|e| anyhow::anyhow!(
            "failed to load TLS CA ({} / {}): {e}",
            config.tls.ca_cert.display(),
            config.tls.ca_key.display()
        ))?;

    let http_client: Arc<Client<HttpConnector, _>> = Arc::new(
        Client::builder(TokioExecutor::new()).build(HttpConnector::new()),
    );

    let listener = TcpListener::bind(&config.listen).await
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", config.listen))?;

    let (config_tx, config_rx) = watch::channel(Arc::new(config));

    let proxy_auth = config_rx.borrow()
        .proxy_auth
        .as_ref()
        .map(proxy::auth::ProxyAuth::new);

    eprintln!("insectjar-proxy: listening on {}", config_rx.borrow().listen);
    if proxy_auth.is_some() {
        eprintln!("insectjar-proxy: proxy authentication enabled");
    }

    let shutdown_clone = Arc::clone(&shutdown);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        shutdown_clone.notify_waiters();
    });

    let mut sighup = signal(SignalKind::hangup())?;

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("insectjar-proxy: accept error: {e}");
                        continue;
                    }
                };

                let config = Arc::clone(&config_rx.borrow());
                let store = Arc::clone(&store);
                let tls = Arc::clone(&tls);
                let http_client = Arc::clone(&http_client);
                let proxy_auth = proxy_auth.clone();

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .preserve_header_case(true)
                        .serve_connection(
                            io,
                            service_fn(move |req| {
                                proxy::handle_request(
                                    req,
                                    Arc::clone(&config),
                                    Arc::clone(&store),
                                    Arc::clone(&tls),
                                    Arc::clone(&http_client),
                                    proxy_auth.clone(),
                                )
                            }),
                        )
                        .with_upgrades()
                        .await
                    {
                        eprintln!("insectjar-proxy: connection error from {peer}: {e}");
                    }
                });
            }

            _ = sighup.recv() => {
                match config::Config::from_file(&config_path) {
                    Err(e) => eprintln!("insectjar-proxy: config reload failed: {e}"),
                    Ok(new_config) => {
                        store.reload(&new_config.store).await;
                        config_tx.send(Arc::new(new_config)).ok();
                        eprintln!("insectjar-proxy: config reloaded");
                    }
                }
            }

            _ = shutdown.notified() => {
                eprintln!("insectjar-proxy: shutting down");
                break;
            }
        }
    }

    Ok(())
}
