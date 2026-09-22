use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Result;
use tokio::sync::{watch, Mutex};
use tracing::{info, warn};
#[cfg(not(target_os = "macos"))]
use zbus::connection;
use zbus::interface;

use crate::{
    config::Config,
    context::Context,
    module::Module,
    modules,
    orchestrator::Orchestrator,
};

#[derive(Debug, Clone, PartialEq)]
pub enum ContainerStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Error(String),
}

impl ContainerStatus {
    fn as_str(&self) -> &str {
        match self {
            Self::Starting => "starting",
            Self::Running  => "running",
            Self::Stopping => "stopping",
            Self::Stopped  => "stopped",
            Self::Error(_) => "error",
        }
    }
}

struct ContainerEntry {
    startup_deadline: tokio::time::Instant,
    status_tx: watch::Sender<ContainerStatus>,
    reloadable: Vec<Arc<dyn Module>>,
    all_enabled: Vec<Arc<dyn Module>>,
    /// Records the time each module was last configured: setup time initially,
    /// updated to reload time after each successful reload.
    module_timestamps: Arc<Mutex<HashMap<String, SystemTime>>>,
    ctx: Context,
    _task: tokio::task::JoinHandle<()>,
}

struct ManagerInner {
    containers: HashMap<String, ContainerEntry>,
}

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Mutex<ManagerInner>>,
    config_dir: std::path::PathBuf,
}

impl Manager {
    pub fn new(config_dir: std::path::PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ManagerInner { containers: HashMap::new() })),
            config_dir,
        }
    }

    fn resolve_config(&self, name: &str) -> std::path::PathBuf {
        self.config_dir.join("containers").join(format!("{}.toml", name))
    }

    async fn load(&self, name: &str, config_path: Option<&str>) -> Result<(Config, Context)> {
        let path = self.config_path(name, config_path);
        let config = Config::load(&path)?;
        let ctx = Context::new(config.container.name.clone())?;
        Ok((config, ctx))
    }

    fn config_path(&self, name: &str, config_path: Option<&str>) -> std::path::PathBuf {
        match config_path {
            Some(path) if !path.is_empty() => std::path::PathBuf::from(path),
            _ => self.resolve_config(name),
        }
    }

    async fn entry_snapshot(
        &self,
        name: &str,
    ) -> Option<(ContainerStatus, watch::Receiver<ContainerStatus>, tokio::time::Instant)> {
        let guard = self.inner.lock().await;
        guard.containers.get(name).map(|entry| {
            (entry.status_tx.borrow().clone(), entry.status_tx.subscribe(), entry.startup_deadline)
        })
    }

    async fn send_status(&self, name: &str, status: ContainerStatus) {
        let guard = self.inner.lock().await;
        if let Some(entry) = guard.containers.get(name) {
            let _ = entry.status_tx.send(status);
        }
    }

    async fn wait_until_removed(&self, name: &str, timeout: std::time::Duration) -> zbus::fdo::Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(zbus::fdo::Error::Failed(
                    format!("timed out waiting for container '{}' to stop before restart", name)
                ));
            }
            if !self.inner.lock().await.containers.contains_key(name) {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// Returns the podman container name for a given config stem, using the
    /// stored context if the container is tracked, otherwise loading the config.
    async fn resolve_container_name(&self, name: &str) -> Result<String> {
        if let Some(entry) = self.inner.lock().await.containers.get(name) {
            return Ok(entry.ctx.container_name.clone());
        }
        let (config, _) = self.load(name, None).await?;
        Ok(config.container.name)
    }

    /// Stop all running containers and wait for their tasks to finish teardown.
    async fn stop_all(&self) {
        let container_names: Vec<String> = self.inner.lock().await.containers.values()
            .map(|e| e.ctx.container_name.clone())
            .collect();
        for container_name in &container_names {
            let _ = crate::podman::stop(container_name).await;
        }
        // Wait for tasks to remove themselves from the map (they do so after teardown).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!("daemon: timed out waiting for containers to stop");
                break;
            }
            if self.inner.lock().await.containers.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Spawn a container task. Returns a watch receiver; the task signals
    /// Running via the oneshot then continues until the container exits.
    async fn spawn_start(&self, name: String, config_path: Option<&str>) -> Result<(watch::Receiver<ContainerStatus>, tokio::time::Instant)> {
        let (config, ctx) = self.load(&name, config_path).await?;
        let deadline = startup_deadline(config.container.startup_timeout_secs);
        let modules = modules::build_registry(&config);
        let (reloadable, all_enabled) = classify_modules(&modules);
        let orch = Orchestrator::new(modules);
        let ctx_for_entry = ctx.clone();
        let module_timestamps: Arc<Mutex<HashMap<String, SystemTime>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let module_timestamps_for_entry = Arc::clone(&module_timestamps);

        let (status_tx, rx) = watch::channel(ContainerStatus::Starting);
        let status_tx2 = status_tx.clone();
        let inner = self.inner.clone();
        let name2 = name.clone();

        // Oneshot: orchestrator fires this once the container is running
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();

        let task = tokio::spawn(async move {
            info!("daemon: starting container '{}'", name2);

            // Spawn a helper that watches the ready signal and updates status_tx
            let status_for_ready = status_tx2.clone();
            let name_for_ready = name2.clone();
            tokio::spawn(async move {
                match ready_rx.await {
                    Ok(Ok(())) => {
                        info!("daemon: container '{}' is running", name_for_ready);
                        let _ = status_for_ready.send(ContainerStatus::Running);
                    }
                    Ok(Err(e)) => {
                        warn!("daemon: container '{}' failed to start: {:#}", name_for_ready, e);
                        let _ = status_for_ready.send(ContainerStatus::Error(e.to_string()));
                    }
                    Err(_) => {} // sender dropped without sending — handled below
                }
            });

            match orch.start(&config, &ctx, Some(ready_tx), Some(module_timestamps)).await {
                Ok(()) => {
                    info!("daemon: container '{}' stopped", name2);
                    let _ = status_tx2.send(ContainerStatus::Stopped);
                }
                Err(e) => {
                    warn!("daemon: container '{}' error: {:#}", name2, e);
                    // Only update if not already set to Error by the ready handler
                    let current = status_tx2.borrow().clone();
                    if !matches!(current, ContainerStatus::Error(_)) {
                        let _ = status_tx2.send(ContainerStatus::Error(e.to_string()));
                    }
                }
            }
            inner.lock().await.containers.remove(&name2);
        });

        self.inner.lock().await.containers.insert(name.clone(), ContainerEntry {
            startup_deadline: deadline,
            status_tx,
            reloadable,
            all_enabled,
            module_timestamps: module_timestamps_for_entry,
            ctx: ctx_for_entry,
            _task: task,
        });

        Ok((rx, deadline))
    }
}

// Preserve the former 90-second allowance for setup and post-start work.
fn startup_deadline(timeout_secs: std::num::NonZeroU32) -> tokio::time::Instant {
    tokio::time::Instant::now()
        + std::time::Duration::from_secs(u64::from(timeout_secs.get()) + 90)
}

async fn wait_for_start(
    name: &str,
    mut rx: watch::Receiver<ContainerStatus>,
    deadline: tokio::time::Instant,
) -> zbus::fdo::Result<()> {
    loop {
        // Inspect the current value too: a subscriber can join after an update.
        match rx.borrow_and_update().clone() {
            ContainerStatus::Running => return Ok(()),
            ContainerStatus::Error(msg) => return Err(zbus::fdo::Error::Failed(msg)),
            ContainerStatus::Stopped => return Err(zbus::fdo::Error::Failed(
                "container exited before reaching running state".into()
            )),
            _ => {}
        }
        tokio::select! {
            result = rx.changed() => {
                if result.is_err() {
                    return Err(zbus::fdo::Error::Failed(
                        "container task exited unexpectedly".into()
                    ));
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Err(zbus::fdo::Error::Failed(
                    format!("timed out waiting for container '{}' to start", name)
                ));
            }
        }
    }
}

// FIXME: Manager methods return zbus::fdo::Result even on macOS where D-Bus is not used.
// This means error strings get the "org.freedesktop.DBus.Error.Failed:" prefix unless
// explicitly stripped (see dispatch_request). Fix by splitting this impl: have the methods
// return anyhow::Result and only wrap in zbus::fdo::Error in the Linux D-Bus path.
#[interface(name = "org.enclo.Manager")]
impl Manager {
    async fn start(&self, name: String, config_path: String) -> zbus::fdo::Result<()> {
        // If already running or starting, subscribe to the existing watch and wait.
        let existing_rx = self.entry_snapshot(&name).await;

        let (rx, deadline) = match existing_rx {
            Some((ContainerStatus::Running, _, _)) => return Ok(()),
            Some((ContainerStatus::Starting, rx, deadline)) => (rx, deadline),
            Some(_) | None => {
                let path = if config_path.is_empty() { None } else { Some(config_path.as_str()) };
                self.spawn_start(name.clone(), path).await
                    .map_err(|e| zbus::fdo::Error::Failed(format!("{:#}", e)))?
            }
        };

        wait_for_start(&name, rx, deadline).await
    }

    async fn check_user_sessions(&self, name: &str) -> zbus::fdo::Result<()> {
        let all_enabled = {
            let guard = self.inner.lock().await;
            guard.containers.get(name)
                .map(|e| e.all_enabled.clone())
                .unwrap_or_default()
        };

        let mut managed = Vec::new();
        for module in &all_enabled {
            managed.extend(module.module_status().await.managed_exec_ids);
        }

        let container_name = self.resolve_container_name(name).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{:#}", e)))?;

        let user_sessions = crate::podman::user_exec_sessions(&container_name, &managed)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{:#}", e)))?;

        if !user_sessions.is_empty() {
            return Err(zbus::fdo::Error::Failed(format!(
                "container '{}' has {} active exec session(s); use --force to stop anyway",
                name,
                user_sessions.len()
            )));
        }

        Ok(())
    }

    async fn stop(&self, name: String, force: bool) -> zbus::fdo::Result<()> {
        if !force {
            self.check_user_sessions(&name).await?;
        }

        let container_name = self.resolve_container_name(&name).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{:#}", e)))?;

        self.send_status(&name, ContainerStatus::Stopping).await;

        crate::podman::stop(&container_name).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{:#}", e)))
    }

    async fn status(&self, name: String) -> zbus::fdo::Result<String> {
        let guard = self.inner.lock().await;
        if let Some(entry) = guard.containers.get(&name) {
            Ok(entry.status_tx.borrow().as_str().to_string())
        } else {
            Ok("stopped".to_string())
        }
    }

    async fn list_containers(&self) -> zbus::fdo::Result<Vec<(String, String)>> {
        let guard = self.inner.lock().await;
        Ok(guard.containers.iter()
            .map(|(k, v)| (k.clone(), v.status_tx.borrow().as_str().to_string()))
            .collect())
    }

    async fn restart(&self, name: String, config_path: String, force: bool) -> zbus::fdo::Result<()> {
        if !force {
            self.check_user_sessions(&name).await?;
        }

        let container_name = self.resolve_container_name(&name).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{:#}", e)))?;
        self.send_status(&name, ContainerStatus::Stopping).await;
        let _ = crate::podman::stop(&container_name).await;
        self.wait_until_removed(&name, std::time::Duration::from_secs(30)).await?;

        self.start(name, config_path).await
    }

    async fn reload(&self, name: String) -> zbus::fdo::Result<()> {
        let (reloadable, ctx) = {
            let guard = self.inner.lock().await;
            let entry = guard.containers.get(&name)
                .ok_or_else(|| zbus::fdo::Error::Failed(
                    format!("container '{}' is not running", name)
                ))?;
            (entry.reloadable.clone(), entry.ctx.clone())
        };

        let module_timestamps = {
            let guard = self.inner.lock().await;
            guard.containers.get(&name).map(|e| Arc::clone(&e.module_timestamps))
        };

        let mut errors: Vec<String> = Vec::new();
        for module in &reloadable {
            match module.reload(&ctx).await {
                Ok(()) => {
                    if let Some(ts) = &module_timestamps {
                        ts.lock().await.insert(module.name().to_string(), SystemTime::now());
                    }
                }
                Err(e) => {
                    errors.push(format!("{}: {:#}", module.name(), e));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(errors.join("; ")))
        }
    }

    async fn module_statuses(&self, name: String) -> zbus::fdo::Result<Vec<(String, String, i64)>> {
        let (all_enabled, module_timestamps) = {
            let guard = self.inner.lock().await;
            let entry = guard.containers.get(&name)
                .ok_or_else(|| zbus::fdo::Error::Failed(
                    format!("container '{}' is not running", name)
                ))?;
            (entry.all_enabled.clone(), Arc::clone(&entry.module_timestamps))
        };

        let timestamps = module_timestamps.lock().await;
        let mut out = Vec::new();
        for module in &all_enabled {
            let status = module.module_status().await;
            let process_str = match status.process_alive {
                Some(true)  => "running".to_string(),
                Some(false) => "exited".to_string(),
                None        => String::new(),
            };
            let secs = timestamps.get(module.name())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(-1);
            out.push((module.name().to_string(), process_str, secs));
        }
        Ok(out)
    }

    async fn user_exec_count(&self, name: String) -> zbus::fdo::Result<u32> {
        let all_enabled = {
            let guard = self.inner.lock().await;
            guard.containers.get(&name)
                .map(|e| e.all_enabled.clone())
                .unwrap_or_default()
        };

        let mut managed = Vec::new();
        for module in &all_enabled {
            managed.extend(module.module_status().await.managed_exec_ids);
        }

        let ctx = Context::new(name.clone())
            .map_err(|e| zbus::fdo::Error::Failed(format!("{:#}", e)))?;

        let sessions = crate::podman::user_exec_sessions(&ctx.container_name, &managed)
            .await
            .unwrap_or_default();

        Ok(sessions.len() as u32)
    }

    async fn ensure_running(&self, name: String, config_path: String) -> zbus::fdo::Result<()> {
        let already_running = {
            let guard = self.inner.lock().await;
            guard.containers.get(&name)
                .map(|e| *e.status_tx.borrow() == ContainerStatus::Running)
                .unwrap_or(false)
        };
        if already_running {
            return Ok(());
        }
        self.start(name, config_path).await
    }
}

#[cfg(not(target_os = "macos"))]
pub async fn run_daemon(config_dir: std::path::PathBuf) -> Result<()> {
    info!("daemon: starting on session bus as org.enclo");
    let manager = Manager::new(config_dir);

    let _conn = connection::Builder::session()?
        .name("org.enclo")?
        .serve_at("/org/enclo", manager.clone())?
        .build()
        .await?;

    info!("daemon: ready");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }

    info!("daemon: shutting down");
    manager.stop_all().await;
    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn run_daemon(config_dir: std::path::PathBuf) -> Result<()> {
    use tokio::net::UnixListener;

    let socket_path = crate::platform::daemon_socket();
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if socket_path.exists() {
        tokio::fs::remove_file(&socket_path).await?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    let manager = Manager::new(config_dir);
    info!("daemon: listening on {}", socket_path.display());
    info!("daemon: ready");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let mgr = manager.clone();
                        tokio::spawn(async move { handle_connection(stream, mgr).await; });
                    }
                    Err(e) => tracing::error!("daemon: accept error: {e}"),
                }
            }
        }
    }

    info!("daemon: shutting down");
    manager.stop_all().await;
    Ok(())
}

#[cfg(target_os = "macos")]
async fn handle_connection(stream: tokio::net::UnixStream, manager: Manager) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    if reader.read_line(&mut line).await.is_err() || line.is_empty() {
        return;
    }

    let resp = match serde_json::from_str::<crate::ipc::Request>(line.trim()) {
        Ok(req) => dispatch_request(&manager, req).await,
        Err(e) => crate::ipc::Response::error(format!("invalid request: {e}")),
    };

    let _ = write_half.write_all(
        (serde_json::to_string(&resp).unwrap_or_default() + "\n").as_bytes(),
    ).await;
}

#[cfg(target_os = "macos")]
async fn dispatch_request(manager: &Manager, req: crate::ipc::Request) -> crate::ipc::Response {
    use crate::ipc::{Request, Response};
    fn e(err: zbus::fdo::Error) -> Response {
        let msg = match err {
            zbus::fdo::Error::Failed(s) => s,
            other => other.to_string(),
        };
        Response::error(msg)
    }
    match req {
        Request::Start { name, config_path } =>
            match manager.start(name, config_path).await { Ok(()) => Response::ok(), Err(err) => e(err) },
        Request::Stop { name, force } =>
            match manager.stop(name, force).await { Ok(()) => Response::ok(), Err(err) => e(err) },
        Request::Status { name } =>
            match manager.status(name).await { Ok(v) => Response::ok_value(v), Err(err) => e(err) },
        Request::ListContainers =>
            match manager.list_containers().await { Ok(v) => Response::ok_value(v), Err(err) => e(err) },
        Request::Restart { name, config_path, force } =>
            match manager.restart(name, config_path, force).await { Ok(()) => Response::ok(), Err(err) => e(err) },
        Request::EnsureRunning { name, config_path } =>
            match manager.ensure_running(name, config_path).await { Ok(()) => Response::ok(), Err(err) => e(err) },
        Request::Reload { name } =>
            match manager.reload(name).await { Ok(()) => Response::ok(), Err(err) => e(err) },
        Request::ModuleStatuses { name } =>
            match manager.module_statuses(name).await { Ok(v) => Response::ok_value(v), Err(err) => e(err) },
        Request::UserExecCount { name } =>
            match manager.user_exec_count(name).await { Ok(v) => Response::ok_value(v), Err(err) => e(err) },
    }
}

fn classify_modules(modules: &[Arc<dyn Module>]) -> (Vec<Arc<dyn Module>>, Vec<Arc<dyn Module>>) {
    let all_enabled: Vec<Arc<dyn Module>> = modules
        .iter()
        .filter(|module| module.enabled())
        .map(Arc::clone)
        .collect();
    let reloadable = all_enabled
        .iter()
        .filter(|module| module.can_reload())
        .map(Arc::clone)
        .collect();
    (reloadable, all_enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{num::NonZeroU32, time::Duration};

    #[tokio::test(start_paused = true)]
    async fn custom_startup_wait_exceeds_old_daemon_limit() {
        let deadline = startup_deadline(NonZeroU32::new(180).unwrap());
        let (tx, rx) = watch::channel(ContainerStatus::Starting);
        let update = async {
            tokio::time::sleep(Duration::from_secs(150)).await;
            tx.send(ContainerStatus::Running).unwrap();
        };
        let (result, ()) = tokio::join!(wait_for_start("test", rx, deadline), update);
        result.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn joining_startup_preserves_original_deadline() {
        let start = tokio::time::Instant::now();
        let deadline = startup_deadline(NonZeroU32::new(60).unwrap());
        let (tx, _rx) = watch::channel(ContainerStatus::Starting);
        tokio::time::sleep(Duration::from_secs(100)).await;
        let error = wait_for_start("test", tx.subscribe(), deadline).await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(start.elapsed(), Duration::from_secs(150));
    }

    #[tokio::test(start_paused = true)]
    async fn startup_observes_current_status_and_errors() {
        let deadline = startup_deadline(NonZeroU32::new(60).unwrap());
        let (tx, rx) = watch::channel(ContainerStatus::Running);
        wait_for_start("test", rx, deadline).await.unwrap();
        tx.send_replace(ContainerStatus::Error("startup failed".into()));
        let error = wait_for_start("test", tx.subscribe(), deadline).await.unwrap_err();
        assert!(error.to_string().contains("startup failed"));
    }
}
