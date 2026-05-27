use async_trait::async_trait;
use crate::{
    config::SecretsConfig,
    context::Context,
    module::{Module, ModuleOutput, ModuleStatus, VolumeMount},
    modules::support::{self, ManagedChild},
};

const SOCKET_POLL_ATTEMPTS: u32 = 60;
const SOCKET_POLL_INTERVAL_MS: u64 = 200;

pub struct Secrets {
    config: SecretsConfig,
    child: ManagedChild,
}

impl Secrets {
    pub fn new(config: SecretsConfig) -> Self {
        Self { config, child: ManagedChild::new("enclo-secrets") }
    }
}

#[async_trait]
impl Module for Secrets {
    fn name(&self) -> &str { "secrets" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        if self.config.config.is_empty() {
            anyhow::bail!(
                "modules.secrets.config must be set to the path of your enclo-secrets config file"
            );
        }

        let socket_path = ctx.enclo_dir.join("secrets.sock");
        let binary_path = support::resolve_binary_path("enclo-secrets")?;

        tracing::info!("starting enclo-secrets");
        let child = tokio::process::Command::new(&binary_path)
            .args([
                "serve",
                "--socket", &socket_path.to_string_lossy(),
                "--config", &self.config.config,
            ])
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn enclo-secrets: {e}"))?;
        self.child.store(child).await;

        tracing::info!("waiting for enclo-secrets socket at {}", socket_path.display());
        support::wait_for_path(
            &socket_path,
            SOCKET_POLL_ATTEMPTS,
            std::time::Duration::from_millis(SOCKET_POLL_INTERVAL_MS),
            "enclo-secrets socket",
        )
        .await?;

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket_path, "/run/enclo-secrets.sock"));
        out.volumes.push(
            VolumeMount::new(&binary_path, "/usr/local/bin/enclo-secrets").with_options("ro"),
        );
        Ok(out)
    }

    fn can_reload(&self) -> bool { true }

    async fn module_status(&self) -> ModuleStatus {
        ModuleStatus { process_alive: self.child.process_alive().await }
    }

    async fn reload(&self, _ctx: &Context) -> anyhow::Result<()> {
        self.child.send_signal(libc::SIGHUP).await
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        self.child.kill().await
    }
}
