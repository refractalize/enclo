mod access;
mod config;
mod proxy;
mod server;
mod store;
mod tls;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use clap::{Parser, Subcommand};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
};
use tokio::{
    net::{TcpListener, TcpStream},
    signal::unix::{signal, SignalKind},
    sync::{Notify, RwLock},
    task::JoinHandle,
};
use enclo_backend_protocol::service;

const LISTENER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(name = "enclo-secrets", about = "Secret service and authenticating proxy for enclo")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server
    Serve {
        /// Path to config file [default: $XDG_CONFIG_HOME/enclo-secrets/config.toml]
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,

        /// Override the Unix socket path from config
        #[arg(long, short = 's')]
        socket: Option<PathBuf>,

        /// Proxy auth username override (must be paired with --password-hash)
        #[arg(long, requires = "password_hash")]
        username: Option<String>,

        /// Proxy auth password hash override (must be paired with --username)
        #[arg(long, requires = "username")]
        password_hash: Option<String>,

        /// Override the proxy listen address from config (e.g. 127.0.0.1:8080)
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
    },

    /// Generate an Argon2 password hash for proxy authentication.
    /// Reads the password from the ENCLO_PROXY_PASSWORD environment variable,
    /// or prompts interactively if not set.
    GenerateBasicAuth {
        /// Username to embed in the generated config snippet
        username: String,
    },
}

struct HttpRuntime {
    config: Arc<config::Config>,
    store: Arc<store::StoreStack>,
    tls: Arc<tls::TlsState>,
    proxy_auth: Option<Arc<proxy::auth::ProxyAuth>>,
}

struct ListenerState {
    addr: std::net::SocketAddr,
    active_connections: AtomicUsize,
    next_connection_id: AtomicUsize,
    connection_tasks: StdMutex<HashMap<usize, tokio::task::AbortHandle>>,
    idle_notify: Notify,
    stop_accepting: Notify,
}

struct ListenerHandle {
    state: Arc<ListenerState>,
    task: JoinHandle<()>,
}

struct ConnectionGuard {
    listener: Arc<ListenerState>,
    id: usize,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.listener
            .connection_tasks
            .lock()
            .expect("connection task registry poisoned")
            .remove(&self.id);
        if self.listener.active_connections.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.listener.idle_notify.notify_waiters();
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("enclo_secrets=info".parse()?)
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::GenerateBasicAuth { username } => cmd_generate_basic_auth(&username),
        Command::Serve { config, socket, username, password_hash, listen } => {
            let cli_auth = match (username, password_hash) {
                (Some(username), Some(password_hash)) => {
                    Some(config::ProxyAuthConfig { username, password_hash })
                }
                _ => None,
            };
            let config_path = config.unwrap_or_else(default_config_path);
            run(config_path, socket, cli_auth, listen).await
        }
    }
}

fn default_config_path() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".config/enclo/secrets.toml")
}

fn cmd_generate_basic_auth(username: &str) -> anyhow::Result<()> {
    use argon2::{
        password_hash::{PasswordHasher, SaltString},
        Argon2,
    };
    use rand_core::OsRng;

    let password = match std::env::var("ENCLO_PROXY_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password("Password: ")
            .map_err(|e| anyhow::anyhow!("failed to read password: {e}"))?,
    };

    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("failed to hash password: {e}"))?
        .to_string();

    println!("[proxy.auth]");
    println!("username      = \"{username}\"");
    println!("password_hash = \"{hash}\"");

    Ok(())
}

async fn run(
    config_path: PathBuf,
    socket_override: Option<PathBuf>,
    cli_auth: Option<config::ProxyAuthConfig>,
    listen_override: Option<std::net::SocketAddr>,
) -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let mut config = config::Config::from_file(&config_path)
        .map_err(|e| anyhow::anyhow!("failed to load config {}: {e}", config_path.display()))?;
    if let Some(addr) = listen_override {
        config.proxy.listen = Some(addr);
    }

    let shutdown = Arc::new(Notify::new());
    let store = Arc::new(
        store::StoreStack::build(&config.store, Arc::clone(&shutdown), "enclo-secrets")
            .await
            .map_err(|e| anyhow::anyhow!("failed to start store: {e}"))?,
    );

    let config = Arc::new(config);
    let config_state = Arc::new(RwLock::new(Arc::clone(&config)));

    service::spawn_shutdown_listener(Arc::clone(&shutdown));
    let mut sighup = signal(SignalKind::hangup())?;

    // --- Socket server ---
    if config.secrets.socket.enabled {
        let socket_path = socket_override
            .clone()
            .or_else(|| config.secrets.socket.path.clone())
            .unwrap_or_else(default_socket_path);
        let store = Arc::clone(&store);
        let config_state = Arc::clone(&config_state);
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            if let Err(e) = server::socket::run(socket_path, store, config_state, shutdown).await {
                tracing::error!("socket server error: {e:#}");
            }
        });
    }

    // --- HTTP listener (proxy) ---
    let mut active_listener: Option<ListenerHandle> = None;
    let mut draining_listeners: Vec<JoinHandle<()>> = Vec::new();
    let http_client: Arc<Client<HttpConnector, Incoming>> =
        Arc::new(Client::builder(TokioExecutor::new()).build(HttpConnector::new()));

    if config.proxy.enabled {
        let listen_addr = config.proxy.listen.expect("validated: proxy.listen required when proxy.enabled");
        let runtime = Arc::new(build_http_runtime(Arc::clone(&config), Arc::clone(&store), cli_auth.as_ref()).await?);
        let runtime_state = Arc::new(RwLock::new(Arc::clone(&runtime)));

        let listener = TcpListener::bind(listen_addr)
            .await
            .map_err(|e| anyhow::anyhow!("failed to bind {listen_addr}: {e}"))?;

        tracing::info!("HTTP listener on {listen_addr}");
        if runtime.proxy_auth.is_some() {
            tracing::info!("proxy authentication enabled");
        }

        active_listener = Some(start_listener(
            listener,
            Arc::clone(&runtime_state),
            Arc::clone(&http_client),
            Arc::clone(&shutdown),
        ));

        loop {
            tokio::select! {
                _ = sighup.recv() => {
                    match reload(
                        &config_path,
                        socket_override.clone(),
                        &store,
                        &config_state,
                        cli_auth.as_ref(),
                        &mut active_listener,
                        &mut draining_listeners,
                        &runtime_state,
                        Arc::clone(&http_client),
                        Arc::clone(&shutdown),
                    ).await {
                        Ok(()) => tracing::info!("config reloaded"),
                        Err(e) => tracing::warn!("config reload failed: {e:#}"),
                    }
                }

                _ = shutdown.notified() => {
                    tracing::info!("shutting down");
                    break;
                }
            }
        }
    } else {
        loop {
            tokio::select! {
                _ = sighup.recv() => {
                    match config::Config::from_file(&config_path) {
                        Err(e) => tracing::warn!("config reload failed: {e}"),
                        Ok(new_config) => {
                            let new_config = Arc::new(new_config);
                            if let Err(e) = store.reload(&new_config.store).await {
                                tracing::warn!("store reload failed: {e:#}");
                            } else {
                                *config_state.write().await = Arc::clone(&new_config);
                                tracing::info!("config reloaded");
                            }
                        }
                    }
                }

                _ = shutdown.notified() => {
                    tracing::info!("shutting down");
                    break;
                }
            }
        }
    }

    if let Some(listener) = active_listener {
        draining_listeners.push(tokio::spawn(drain_listener(listener)));
    }
    for task in draining_listeners {
        let _ = task.await;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn reload(
    config_path: &PathBuf,
    socket_override: Option<PathBuf>,
    store: &Arc<store::StoreStack>,
    config_state: &Arc<RwLock<Arc<config::Config>>>,
    cli_auth: Option<&config::ProxyAuthConfig>,
    active_listener: &mut Option<ListenerHandle>,
    draining_listeners: &mut Vec<JoinHandle<()>>,
    runtime_state: &Arc<RwLock<Arc<HttpRuntime>>>,
    http_client: Arc<Client<HttpConnector, Incoming>>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()> {
    let new_config = config::Config::from_file(config_path)?;
    let new_config = Arc::new(new_config);

    store.reload(&new_config.store).await?;
    *config_state.write().await = Arc::clone(&new_config);

    let _ = socket_override; // socket server reads config_state live; no restart needed

    let current_listen = {
        let rt = runtime_state.read().await;
        rt.config.proxy.listen
    };

    let new_runtime = Arc::new(
        build_http_runtime(Arc::clone(&new_config), Arc::clone(store), cli_auth).await?
    );

    let new_listen = new_config.proxy.listen;
    let needs_new_listener = new_listen != current_listen;

    *runtime_state.write().await = Arc::clone(&new_runtime);

    if needs_new_listener {
        if let Some(listen_addr) = new_listen {
            let new_tcp = TcpListener::bind(listen_addr)
                .await
                .map_err(|e| anyhow::anyhow!("failed to bind {listen_addr}: {e}"))?;
            let new_handle = start_listener(
                new_tcp,
                Arc::clone(runtime_state),
                Arc::clone(&http_client),
                Arc::clone(&shutdown),
            );
            if let Some(old) = active_listener.replace(new_handle) {
                draining_listeners.push(tokio::spawn(drain_listener(old)));
            }
        }
    }

    tracing::info!("config reloaded");
    Ok(())
}

async fn build_http_runtime(
    config: Arc<config::Config>,
    store: Arc<store::StoreStack>,
    cli_auth: Option<&config::ProxyAuthConfig>,
) -> anyhow::Result<HttpRuntime> {
    let tls_config = config.proxy.tls.as_ref().expect("validated: proxy.tls required");
    let tls = tls::TlsState::load(&tls_config.ca_cert, &tls_config.ca_key).map_err(|e| {
        anyhow::anyhow!(
            "failed to load TLS CA ({} / {}): {e}",
            tls_config.ca_cert.display(),
            tls_config.ca_key.display()
        )
    })?;

    let proxy_auth = if let Some(auth) = cli_auth {
        Some(proxy::auth::ProxyAuth::new(auth)
            .map_err(|e| anyhow::anyhow!("proxy auth config error: {e}"))?)
    } else if let Some(auth) = &config.proxy.auth {
        Some(proxy::auth::ProxyAuth::new(auth)
            .map_err(|e| anyhow::anyhow!("proxy auth config error: {e}"))?)
    } else {
        None
    };

    Ok(HttpRuntime { config, store, tls, proxy_auth })
}

fn start_listener(
    listener: TcpListener,
    runtime_state: Arc<RwLock<Arc<HttpRuntime>>>,
    http_client: Arc<Client<HttpConnector, Incoming>>,
    shutdown: Arc<Notify>,
) -> ListenerHandle {
    let addr = listener.local_addr().expect("listener must have local addr");
    let state = Arc::new(ListenerState {
        addr,
        active_connections: AtomicUsize::new(0),
        next_connection_id: AtomicUsize::new(0),
        connection_tasks: StdMutex::new(HashMap::new()),
        idle_notify: Notify::new(),
        stop_accepting: Notify::new(),
    });

    let task_state = Arc::clone(&state);
    let task = tokio::spawn(async move {
        run_listener(listener, task_state, runtime_state, http_client, shutdown).await;
    });

    ListenerHandle { state, task }
}

async fn run_listener(
    listener: TcpListener,
    state: Arc<ListenerState>,
    runtime_state: Arc<RwLock<Arc<HttpRuntime>>>,
    http_client: Arc<Client<HttpConnector, Incoming>>,
    shutdown: Arc<Notify>,
) {
    loop {
        tokio::select! {
            _ = state.stop_accepting.notified() => break,
            _ = shutdown.notified() => break,
            result = listener.accept() => {
                let (stream, peer) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("accept error on {}: {e}", state.addr);
                        continue;
                    }
                };

                let (connection_id, guard) = begin_connection(&state);
                let runtime_state = Arc::clone(&runtime_state);
                let http_client = Arc::clone(&http_client);

                let task = tokio::spawn(async move {
                    handle_http_connection(stream, peer, runtime_state, http_client).await;
                    drop(guard);
                });
                register_connection_task(&state, connection_id, task.abort_handle());
            }
        }
    }
}

fn begin_connection(listener: &Arc<ListenerState>) -> (usize, ConnectionGuard) {
    let id = listener.next_connection_id.fetch_add(1, Ordering::Relaxed);
    listener.active_connections.fetch_add(1, Ordering::AcqRel);
    (id, ConnectionGuard { listener: Arc::clone(listener), id })
}

fn register_connection_task(
    listener: &Arc<ListenerState>,
    id: usize,
    abort_handle: tokio::task::AbortHandle,
) {
    listener
        .connection_tasks
        .lock()
        .expect("connection task registry poisoned")
        .insert(id, abort_handle);
}

async fn handle_http_connection(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    runtime_state: Arc<RwLock<Arc<HttpRuntime>>>,
    http_client: Arc<Client<HttpConnector, Incoming>>,
) {
    let io = TokioIo::new(stream);
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .serve_connection(
            io,
            service_fn(move |req| {
                let runtime_state = Arc::clone(&runtime_state);
                let http_client = Arc::clone(&http_client);
                async move {
                    let runtime = runtime_state.read().await.clone();
                    proxy::handle_request(
                        req,
                        Arc::clone(&runtime.config),
                        Arc::clone(&runtime.store),
                        Arc::clone(&runtime.tls),
                        Arc::clone(&http_client),
                        runtime.proxy_auth.clone(),
                    )
                    .await
                }
            }),
        )
        .with_upgrades()
        .await
    {
        tracing::warn!("connection error from {peer}: {e}");
    }
}

async fn drain_listener(listener: ListenerHandle) {
    let addr = listener.state.addr;
    listener.state.stop_accepting.notify_waiters();
    let _ = listener.task.await;
    if !drain_listener_state(&listener.state, LISTENER_DRAIN_TIMEOUT).await {
        tracing::warn!(
            addr = %addr,
            timeout_secs = LISTENER_DRAIN_TIMEOUT.as_secs(),
            "listener drain timed out; aborting remaining connection tasks"
        );
    }
    tracing::info!("listener on {} drained", addr);
}

async fn wait_for_idle(listener: &ListenerState) {
    loop {
        if listener.active_connections.load(Ordering::Acquire) == 0 {
            return;
        }
        listener.idle_notify.notified().await;
    }
}

async fn abort_connections(listener: &Arc<ListenerState>) {
    let handles: Vec<tokio::task::AbortHandle> = listener
        .connection_tasks
        .lock()
        .expect("connection task registry poisoned")
        .drain()
        .map(|(_, handle)| handle)
        .collect();
    for handle in handles {
        handle.abort();
    }
}

async fn drain_listener_state(listener: &Arc<ListenerState>, timeout: Duration) -> bool {
    if tokio::time::timeout(timeout, wait_for_idle(listener)).await.is_ok() {
        return true;
    }
    abort_connections(listener).await;
    wait_for_idle(listener).await;
    false
}

fn default_socket_path() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("enclo-secrets.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_listener_state() -> Arc<ListenerState> {
        Arc::new(ListenerState {
            addr: "127.0.0.1:12345".parse().unwrap(),
            active_connections: AtomicUsize::new(0),
            next_connection_id: AtomicUsize::new(0),
            connection_tasks: StdMutex::new(HashMap::new()),
            idle_notify: Notify::new(),
            stop_accepting: Notify::new(),
        })
    }

    #[tokio::test]
    async fn drain_listener_state_returns_immediately_when_idle() {
        let state = test_listener_state();
        assert!(drain_listener_state(&state, Duration::from_millis(10)).await);
    }

    #[tokio::test]
    async fn drain_listener_state_waits_for_connection_to_finish() {
        let state = test_listener_state();
        let (id, guard) = begin_connection(&state);
        let task: JoinHandle<()> = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(guard);
        });
        register_connection_task(&state, id, task.abort_handle());

        assert!(drain_listener_state(&state, Duration::from_millis(100)).await);
        let _ = task.await;
        assert_eq!(state.active_connections.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn drain_listener_state_aborts_stuck_connections_after_timeout() {
        let state = test_listener_state();
        let (id, guard) = begin_connection(&state);
        let task: JoinHandle<()> = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        register_connection_task(&state, id, task.abort_handle());

        assert!(!drain_listener_state(&state, Duration::from_millis(10)).await);
        let _ = task.await;
        assert_eq!(state.active_connections.load(Ordering::Acquire), 0);
    }
}
