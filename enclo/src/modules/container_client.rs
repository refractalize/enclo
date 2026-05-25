use async_trait::async_trait;
use crate::{
    config::ContainerClientConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
};

pub struct ContainerClient {
    config: ContainerClientConfig,
}

impl ContainerClient {
    pub fn new(config: ContainerClientConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Module for ContainerClient {
    fn name(&self) -> &str { "container_client" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, _ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let binary_path = resolve_binary_path("enclo-container-client")?;
        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&binary_path, "/usr/local/bin/enclo").with_options("ro"),
        );
        Ok(out)
    }
}

fn resolve_binary_path(binary: &str) -> anyhow::Result<std::path::PathBuf> {
    // Prefer a sibling of the running executable — works without a separate
    // install step when both binaries are built to the same directory.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    // Fall back to PATH lookup.
    let path = which::which(binary)
        .map_err(|_| anyhow::anyhow!("{binary} not found (checked sibling directory and $PATH)"))?;
    Ok(path)
}
