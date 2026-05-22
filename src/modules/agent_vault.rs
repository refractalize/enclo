use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::process::Child;
use crate::{
    config::AgentVaultConfig,
    context::Context,
    module::{Module, ModuleOutput, PostStartCmd, VolumeMount},
};

const POLL_ATTEMPTS: u32 = 60;
const POLL_INTERVAL_MS: u64 = 500;

pub struct AgentVault {
    config: AgentVaultConfig,
    child: Arc<Mutex<Option<Child>>>,
}

impl AgentVault {
    pub fn new(config: AgentVaultConfig) -> Self {
        Self { config, child: Arc::new(Mutex::new(None)) }
    }

    fn module_dir(&self, ctx: &Context) -> std::path::PathBuf {
        ctx.module_dir("agent-vault")
    }

    fn ca_cert_path(&self, ctx: &Context) -> std::path::PathBuf {
        self.module_dir(ctx).join("ca.pem")
    }

    fn env_file_path(&self, ctx: &Context) -> std::path::PathBuf {
        self.module_dir(ctx).join("agent-vault.env")
    }

    fn container_name(&self, ctx: &Context) -> String {
        format!("safebox-{}-agent-vault", ctx.container_name)
    }

    async fn run_command(label: &str, cmd: &str) -> anyhow::Result<String> {
        let out = tokio::process::Command::new("sh")
            .args(["-c", cmd])
            .output()
            .await?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("{} failed: {}", label, stderr.trim());
        }

        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    }

    async fn fetch_master_password(&self) -> anyhow::Result<String> {
        let cmd = self.config.master_password_command.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "agent_vault.master_password_command is required but not set in config"
            )
        })?;
        Self::run_command("master_password_command", cmd).await
    }

    async fn fetch_token(&self) -> anyhow::Result<Option<String>> {
        match self.config.token_command.as_deref() {
            Some(cmd) => Self::run_command("token_command", cmd).await.map(Some),
            None => Ok(None),
        }
    }

    fn write_env_file(&self, ctx: &Context, password: &str) -> anyhow::Result<()> {
        let path = self.env_file_path(ctx);
        let content = format!("AGENT_VAULT_MASTER_PASSWORD={}\n", password);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }

    async fn spawn_container(&self, ctx: &Context) -> anyhow::Result<()> {
        let container_name = self.container_name(ctx);
        crate::podman::rm_if_exists(&container_name).await?;

        let env_file = self.env_file_path(ctx);
        let mut args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--name".to_string(), container_name,
            "--network".to_string(), "bridge".to_string(),
            "--env-file".to_string(), env_file.to_string_lossy().into_owned(),
            "--volume".to_string(), format!("{}:/data", self.config.data_volume),
        ];
        for port in &self.config.ports {
            args.push("--publish".to_string());
            args.push(port.clone());
        }
        for env in &self.config.env {
            args.push("--env".to_string());
            args.push(env.clone());
        }
        args.push(self.config.image.clone());

        let child = tokio::process::Command::new("podman")
            .args(&args)
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn agent-vault container: {}", e))?;

        *self.child.lock().await = Some(child);
        Ok(())
    }

    async fn poll_ready(&self, client: &reqwest::Client) -> anyhow::Result<()> {
        let health_url = format!("{}/health", self.config.api_url);
        for _ in 0..POLL_ATTEMPTS {
            if let Some(child) = self.child.lock().await.as_mut() {
                if let Some(status) = child.try_wait()? {
                    anyhow::bail!("agent-vault container exited unexpectedly with {}", status);
                }
            }
            if health_ok(client, &health_url).await {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
        anyhow::bail!("agent-vault did not become ready at {}/health", self.config.api_url)
    }

    async fn fetch_ca_cert(&self, ctx: &Context, client: &reqwest::Client) -> anyhow::Result<std::path::PathBuf> {
        let ca_url = format!("{}/v1/mitm/ca.pem", self.config.api_url);
        let response = client.get(&ca_url).send().await
            .map_err(|e| anyhow::anyhow!("failed to fetch CA cert from {}: {}", ca_url, e))?;

        if !response.status().is_success() {
            anyhow::bail!("failed to fetch CA cert from {}: {}", ca_url, response.status());
        }

        let bytes = response.bytes().await?;
        let path = self.ca_cert_path(ctx);
        tokio::fs::write(&path, &bytes).await?;
        Ok(path)
    }
}

fn build_proxy_url(base_url: &str, token: Option<&str>) -> anyhow::Result<String> {
    let Some(token) = token else {
        return Ok(base_url.to_string());
    };
    // Insert "token:@" after "://" to produce https://token:@host:port
    let pos = base_url.find("://")
        .ok_or_else(|| anyhow::anyhow!("invalid proxy_url '{}': no scheme", base_url))?;
    Ok(format!("{}://{}:@{}", &base_url[..pos], token, &base_url[pos + 3..]))
}

async fn health_ok(client: &reqwest::Client, url: &str) -> bool {
    client.get(url)
        .timeout(std::time::Duration::from_secs(1))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

#[async_trait]
impl Module for AgentVault {
    fn name(&self) -> &str { "agent_vault" }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        tokio::fs::create_dir_all(self.module_dir(ctx)).await?;
        let client = reqwest::Client::new();

        tracing::info!("fetching agent-vault master password");
        let password = self.fetch_master_password().await?;
        tracing::info!("fetching agent-vault token");
        let token = self.fetch_token().await?;
        self.write_env_file(ctx, &password)?;

        tracing::info!("starting agent-vault container");
        self.spawn_container(ctx).await?;

        tracing::info!("waiting for agent-vault to be ready");
        self.poll_ready(&client).await?;
        let _ = tokio::fs::remove_file(self.env_file_path(ctx)).await;

        let ca_path = self.fetch_ca_cert(ctx, &client).await?;
        tracing::info!("CA cert written to {}", ca_path.display());

        let proxy_url = build_proxy_url(&self.config.proxy_url, token.as_deref())?;

        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&ca_path, "/etc/pki/ca-trust/source/anchors/agent-vault-ca.pem")
                .with_options("ro"),
        );
        out.post_start_cmds.push(PostStartCmd {
            args: vec!["update-ca-trust".into()],
            user: Some("root".into()),
        });
        out.env_vars.push(("HTTP_PROXY".into(), proxy_url.clone()));
        out.env_vars.push(("HTTPS_PROXY".into(), proxy_url.clone()));
        out.exec_env_vars.push(("HTTP_PROXY".into(), proxy_url.clone()));
        out.exec_env_vars.push(("HTTPS_PROXY".into(), proxy_url));
        Ok(out)
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.lock().await.take() {
            tracing::info!("stopping agent-vault container");
            let _ = child.kill().await;
        }
        Ok(())
    }
}
