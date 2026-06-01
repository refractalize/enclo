use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::{process::Command, sync::Notify};

use crate::{
    config::{ProxyMode, SecretsConfig},
    context::Context,
    module::{Module, ModuleOutput, ModuleStatus, PostStartCmd, VolumeMount},
    modules::support::{self, ManagedChild},
    platform::Platform,
};

const SOCKET_POLL_ATTEMPTS: u32 = 60;
const SOCKET_POLL_INTERVAL_MS: u64 = 200;
const PROXY_POLL_ATTEMPTS: u32 = 60;
const PROXY_POLL_INTERVAL_MS: u64 = 500;
const CONTAINER_PROXY_HOST: &str = "host.containers.internal";
const BRIDGE_CONTAINER_PATH: &str = "/usr/local/bin/enclo-container-bridge";
const BRIDGE_RETRY_DELAY: Duration = Duration::from_secs(2);

pub struct Secrets {
    config: SecretsConfig,
    child: ManagedChild,
    bridge_cancel: Arc<Notify>,
}

impl Secrets {
    pub fn new(config: SecretsConfig) -> Self {
        Self {
            config,
            child: ManagedChild::new("enclo-secrets"),
            bridge_cancel: Arc::new(Notify::new()),
        }
    }
}

/// Minimal TOML preview to extract proxy settings from the unified config file.
#[derive(Deserialize)]
struct ConfigPreview {
    #[serde(default)]
    proxy: ProxyPreview,
}

#[derive(Deserialize, Default)]
struct ProxyPreview {
    #[serde(default)]
    enabled: bool,
    listen: Option<SocketAddr>,
    tls: Option<TlsPreview>,
}

#[derive(Deserialize)]
struct TlsPreview {
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
        .map_err(|e| anyhow::anyhow!("failed to run {binary} generate-basic-auth: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("generate-basic-auth failed: {}", stderr.trim());
    }

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
impl Module for Secrets {
    fn name(&self) -> &str { "secrets" }
    fn enabled(&self) -> bool { self.config.enabled }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        if self.config.config.is_empty() {
            anyhow::bail!(
                "modules.secrets.config must be set to the path of your enclo-secrets config file"
            );
        }

        let config_path = std::path::Path::new(&self.config.config);
        let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));

        let raw = tokio::fs::read_to_string(config_path).await
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", self.config.config))?;
        let preview: ConfigPreview = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", self.config.config))?;

        let socket_path = ctx.enclo_dir.join("secrets.sock");
        let binary_path = support::resolve_binary_path("enclo-secrets")?;

        let mut args = vec![
            "serve".to_string(),
            "--socket".to_string(), socket_path.to_string_lossy().into_owned(),
            "--config".to_string(), self.config.config.clone(),
        ];

        // Proxy-specific setup: generate ephemeral credentials and pass them in.
        let proxy_password = if preview.proxy.enabled {
            tracing::info!("generating enclo-secrets basic auth credentials");
            let pw = random_password();
            let hash = generate_password_hash(&binary_path.to_string_lossy(), &pw).await?;
            args.extend(["--username".into(), "enclo".into(), "--password-hash".into(), hash]);
            Some(pw)
        } else {
            None
        };

        tracing::info!("starting enclo-secrets");
        let child = tokio::process::Command::new(&binary_path)
            .args(&args)
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn enclo-secrets: {e}"))?;
        self.child.store(child).await;

        tracing::info!("waiting for enclo-secrets socket at {}", socket_path.display());
        support::wait_for_path(
            &socket_path,
            SOCKET_POLL_ATTEMPTS,
            std::time::Duration::from_millis(SOCKET_POLL_INTERVAL_MS),
            "enclo-secrets socket",
        )
        .await?;

        let mut out = ModuleOutput::default();

        match ctx.platform {
            Platform::Linux => {
                // Mount the socket and in-container client binary directly.
                out.volumes.push(VolumeMount::new(&socket_path, "/tmp/enclo-secrets.sock"));
                let container_binary = support::resolve_container_binary_path("enclo-secrets")?;
                out.volumes.push(
                    VolumeMount::new(&container_binary, "/usr/local/bin/enclo-secrets").with_options("ro"),
                );
            }
            Platform::MacOs => {
                // The Podman VM does not pass Unix socket mounts through; mount the
                // bridge binary instead and let post_start tunnel the socket via podman exec.
                let bridge_binary = support::resolve_container_binary_path("enclo-container-bridge")?;
                out.volumes.push(
                    VolumeMount::new(&bridge_binary, BRIDGE_CONTAINER_PATH).with_options("ro"),
                );
            }
        }

        // Proxy output: CA cert, update-ca-trust, and HTTP_PROXY env vars.
        if let Some(password) = proxy_password {
            let listen_addr = preview.proxy.listen
                .ok_or_else(|| anyhow::anyhow!("proxy.listen is required when proxy.enabled = true"))?;
            let tls = preview.proxy.tls
                .ok_or_else(|| anyhow::anyhow!("proxy.tls is required when proxy.enabled = true"))?;

            tracing::info!("waiting for enclo-secrets proxy on {listen_addr}");
            support::wait_for_tcp(
                listen_addr,
                PROXY_POLL_ATTEMPTS,
                std::time::Duration::from_millis(PROXY_POLL_INTERVAL_MS),
                "enclo-secrets proxy",
            )
            .await?;

            let ca_cert = if tls.ca_cert.is_absolute() {
                tls.ca_cert
            } else {
                config_dir.join(&tls.ca_cert)
            };

            out.volumes.push(
                VolumeMount::new(&ca_cert, "/etc/pki/ca-trust/source/anchors/enclo-secrets-ca.pem")
                    .with_options("ro"),
            );
            out.post_start_cmds.push(PostStartCmd {
                args: vec!["update-ca-trust".into()],
                user: Some("root".into()),
            });

            let proxy_url = format!(
                "http://enclo:{password}@{}:{}",
                CONTAINER_PROXY_HOST,
                listen_addr.port(),
            );

            match self.config.mode {
                ProxyMode::Implicit => {
                    for key in &["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                        out.env_vars.push(((*key).into(), proxy_url.clone()));
                        out.exec_env_vars.push(((*key).into(), proxy_url.clone()));
                    }
                }
                ProxyMode::Explicit => {
                    out.env_vars.push(("ENCLO_PROXY".into(), proxy_url.clone()));
                    out.exec_env_vars.push(("ENCLO_PROXY".into(), proxy_url));
                }
            }
        }

        Ok(out)
    }

    async fn post_start(&self, ctx: &Context) -> anyhow::Result<()> {
        if ctx.platform != Platform::MacOs {
            return Ok(());
        }

        let host_socket = ctx.enclo_dir.join("secrets.sock");
        let container_socket = self.config.bridge_socket_path.clone()
            .unwrap_or_else(|| "/tmp/enclo-secrets.sock".into());

        tokio::spawn(bridge_loop(
            ctx.container_name.clone(),
            host_socket,
            PathBuf::from(BRIDGE_CONTAINER_PATH),
            container_socket.clone(),
            Arc::clone(&self.bridge_cancel),
        ));

        tracing::info!("waiting for enclo-container-bridge socket in container");
        support::wait_for_container_socket(
            &ctx.container_name,
            &container_socket,
            30,
            Duration::from_millis(200),
            "enclo-container-bridge socket",
        )
        .await
    }

    fn can_reload(&self) -> bool { true }

    async fn module_status(&self) -> ModuleStatus {
        ModuleStatus { process_alive: self.child.process_alive().await }
    }

    async fn reload(&self, _ctx: &Context) -> anyhow::Result<()> {
        self.child.send_signal(libc::SIGHUP).await
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        self.bridge_cancel.notify_one();
        self.child.kill().await
    }
}

async fn bridge_loop(
    container_name: String,
    host_socket: PathBuf,
    bridge_binary: PathBuf,
    container_socket: String,
    cancel: Arc<Notify>,
) {
    loop {
        let result = tokio::select! {
            _ = cancel.notified() => return,
            result = run_bridge_once(
                &container_name, &host_socket, &bridge_binary, &container_socket,
            ) => result,
        };

        match result {
            Ok(()) => tracing::info!("container secrets bridge disconnected"),
            Err(e) => tracing::warn!("container secrets bridge error: {e}"),
        }

        tokio::select! {
            _ = cancel.notified() => return,
            _ = tokio::time::sleep(BRIDGE_RETRY_DELAY) => {}
        }
    }
}

async fn run_bridge_once(
    container_name: &str,
    host_socket: &Path,
    bridge_binary: &Path,
    container_socket: &str,
) -> anyhow::Result<()> {
    let stream = tokio::net::UnixStream::connect(host_socket)
        .await
        .map_err(|e| anyhow::anyhow!("connect to secrets socket: {e}"))?;
    let (mut sock_read, mut sock_write) = stream.into_split();

    let binary_str = bridge_binary.to_string_lossy();
    let mut child = Command::new("podman")
        .args([
            "exec", "-i",
            container_name,
            binary_str.as_ref(),
            "--socket", container_socket,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn podman exec: {e}"))?;

    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = child.stdout.take().unwrap();

    let fwd_requests = tokio::spawn(async move {
        tokio::io::copy(&mut child_stdout, &mut sock_write).await
    });

    let fwd_responses = tokio::spawn(async move {
        tokio::io::copy(&mut sock_read, &mut child_stdin).await
    });

    tokio::select! {
        status = child.wait() => {
            let s = status.map_err(|e| anyhow::anyhow!("podman exec wait: {e}"))?;
            if !s.success() {
                anyhow::bail!("podman exec exited with {s}");
            }
        }
        r = fwd_requests => {
            r.map_err(|e| anyhow::anyhow!("request forwarder panicked: {e}"))??;
        }
        r = fwd_responses => {
            r.map_err(|e| anyhow::anyhow!("response forwarder panicked: {e}"))??;
        }
    }

    Ok(())
}
