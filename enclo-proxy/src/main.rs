mod config;
mod proxy;
mod store;
mod tls;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Mutex as StdMutex,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
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
use enclo_proxy_store_common::service;

const LISTENER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(name = "enclo-proxy", about = "Authenticating HTTPS proxy")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy server
    Serve {
        /// Path to config file [default: $XDG_CONFIG_HOME/enclo-proxy/config.toml]
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

struct ProxyRuntime {
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
                .add_directive("enclo_secret_store=info".parse()?)
        )
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let cli = Cli::parse();

    match cli.command {
        Command::GenerateBasicAuth { username } => cmd_generate_basic_auth(&username),
        Command::Serve {
            config,
            username,
            password_hash,
        } => {
            let cli_auth = match (username, password_hash) {
                (Some(username), Some(password_hash)) => {
                    Some(config::ProxyAuthConfig {
                        username,
                        password_hash,
                    })
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
        .join("enclo-proxy")
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

async fn run_proxy(
    config_path: PathBuf,
    cli_auth: Option<config::ProxyAuthConfig>,
) -> anyhow::Result<()> {
    let config = config::Config::from_file(&config_path)
        .map_err(|e| anyhow::anyhow!("failed to load config {}: {e}", config_path.display()))?;

    let shutdown = Arc::new(Notify::new());
    let store = Arc::new(
        store::StoreStack::build(&config.store, Arc::clone(&shutdown), "enclo-proxy")
            .await
            .map_err(|e| anyhow::anyhow!("failed to start store: {e}"))?,
    );
    let runtime = Arc::new(build_runtime(config, Arc::clone(&store), cli_auth.as_ref()).await?);
    let runtime_state = Arc::new(RwLock::new(Arc::clone(&runtime)));

    let http_client: Arc<Client<HttpConnector, Incoming>> =
        Arc::new(Client::builder(TokioExecutor::new()).build(HttpConnector::new()));

    let listener = TcpListener::bind(runtime.config.listen)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", runtime.config.listen))?;
    let mut active_listener = start_listener(
        listener,
        Arc::clone(&runtime_state),
        Arc::clone(&http_client),
        Arc::clone(&shutdown),
    );
    let mut draining_listeners: Vec<JoinHandle<()>> = Vec::new();

    tracing::info!("listening on {}", runtime.config.listen);
    if runtime.proxy_auth.is_some() {
        tracing::info!("proxy authentication enabled");
    }

    service::spawn_shutdown_listener(Arc::clone(&shutdown));

    let mut sighup = signal(SignalKind::hangup())?;

    loop {
        tokio::select! {
            _ = sighup.recv() => {
                match config::Config::from_file(&config_path) {
                    Err(e) => tracing::warn!("config reload failed: {e}"),
                    Ok(new_config) => {
                        let current_listen = {
                            let runtime = runtime_state.read().await;
                            runtime.config.listen
                        };
                        match prepare_reload(
                            new_config,
                            current_listen,
                            Arc::clone(&store),
                            cli_auth.as_ref(),
                        )
                        .await
                        {
                            Err(e) => tracing::warn!("config reload failed: {e:#}"),
                            Ok(reload) => {
                                let runtime = Arc::new(reload.runtime);
                                let auth_enabled = runtime.proxy_auth.is_some();
                                let listen_addr = runtime.config.listen;
                                *runtime_state.write().await = Arc::clone(&runtime);

                                if let Some(listener) = reload.listener {
                                    let next_listener = start_listener(
                                        listener,
                                        Arc::clone(&runtime_state),
                                        Arc::clone(&http_client),
                                        Arc::clone(&shutdown),
                                    );
                                    let old_listener = std::mem::replace(&mut active_listener, next_listener);
                                    draining_listeners.push(tokio::spawn(drain_listener(old_listener)));
                                }

                                tracing::info!("config reloaded; listening on {}", listen_addr);
                                if auth_enabled {
                                    tracing::info!("proxy authentication enabled");
                                }
                            }
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

    draining_listeners.push(tokio::spawn(drain_listener(active_listener)));
    for task in draining_listeners {
        let _ = task.await;
    }

    Ok(())
}

struct ReloadPrepared {
    runtime: ProxyRuntime,
    listener: Option<TcpListener>,
}

async fn build_runtime(
    config: config::Config,
    store: Arc<store::StoreStack>,
    cli_auth: Option<&config::ProxyAuthConfig>,
) -> anyhow::Result<ProxyRuntime> {
    let tls = tls::TlsState::load(&config.tls.ca_cert, &config.tls.ca_key).map_err(|e| {
        anyhow::anyhow!(
            "failed to load TLS CA ({} / {}): {e}",
            config.tls.ca_cert.display(),
            config.tls.ca_key.display()
        )
    })?;

    let proxy_auth = load_proxy_auth(&config, cli_auth)?;

    Ok(ProxyRuntime {
        config: Arc::new(config),
        store,
        tls,
        proxy_auth,
    })
}

async fn prepare_reload(
    config: config::Config,
    current_listen: std::net::SocketAddr,
    store: Arc<store::StoreStack>,
    cli_auth: Option<&config::ProxyAuthConfig>,
) -> anyhow::Result<ReloadPrepared> {
    let listener = if config.listen != current_listen {
        Some(
            TcpListener::bind(config.listen)
                .await
                .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", config.listen))?,
        )
    } else {
        None
    };

    let tls = tls::TlsState::load(&config.tls.ca_cert, &config.tls.ca_key).map_err(|e| {
        anyhow::anyhow!(
            "failed to load TLS CA ({} / {}): {e}",
            config.tls.ca_cert.display(),
            config.tls.ca_key.display()
        )
    })?;
    let proxy_auth = load_proxy_auth(&config, cli_auth)?;
    store.reload(&config.store).await?;

    Ok(ReloadPrepared {
        runtime: ProxyRuntime {
            config: Arc::new(config),
            store,
            tls,
            proxy_auth,
        },
        listener,
    })
}

fn load_proxy_auth(
    config: &config::Config,
    cli_auth: Option<&config::ProxyAuthConfig>,
) -> anyhow::Result<Option<Arc<proxy::auth::ProxyAuth>>> {
    if let Some(auth) = cli_auth {
        return proxy::auth::ProxyAuth::new(auth).map(Some).map_err(|e| {
            anyhow::anyhow!("proxy auth config error: {e}")
        });
    }

    config
        .proxy_auth
        .as_ref()
        .map(|auth| {
            proxy::auth::ProxyAuth::new(auth)
                .map_err(|e| anyhow::anyhow!("proxy auth config error: {e}"))
        })
        .transpose()
}

fn start_listener(
    listener: TcpListener,
    runtime_state: Arc<RwLock<Arc<ProxyRuntime>>>,
    http_client: Arc<Client<HttpConnector, Incoming>>,
    shutdown: Arc<Notify>,
) -> ListenerHandle {
    let addr = listener
        .local_addr()
        .expect("listener should always have a local address");
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
    runtime_state: Arc<RwLock<Arc<ProxyRuntime>>>,
    http_client: Arc<Client<HttpConnector, Incoming>>,
    shutdown: Arc<Notify>,
) {
    loop {
        tokio::select! {
            _ = state.stop_accepting.notified() => break,
            _ = shutdown.notified() => break,
            result = listener.accept() => {
                let (stream, peer) = match result {
                    Ok(value) => value,
                    Err(e) => {
                        tracing::warn!("accept error on {}: {e}", state.addr);
                        continue;
                    }
                };

                let (connection_id, guard) = begin_connection(&state);
                let runtime_state = Arc::clone(&runtime_state);
                let http_client = Arc::clone(&http_client);

                let task = tokio::spawn(async move {
                    handle_connection(stream, peer, runtime_state, http_client).await;
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
    (
        id,
        ConnectionGuard {
            listener: Arc::clone(listener),
            id,
        },
    )
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

async fn handle_connection(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    runtime_state: Arc<RwLock<Arc<ProxyRuntime>>>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::JoinHandle;

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
