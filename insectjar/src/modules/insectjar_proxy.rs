use std::net::SocketAddr;
use std::sync::Arc;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::sync::Mutex;
use crate::{
    config::InsectjarProxyConfig,
    context::Context,
    module::{Module, ModuleOutput, PostStartCmd, VolumeMount},
};

const POLL_ATTEMPTS: u32 = 60;
const POLL_INTERVAL_MS: u64 = 500;

pub struct InsectjarProxy {
    config: InsectjarProxyConfig,
    child: Arc<Mutex<Option<Child>>>,
}

impl InsectjarProxy {
    pub fn new(config: InsectjarProxyConfig) -> Self {
        Self { config, child: Arc::new(Mutex::new(None)) }
    }
}

/// Minimal TOML wrapper to extract the fields we need from a proxy config file.
#[derive(Deserialize)]
struct ProxyConfigPreview {
    listen: SocketAddr,
    tls: ProxyTlsPreview,
}

#[derive(Deserialize)]
struct ProxyTlsPreview {
    ca_cert: std::path::PathBuf,
}

fn random_password() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn generate_password_hash(binary: &str, password: &str) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(binary)
        .args(["generate-basic-auth", "insectjar"])
        .env("INSECTJAR_PROXY_PASSWORD", password)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run insectjar-proxy generate-basic-auth: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("generate-basic-auth failed: {}", stderr.trim());
    }

    // Output is TOML like:
    //   [proxy_auth]
    //   username      = "insectjar"
    //   password_hash = "$argon2id$..."
    let stdout = String::from_utf8(out.stdout)?;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("password_hash") {
            let hash = rest
                .trim_start_matches(|c: char| c == ' ' || c == '=')
                .trim_matches('"');
            return Ok(hash.to_string());
        }
    }
    anyhow::bail!("generate-basic-auth output did not contain password_hash")
}

async fn poll_ready(addr: SocketAddr) -> anyhow::Result<()> {
    for _ in 0..POLL_ATTEMPTS {
        if TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
    anyhow::bail!("insectjar-proxy did not become ready at {addr}")
}

#[async_trait]
impl Module for InsectjarProxy {
    fn name(&self) -> &str { "proxy" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        if self.config.config.is_empty() {
            anyhow::bail!("modules.insectjar_proxy.config must be set to the path of your insectjar-proxy config file");
        }

        tokio::fs::create_dir_all(ctx.module_dir("proxy")).await?;

        let config_path = std::path::Path::new(&self.config.config);
        let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));

        let raw = tokio::fs::read_to_string(config_path).await
            .map_err(|e| anyhow::anyhow!("failed to read proxy config {}: {e}", self.config.config))?;
        let preview: ProxyConfigPreview = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("failed to parse proxy config {}: {e}", self.config.config))?;
        let listen_addr = preview.listen;
        let ca_cert = if preview.tls.ca_cert.is_absolute() {
            preview.tls.ca_cert
        } else {
            config_dir.join(&preview.tls.ca_cert)
        };

        let password = random_password();

        tracing::info!("generating insectjar-proxy basic auth credentials");
        let password_hash = generate_password_hash(&self.config.binary, &password).await?;

        tracing::info!("starting insectjar-proxy");
        let child = tokio::process::Command::new(&self.config.binary)
            .args([
                "serve",
                "--config", &self.config.config,
                "--username", "insectjar",
                "--password-hash", &password_hash,
            ])
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn insectjar-proxy: {e}"))?;
        *self.child.lock().await = Some(child);

        tracing::info!("waiting for insectjar-proxy to be ready on {listen_addr}");
        poll_ready(listen_addr).await?;

        let proxy_url = format!("http://insectjar:{password}@{listen_addr}");

        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&ca_cert, "/etc/pki/ca-trust/source/anchors/insectjar-proxy-ca.pem")
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
            tracing::info!("stopping insectjar-proxy");
            let _ = child.kill().await;
        }
        Ok(())
    }
}
