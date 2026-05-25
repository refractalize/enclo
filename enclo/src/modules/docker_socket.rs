use std::path::PathBuf;
use async_trait::async_trait;
use crate::{
    config::DockerSocketConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
};

pub struct DockerSocket {
    config: DockerSocketConfig,
}

impl DockerSocket {
    pub fn new(config: DockerSocketConfig) -> Self {
        Self { config }
    }

    fn resolve_socket_path(&self) -> anyhow::Result<PathBuf> {
        if let Ok(docker_host) = std::env::var("DOCKER_HOST") {
            match docker_host.strip_prefix("unix://") {
                Some(path) => return Ok(PathBuf::from(path)),
                None => anyhow::bail!(
                    "DOCKER_HOST is set to \"{}\" which is not a unix socket; \
                     only unix:// is supported",
                    docker_host
                ),
            }
        }
        Ok(PathBuf::from(&self.config.socket_path))
    }
}

#[async_trait]
impl Module for DockerSocket {
    fn name(&self) -> &str { "docker_socket" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, _ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let socket = self.resolve_socket_path()?;

        if !socket.exists() {
            anyhow::bail!("docker socket not found at {}", socket.display());
        }

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket, "/var/run/docker.sock"));
        Ok(out)
    }
}
