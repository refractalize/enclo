use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{watch, Mutex};
use tracing::{info, warn};
use zbus::{connection, interface};

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
    status_tx: watch::Sender<ContainerStatus>,
    reloadable: Vec<Arc<dyn Module>>,
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
        self.config_dir.join(format!("{}.toml", name))
    }

    async fn load(&self, name: &str, config_path: Option<&str>) -> Result<(Config, Context)> {
        let path = match config_path {
            Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
            _ => self.resolve_config(name),
        };
        let config = Config::load(&path)?;
        let ctx = Context::new(config.container.name.clone())?;
        Ok((config, ctx))
    }

    /// Stop all running containers and wait for their tasks to finish teardown.
    async fn stop_all(&self) {
        let names: Vec<String> = self.inner.lock().await.containers.keys().cloned().collect();
        for name in &names {
            if let Ok(ctx) = Context::new(name.clone()) {
                let _ = crate::podman::stop(&ctx.container_name).await;
            }
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
    async fn spawn_start(&self, name: String, config_path: Option<&str>) -> Result<watch::Receiver<ContainerStatus>> {
        let (config, ctx) = self.load(&name, config_path).await?;
        let modules = modules::build_registry(&config.modules);
        let reloadable: Vec<Arc<dyn Module>> = modules.iter()
            .filter(|m| m.enabled() && m.can_reload())
            .map(Arc::clone)
            .collect();
        let orch = Orchestrator::new(modules);
        let ctx_for_entry = ctx.clone();

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

            match orch.start(&config, &ctx, Some(ready_tx)).await {
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
            status_tx,
            reloadable,
            ctx: ctx_for_entry,
            _task: task,
        });

        Ok(rx)
    }
}

#[interface(name = "org.insectjar.Manager")]
impl Manager {
    async fn start(&self, name: String, config_path: String) -> zbus::fdo::Result<()> {
        // If already running or starting, subscribe to the existing watch and wait.
        let existing_rx = {
            let guard = self.inner.lock().await;
            guard.containers.get(&name).map(|entry| {
                (entry.status_tx.borrow().clone(), entry.status_tx.subscribe())
            })
        };

        let mut rx = match existing_rx {
            Some((ContainerStatus::Running, _)) => return Ok(()),
            Some((ContainerStatus::Starting, rx)) => rx,
            Some(_) | None => {
                let path = if config_path.is_empty() { None } else { Some(config_path.as_str()) };
                self.spawn_start(name.clone(), path).await
                    .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            }
        };

        // Wait until Running or Error (up to 120 s)
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            tokio::select! {
                result = rx.changed() => {
                    if result.is_err() {
                        // Watch sender dropped — task finished
                        return Err(zbus::fdo::Error::Failed(
                            "container task exited unexpectedly".into()
                        ));
                    }
                    match rx.borrow().clone() {
                        ContainerStatus::Running => return Ok(()),
                        ContainerStatus::Error(msg) => return Err(zbus::fdo::Error::Failed(msg)),
                        ContainerStatus::Stopped => return Err(zbus::fdo::Error::Failed(
                            "container exited before reaching running state".into()
                        )),
                        _ => continue,
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

    async fn stop(&self, name: String) -> zbus::fdo::Result<()> {
        let ctx = Context::new(name.clone())
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        // Mark as stopping
        {
            let guard = self.inner.lock().await;
            if let Some(entry) = guard.containers.get(&name) {
                let _ = entry.status_tx.send(ContainerStatus::Stopping);
            }
        }

        crate::podman::stop(&ctx.container_name).await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
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

    async fn restart(&self, name: String, config_path: String) -> zbus::fdo::Result<()> {
        // Stop the container if it's running, ignoring errors (it may already be stopped).
        let ctx = Context::new(name.clone())
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        {
            let guard = self.inner.lock().await;
            if let Some(entry) = guard.containers.get(&name) {
                let _ = entry.status_tx.send(ContainerStatus::Stopping);
            }
        }
        let _ = crate::podman::stop(&ctx.container_name).await;

        // Wait for the task to finish removing itself from the map.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(zbus::fdo::Error::Failed(
                    format!("timed out waiting for container '{}' to stop before restart", name)
                ));
            }
            let still_running = self.inner.lock().await.containers.contains_key(&name);
            if !still_running { break; }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

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

        let mut errors: Vec<String> = Vec::new();
        for module in &reloadable {
            if let Err(e) = module.reload(&ctx).await {
                errors.push(format!("{}: {:#}", module.name(), e));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(errors.join("; ")))
        }
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

pub async fn run_daemon(config_dir: std::path::PathBuf) -> Result<()> {
    info!("daemon: starting on session bus as org.insectjar");
    let manager = Manager::new(config_dir);

    let _conn = connection::Builder::session()?
        .name("org.insectjar")?
        .serve_at("/org/insectjar", manager.clone())?
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
