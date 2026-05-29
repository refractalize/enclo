use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use tokio::{process::Command, sync::Notify};

use crate::{
    config::SecretsBridgeConfig,
    context::Context,
    module::{Module, ModuleOutput, ModuleStatus, VolumeMount},
    modules::support,
};

const BRIDGE_CONTAINER_PATH: &str = "/usr/local/bin/enclo-container-bridge";
const RETRY_DELAY_SECS: u64 = 2;

pub struct SecretsBridge {
    config: SecretsBridgeConfig,
    cancel: Arc<Notify>,
}

impl SecretsBridge {
    pub fn new(config: SecretsBridgeConfig) -> Self {
        Self { config, cancel: Arc::new(Notify::new()) }
    }
}

#[async_trait]
impl Module for SecretsBridge {
    fn name(&self) -> &str { "secrets_bridge" }
    fn enabled(&self) -> bool { self.config.enabled }
    fn dependencies(&self) -> &[&str] { &["secrets"] }

    async fn setup(&self, _ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let binary_path = support::resolve_container_binary_path("enclo-container-bridge")?;
        let mut out = ModuleOutput::default();
        out.volumes.push(
            VolumeMount::new(&binary_path, BRIDGE_CONTAINER_PATH).with_options("ro"),
        );
        Ok(out)
    }

    async fn post_start(&self, ctx: &Context) -> anyhow::Result<()> {
        let host_socket = ctx.enclo_dir.join("secrets.sock");

        let container_socket = self.config.socket_path.clone()
            .unwrap_or_else(|| "/tmp/enclo-secrets.sock".into());

        tokio::spawn(bridge_loop(
            ctx.container_name.clone(),
            host_socket,
            PathBuf::from(BRIDGE_CONTAINER_PATH),
            container_socket.clone(),
            Arc::clone(&self.cancel),
        ));

        tracing::info!("waiting for enclo-container-bridge socket in container");
        support::wait_for_container_socket(
            &ctx.container_name,
            &container_socket,
            30,
            std::time::Duration::from_millis(200),
            "enclo-container-bridge socket",
        )
        .await
    }

    async fn module_status(&self) -> ModuleStatus {
        ModuleStatus { process_alive: None }
    }

    async fn teardown(&self, _ctx: &Context) -> anyhow::Result<()> {
        self.cancel.notify_one();
        Ok(())
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
            _ = tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)) => {}
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

    // Forward container bridge requests (stdout) → host secrets socket
    let fwd_requests = tokio::spawn(async move {
        tokio::io::copy(&mut child_stdout, &mut sock_write).await
    });

    // Forward host secrets responses → container bridge stdin
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
