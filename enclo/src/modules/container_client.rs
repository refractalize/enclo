use async_trait::async_trait;
use crate::{
    config::ContainerClientConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
    modules::support,
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
        let binary_path = support::resolve_container_binary_path("enclo-container-client")?;
        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&binary_path, "/usr/local/bin/enclo").with_options("ro"),
        );
        Ok(out)
    }
}
