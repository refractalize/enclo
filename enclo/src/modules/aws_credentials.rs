use async_trait::async_trait;
use crate::{
    config::AwsCredentialsConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
    modules::support,
};

pub struct AwsCredentials {
    config: AwsCredentialsConfig,
}

impl AwsCredentials {
    pub fn new(config: AwsCredentialsConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Module for AwsCredentials {
    fn name(&self) -> &str { "aws-credentials" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, _ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let binary_path = support::resolve_container_binary_path("enclo-aws-credentials")?;
        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&binary_path, "/usr/local/bin/enclo-aws-credentials")
                .with_options("ro"),
        );
        Ok(out)
    }
}
