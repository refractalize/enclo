use std::net::SocketAddr;
use async_trait::async_trait;
use serde::Deserialize;
use crate::{
    config::{EncloProxyConfig, ProxyMode},
    context::Context,
    module::{Module, ModuleOutput, ModuleStatus, PostStartCmd, VolumeMount},
    modules::support::{self, ManagedChild},
};

const POLL_ATTEMPTS: u32 = 60;
const POLL_INTERVAL_MS: u64 = 500;
const CONTAINER_PROXY_HOST: &str = "host.containers.internal";

pub struct EncloProxy {
    config: EncloProxyConfig,
    child: ManagedChild,
}

impl EncloProxy {
    pub fn new(config: EncloProxyConfig) -> Self {
        Self { config, child: ManagedChild::new("enclo-secrets") }
    }
}

/// Minimal TOML wrapper to extract the fields we need from the unified config file.
#[derive(Deserialize)]
struct ProxyConfigPreview {
    proxy: ProxySection,
}

#[derive(Deserialize)]
struct ProxySection {
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
        .args(["generate-basic-auth", "enclo"])
        .env("ENCLO_PROXY_PASSWORD", password)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run enclo-secrets generate-basic-auth: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("generate-basic-auth failed: {}", stderr.trim());
    }

    // Output is TOML like:
    //   [proxy.auth]
    //   username      = "enclo"
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

#[async_trait]
impl Module for EncloProxy {
    fn name(&self) -> &str { "proxy" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        if self.config.config.is_empty() {
            anyhow::bail!("modules.proxy.config must be set to the path of your enclo-secrets config file");
        }

        tokio::fs::create_dir_all(ctx.module_dir("proxy")).await?;

        let config_path = std::path::Path::new(&self.config.config);
        let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));

        let raw = tokio::fs::read_to_string(config_path).await
            .map_err(|e| anyhow::anyhow!("failed to read proxy config {}: {e}", self.config.config))?;
        let preview: ProxyConfigPreview = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("failed to parse proxy config {}: {e}", self.config.config))?;
        let listen_addr = preview.proxy.listen;
        let ca_cert = if preview.proxy.tls.ca_cert.is_absolute() {
            preview.proxy.tls.ca_cert
        } else {
            config_dir.join(&preview.proxy.tls.ca_cert)
        };

        let password = random_password();

        tracing::info!("generating enclo-secrets basic auth credentials");
        let password_hash = generate_password_hash("enclo-secrets", &password).await?;

        tracing::info!("starting enclo-secrets");
        let child = tokio::process::Command::new("enclo-secrets")
            .args([
                "serve",
                "--config", &self.config.config,
                "--username", "enclo",
                "--password-hash", &password_hash,
            ])
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn enclo-secrets: {e}"))?;
        self.child.store(child).await;

        tracing::info!("waiting for enclo-secrets to be ready on {listen_addr}");
        support::wait_for_tcp(
            listen_addr,
            POLL_ATTEMPTS,
            std::time::Duration::from_millis(POLL_INTERVAL_MS),
            "enclo-secrets",
        )
        .await?;

        let proxy_url = format!(
            "http://enclo:{password}@{}:{}",
            CONTAINER_PROXY_HOST,
            listen_addr.port(),
        );

        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&ca_cert, "/etc/pki/ca-trust/source/anchors/enclo-secrets-ca.pem")
                .with_options("ro"),
        );
        out.post_start_cmds.push(PostStartCmd {
            args: vec!["update-ca-trust".into()],
            user: Some("root".into()),
        });

        match self.config.mode {
            ProxyMode::Implicit => {
                for key in &["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                    out.env_vars.push(((*key).into(), proxy_url.clone()));
                    out.exec_env_vars.push(((*key).into(), proxy_url.clone()));
                }
            }
            ProxyMode::Explicit => {
                out.env_vars.push(("INSECTJAR_PROXY".into(), proxy_url.clone()));
                out.exec_env_vars.push(("INSECTJAR_PROXY".into(), proxy_url));
            }
        }

        Ok(out)
    }

    fn can_reload(&self) -> bool { true }

    async fn module_status(&self) -> ModuleStatus {
        ModuleStatus { process_alive: self.child.process_alive().await, managed_exec_ids: Vec::new() }
    }

    async fn reload(&self, _ctx: &Context) -> anyhow::Result<()> {
        self.child.send_signal(libc::SIGHUP).await
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        self.child.kill().await
    }
}
