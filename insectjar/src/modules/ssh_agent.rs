use std::path::PathBuf;
use async_trait::async_trait;
use crate::{
    config::{SshAgentConfig, SshBackend},
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
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
                Ok(ctx.runtime_dir.join("gnupg/S.gpg-agent.ssh"))
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

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let socket = self.host_socket_path(ctx)?;

        if !socket.exists() {
            anyhow::bail!(
                "SSH agent socket not found at {}; ensure the agent is running",
                socket.display()
            );
        }

        tokio::net::UnixStream::connect(&socket).await.map_err(|e| {
            anyhow::anyhow!(
                "SSH agent socket exists at {} but is not connectable: {}",
                socket.display(), e
            )
        })?;

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket, CONTAINER_SOCKET_PATH));
        out.env_vars.push(("SSH_AUTH_SOCK".into(), CONTAINER_SOCKET_PATH.into()));
        out.exec_env_vars.push(("SSH_AUTH_SOCK".into(), CONTAINER_SOCKET_PATH.into()));
        Ok(out)
    }
}
