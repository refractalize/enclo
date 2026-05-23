mod config;
mod proxy;
mod store;
mod tls;

use std::{path::PathBuf, sync::Arc};
use clap::{Parser, Subcommand};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use tokio::{net::TcpListener, sync::{watch, Notify}};
use tokio::signal::unix::{signal, SignalKind};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

#[derive(Parser)]
#[command(name = "insectjar-proxy", about = "Authenticating HTTPS proxy")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy server
    Serve {
        /// Path to config file [default: $XDG_CONFIG_HOME/insectjar-proxy/config.toml]
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,

        /// Proxy auth username; must be paired with --password-hash
        #[arg(long, requires = "password_hash")]
        username: Option<String>,

        /// Argon2 password hash for proxy auth; must be paired with --username
        #[arg(long, requires = "username")]
        password_hash: Option<String>,
    },

    /// Generate an Argon2 password hash for use in proxy authentication.
    /// Reads the password from the INSECTJAR_PROXY_PASSWORD environment variable,
    /// or prompts interactively if not set.
    GenerateBasicAuth {
        /// Username to embed in the generated config snippet
        username: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("insectjar_secret_store=info".parse()?)
        )
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let cli = Cli::parse();

    match cli.command {
        Command::GenerateBasicAuth { username } => cmd_generate_basic_auth(&username),
        Command::Serve { config, username, password_hash } => {
            let cli_auth = match (username, password_hash) {
                (Some(username), Some(password_hash)) => {
                    Some(config::ProxyAuthConfig { username, password_hash })
                }
                _ => None,
            };
            let config_path = config.unwrap_or_else(default_config_path);
            run_proxy(config_path, cli_auth).await
        }
    }
}

fn default_config_path() -> PathBuf {
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("insectjar-proxy")
        .join("config.toml")
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

async fn run_proxy(config_path: PathBuf, cli_auth: Option<config::ProxyAuthConfig>) -> anyhow::Result<()> {
    let config = config::Config::from_file(&config_path)
        .map_err(|e| anyhow::anyhow!("failed to load config {}: {e}", config_path.display()))?;

    let shutdown = Arc::new(Notify::new());
    let store = Arc::new(
        store::StoreStack::build(&config.store, Arc::clone(&shutdown), "insectjar-proxy")
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

    let proxy_auth = if let Some(auth) = cli_auth.as_ref() {
        Some(proxy::auth::ProxyAuth::new(auth)
            .map_err(|e| anyhow::anyhow!("proxy auth config error: {e}"))?)
    } else {
        let config = config_rx.borrow();
        config.proxy_auth.as_ref()
            .map(|a| proxy::auth::ProxyAuth::new(a)
                .map_err(|e| anyhow::anyhow!("proxy auth config error: {e}")))
            .transpose()?
    };

    tracing::info!("listening on {}", config_rx.borrow().listen);
    if proxy_auth.is_some() {
        tracing::info!("proxy authentication enabled");
    }

    let shutdown_clone = Arc::clone(&shutdown);
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        shutdown_clone.notify_one();
    });

    let mut sighup = signal(SignalKind::hangup())?;

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("accept error: {e}");
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
                        tracing::warn!("connection error from {peer}: {e}");
                    }
                });
            }

            _ = sighup.recv() => {
                match config::Config::from_file(&config_path) {
                    Err(e) => tracing::warn!("config reload failed: {e}"),
                    Ok(new_config) => {
                        store.reload(&new_config.store).await;
                        config_tx.send(Arc::new(new_config)).ok();
                        tracing::info!("config reloaded");
                    }
                }
            }

            _ = shutdown.notified() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }

    Ok(())
}
