use std::path::PathBuf;
use async_trait::async_trait;
use crate::{
    config::PodmanSocketConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
};

pub struct PodmanSocket {
    config: PodmanSocketConfig,
}

impl PodmanSocket {
    pub fn new(config: PodmanSocketConfig) -> Self {
        Self { config }
    }

    fn resolve_socket_path(&self, ctx: &Context) -> anyhow::Result<PathBuf> {
        if let Ok(container_host) = std::env::var("CONTAINER_HOST") {
            match container_host.strip_prefix("unix://") {
                Some(path) => return Ok(PathBuf::from(path)),
                None => anyhow::bail!(
                    "CONTAINER_HOST is set to \"{}\" which is not a unix socket; \
                     only unix:// is supported",
                    container_host
                ),
            }
        }
        Ok(self.config.socket_path
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.runtime_dir.join("podman/podman.sock")))
    }
}

#[async_trait]
impl Module for PodmanSocket {
    fn name(&self) -> &str { "podman_socket" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let socket = self.resolve_socket_path(ctx)?;

        if !socket.exists() {
            anyhow::bail!("podman socket not found at {}", socket.display());
        }

        let mut out = ModuleOutput::default();
        out.volumes.push(VolumeMount::new(&socket, "/run/podman/podman.sock"));
        out.env_vars.push((
            "CONTAINER_HOST".into(),
            "unix:///run/podman/podman.sock".into(),
        ));
        Ok(out)
    }
}
