use std::sync::Arc;
use async_trait::async_trait;
use tokio::process::Child;
use tokio::sync::Mutex;
use crate::{
    config::SecretsConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
};

const SOCKET_POLL_ATTEMPTS: u32 = 60;
const SOCKET_POLL_INTERVAL_MS: u64 = 200;

pub struct Secrets {
    config: SecretsConfig,
    child: Arc<Mutex<Option<Child>>>,
}

impl Secrets {
    pub fn new(config: SecretsConfig) -> Self {
        Self { config, child: Arc::new(Mutex::new(None)) }
    }
}

async fn resolve_binary_path(binary: &str) -> anyhow::Result<std::path::PathBuf> {
    let out = tokio::process::Command::new("sh")
        .args(["-c", &format!("command -v {}", shell_escape(binary))])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to resolve binary path: {e}"))?;

    if !out.status.success() || out.stdout.is_empty() {
        anyhow::bail!("insectjar-secrets binary not found in PATH: {}", binary);
    }

    let path = String::from_utf8(out.stdout)
        .map_err(|e| anyhow::anyhow!("binary path is not valid UTF-8: {e}"))?;
    Ok(std::path::PathBuf::from(path.trim()))
}

fn shell_escape(s: &str) -> String {
    // Simple single-quote escape: replace ' with '\''
    format!("'{}'", s.replace('\'', r"'\''"))
}

async fn poll_socket(path: &std::path::Path) -> anyhow::Result<()> {
    for _ in 0..SOCKET_POLL_ATTEMPTS {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(SOCKET_POLL_INTERVAL_MS)).await;
    }
    anyhow::bail!(
        "insectjar-secrets socket did not appear at {} after {}ms",
        path.display(),
        SOCKET_POLL_ATTEMPTS as u64 * SOCKET_POLL_INTERVAL_MS,
    )
}

#[async_trait]
impl Module for Secrets {
    fn name(&self) -> &str { "secrets" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        if self.config.config.is_empty() {
            anyhow::bail!(
                "modules.secrets.config must be set to the path of your insectjar-secrets config file"
            );
        }

        let socket_path = ctx.insectjar_dir.join("secrets.sock");
        let binary_path = resolve_binary_path("insectjar-secrets").await?;

        tracing::info!("starting insectjar-secrets");
        let child = tokio::process::Command::new(&binary_path)
            .args([
                "serve",
                "--socket", &socket_path.to_string_lossy(),
                "--config", &self.config.config,
            ])
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn insectjar-secrets: {e}"))?;
        *self.child.lock().await = Some(child);

        tracing::info!("waiting for insectjar-secrets socket at {}", socket_path.display());
        poll_socket(&socket_path).await?;

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket_path, "/run/insectjar-secrets.sock"));
        out.volumes.push(
            VolumeMount::new(&binary_path, "/usr/local/bin/insectjar-secrets").with_options("ro"),
        );
        Ok(out)
    }

    fn can_reload(&self) -> bool { true }

    async fn reload(&self, _ctx: &Context) -> anyhow::Result<()> {
        let guard = self.child.lock().await;
        if let Some(child) = guard.as_ref() {
            let pid = child.id()
                .ok_or_else(|| anyhow::anyhow!("insectjar-secrets has no PID (already exited?)"))?;
            let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) };
            if rc != 0 {
                anyhow::bail!(
                    "failed to send SIGHUP to insectjar-secrets (pid {}): {}",
                    pid, std::io::Error::last_os_error()
                );
            }
            tracing::info!("sent SIGHUP to insectjar-secrets (pid {})", pid);
        } else {
            anyhow::bail!("insectjar-secrets is not running");
        }
        Ok(())
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.lock().await.take() {
            tracing::info!("stopping insectjar-secrets");
            let _ = child.kill().await;
        }
        Ok(())
    }
}
