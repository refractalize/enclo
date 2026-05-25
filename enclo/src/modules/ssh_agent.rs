use std::path::PathBuf;
use async_trait::async_trait;
use crate::{
    config::{SshAgentConfig, SshBackend},
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
    modules::support,
};

const CONTAINER_SOCKET_PATH: &str = "/run/ssh-agent.sock";

pub struct SshAgent {
    config: SshAgentConfig,
}

impl SshAgent {
    pub fn new(config: SshAgentConfig) -> Self {
        Self { config }
    }

    fn host_socket_path(&self, ctx: &Context) -> anyhow::Result<PathBuf> {
        if let Some(path) = &self.config.socket_path {
            return Ok(PathBuf::from(path));
        }
        match self.config.backend {
            SshBackend::GpgAgent => {
                Ok(ctx.support.default_gpg_agent_ssh_socket_path(&ctx.runtime_dir))
            }
            SshBackend::Standalone => {
                let val = std::env::var("SSH_AUTH_SOCK").map_err(|_| {
                    anyhow::anyhow!(
                        "ssh_agent backend is 'standalone' but SSH_AUTH_SOCK is not set \
                         and no socket_path is configured"
                    )
                })?;
                Ok(PathBuf::from(val))
            }
        }
    }
}

#[async_trait]
impl Module for SshAgent {
    fn name(&self) -> &str { "ssh_agent" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let socket = self.host_socket_path(ctx)?;

        if !socket.exists() {
            anyhow::bail!(
                "SSH agent socket not found at {}; ensure the agent is running",
                socket.display()
            );
        }

        support::ensure_unix_socket_connectable(&socket, "SSH agent socket").await?;

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket, CONTAINER_SOCKET_PATH));
        out.env_vars.push(("SSH_AUTH_SOCK".into(), CONTAINER_SOCKET_PATH.into()));
        out.exec_env_vars.push(("SSH_AUTH_SOCK".into(), CONTAINER_SOCKET_PATH.into()));
        Ok(out)
    }
}
