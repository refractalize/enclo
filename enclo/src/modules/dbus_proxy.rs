use async_trait::async_trait;
use crate::{
    config::DbusProxyConfig,
    context::Context,
    module::{Module, ModuleOutput, ModuleStatus, VolumeMount},
    modules::support::{self, ManagedChild},
};

const CONTAINER_SOCKET_PATH: &str = "/run/dbus-proxy/bus";
const READY_POLL_ATTEMPTS: u32 = 40;
const READY_POLL_INTERVAL_MS: u64 = 250;

pub struct DbusProxy {
    config: DbusProxyConfig,
    child: ManagedChild,
}

impl DbusProxy {
    pub fn new(config: DbusProxyConfig) -> Self {
        Self {
            config,
            child: ManagedChild::new("xdg-dbus-proxy"),
        }
    }

    fn module_dir(&self, ctx: &Context) -> std::path::PathBuf {
        ctx.module_dir("dbus-proxy")
    }

    fn socket_path(&self, ctx: &Context) -> std::path::PathBuf {
        self.module_dir(ctx).join("bus")
    }
}

#[async_trait]
impl Module for DbusProxy {
    fn name(&self) -> &str { "dbus_proxy" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let host_bus = format!(
            "unix:path={}",
            ctx.support.session_bus_socket_path(&ctx.runtime_dir).display()
        );

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

        self.child.store(child).await;

        tracing::info!("waiting for xdg-dbus-proxy to be ready");
        support::wait_for_unix_socket(
            &socket_path,
            READY_POLL_ATTEMPTS,
            std::time::Duration::from_millis(READY_POLL_INTERVAL_MS),
            "xdg-dbus-proxy",
            Some(&self.child),
        )
        .await?;

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket_path, CONTAINER_SOCKET_PATH));
        let bus_addr = format!("unix:path={}", CONTAINER_SOCKET_PATH);
        out.env_vars.push(("DBUS_SESSION_BUS_ADDRESS".into(), bus_addr.clone()));
        out.exec_env_vars.push(("DBUS_SESSION_BUS_ADDRESS".into(), bus_addr));
        Ok(out)
    }

    async fn module_status(&self) -> ModuleStatus {
        ModuleStatus { process_alive: self.child.process_alive().await }
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        self.child.kill().await
    }
}
