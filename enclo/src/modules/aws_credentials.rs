use async_trait::async_trait;
use crate::{
    config::AwsCredentialsConfig,
    context::Context,
    module::{Module, ModuleOutput, VolumeMount},
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
        let binary_path = resolve_binary_path("enclo-aws-credentials").await?;
        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&binary_path, "/usr/local/bin/enclo-aws-credentials")
                .with_options("ro"),
        );
        Ok(out)
    }
}

async fn resolve_binary_path(binary: &str) -> anyhow::Result<std::path::PathBuf> {
    let out = tokio::process::Command::new("sh")
        .args(["-c", &format!("command -v '{}'", binary.replace('\'', r"'\''"))])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to resolve binary path: {e}"))?;

    if !out.status.success() || out.stdout.is_empty() {
        anyhow::bail!("{binary} not found in PATH");
    }

    let path = String::from_utf8(out.stdout)
        .map_err(|e| anyhow::anyhow!("binary path is not valid UTF-8: {e}"))?;
    Ok(std::path::PathBuf::from(path.trim()))
}
