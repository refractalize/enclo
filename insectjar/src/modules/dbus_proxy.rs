use std::sync::Arc;
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::process::Child;
use crate::{
    config::DbusProxyConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
};

const CONTAINER_SOCKET_PATH: &str = "/run/dbus-proxy/bus";
const READY_POLL_ATTEMPTS: u32 = 40;
const READY_POLL_INTERVAL_MS: u64 = 250;

pub struct DbusProxy {
    config: DbusProxyConfig,
    child: Arc<Mutex<Option<Child>>>,
}

impl DbusProxy {
    pub fn new(config: DbusProxyConfig) -> Self {
        Self {
            config,
            child: Arc::new(Mutex::new(None)),
        }
    }

    fn module_dir(&self, ctx: &Context) -> std::path::PathBuf {
        ctx.module_dir("dbus-proxy")
    }

    fn socket_path(&self, ctx: &Context) -> std::path::PathBuf {
        self.module_dir(ctx).join("bus")
    }
}

async fn wait_for_socket(path: &std::path::Path, child: &Arc<Mutex<Option<Child>>>) -> anyhow::Result<()> {
    for _ in 0..READY_POLL_ATTEMPTS {
        if let Some(c) = child.lock().await.as_mut() {
            if let Some(status) = c.try_wait()? {
                anyhow::bail!("xdg-dbus-proxy exited unexpectedly with {}", status);
            }
        }
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(READY_POLL_INTERVAL_MS)).await;
    }
    anyhow::bail!(
        "xdg-dbus-proxy socket never became connectable at {} after {}ms",
        path.display(),
        READY_POLL_ATTEMPTS * READY_POLL_INTERVAL_MS as u32,
    )
}

#[async_trait]
impl Module for DbusProxy {
    fn name(&self) -> &str { "dbus_proxy" }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let host_bus = format!("unix:path={}", ctx.runtime_dir.join("bus").display());

        tokio::fs::create_dir_all(self.module_dir(ctx)).await?;
        let socket_path = self.socket_path(ctx);
        if socket_path.exists() {
            tokio::fs::remove_file(&socket_path).await?;
        }

        let mut args = vec![
            host_bus,
            socket_path.to_string_lossy().into_owned(),
            "--filter".to_string(),
        ];
        for name in &self.config.allowed_interfaces {
            args.push(format!("--talk={}", name));
        }
        for rule in &self.config.call_rules {
            args.push(format!("--call={}", rule));
        }
        for rule in &self.config.broadcast_rules {
            args.push(format!("--broadcast={}", rule));
        }

        tracing::info!("spawning xdg-dbus-proxy");
        let child = tokio::process::Command::new("xdg-dbus-proxy")
            .args(&args)
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn xdg-dbus-proxy: {}", e))?;

        *self.child.lock().await = Some(child);

        tracing::info!("waiting for xdg-dbus-proxy to be ready");
        wait_for_socket(&socket_path, &self.child).await?;

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket_path, CONTAINER_SOCKET_PATH));
        let bus_addr = format!("unix:path={}", CONTAINER_SOCKET_PATH);
        out.env_vars.push(("DBUS_SESSION_BUS_ADDRESS".into(), bus_addr.clone()));
        out.exec_env_vars.push(("DBUS_SESSION_BUS_ADDRESS".into(), bus_addr));
        Ok(out)
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.lock().await.take() {
            tracing::info!("killing xdg-dbus-proxy");
            let _ = child.kill().await;
        }
        Ok(())
    }
}
