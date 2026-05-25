use async_trait::async_trait;
use crate::{
    config::NvidiaConfig,
    context::{Context, Platform},
    module::{Module, ModuleOutput},
};

pub struct Nvidia {
    config: NvidiaConfig,
}

impl Nvidia {
    pub fn new(config: NvidiaConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Module for Nvidia {
    fn name(&self) -> &str { "nvidia" }
    fn enabled(&self) -> bool { self.config.enabled }

    fn supported_platforms(&self) -> &[Platform] {
        &[Platform::Linux]
    }

    async fn setup(&self, _ctx: &Context) -> anyhow::Result<ModuleOutput> {
        if !std::path::Path::new("/dev/nvidia0").exists() {
            anyhow::bail!("nvidia device not found at /dev/nvidia0");
        }

        let mut out = ModuleOutput::default();
        out.podman_args.push("--device=nvidia.com/gpu=all".into());
        Ok(out)
    }
}
