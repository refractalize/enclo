use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::{
    process::Command,
    sync::{Mutex, Notify},
};

use crate::{
    config::{ProxyMode, SecretsConfig},
    context::Context,
    module::{Module, ModuleOutput, ModuleStatus, PostStartCmd, VolumeMount},
    modules::support::{self, ManagedChild},
    platform::Platform,
};

const SOCKET_POLL_ATTEMPTS: u32 = 60;
const SOCKET_POLL_INTERVAL_MS: u64 = 200;
const CONTAINER_PROXY_HOST: &str = "host.containers.internal";
const BRIDGE_CONTAINER_PATH: &str = "/usr/local/bin/enclo-container-bridge";
const BRIDGE_RETRY_DELAY: Duration = Duration::from_secs(2);

pub struct Secrets {
    config: SecretsConfig,
    network: String,
    child: ManagedChild,
    bridge_cancel: Arc<Notify>,
    bridge_exec_id: Mutex<Option<String>>,
    pending: Mutex<Option<Command>>,
    host_store: ManagedChild,
    namespaced: Mutex<bool>,
}

impl Secrets {
    pub fn new(config: SecretsConfig, network: String) -> Self {
        Self {
            config,
            network,
            child: ManagedChild::new("enclo-secrets"),
            bridge_cancel: Arc::new(Notify::new()),
            bridge_exec_id: Mutex::new(None),
            pending: Mutex::new(None),
            host_store: ManagedChild::new("enclo-host-store"),
            namespaced: Mutex::new(false),
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
    fn name(&self) -> &str {
        "secrets"
    }
    fn enabled(&self) -> bool {
        self.config.enabled
    }

    async fn setup(&self, ctx: &Context) -> anyhow::Result<ModuleOutput> {
        let config_str = self.config.config.clone().unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
            format!("{home}/.config/enclo/secrets.toml")
        });

        let config_path = std::path::Path::new(&config_str);
        let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));

        let raw = tokio::fs::read_to_string(config_path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", config_str))?;
        let preview: ConfigPreview = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", config_str))?;

        let deferred =
            preview.proxy.enabled && self.network != "host" && ctx.platform == Platform::Linux;
        let socket_dir = ctx.enclo_dir.join("secrets-service");
        let socket_path = if deferred {
            tokio::fs::create_dir_all(&socket_dir).await?;
            socket_dir.join("secrets.sock")
        } else {
            ctx.enclo_dir.join("secrets.sock")
        };
        let binary_path = support::resolve_binary_path("enclo-secrets")?;

        let (bind_ip, container_proxy_host) = if self.network == "host" || !preview.proxy.enabled {
            (IpAddr::V4(Ipv4Addr::LOCALHOST), "127.0.0.1".to_string())
        } else {
            let gw = crate::podman::network_gateway(&self.network).await?;
            (
                gw,
                if deferred {
                    gw.to_string()
                } else {
                    CONTAINER_PROXY_HOST.to_string()
                },
            )
        };

        let mut args = vec![
            "serve".to_string(),
            "--socket".to_string(),
            socket_path.to_string_lossy().into_owned(),
            "--config".to_string(),
            config_str.clone(),
        ];

        // Proxy-specific setup: generate ephemeral credentials and pass them in.
        let proxy_password = if preview.proxy.enabled {
            tracing::info!("generating enclo-secrets basic auth credentials");
            let pw = random_password();
            let hash = generate_password_hash(&binary_path.to_string_lossy(), &pw).await?;
            args.extend([
                "--username".into(),
                "enclo".into(),
                "--password-hash".into(),
                hash,
            ]);
            Some(pw)
        } else {
            None
        };

        // Bind the proxy port here and hand the socket to the sidecar, so the
        // address is known up front and early connections queue in the backlog.
        let proxy_listener = if preview.proxy.enabled && !deferred {
            let listener = std::net::TcpListener::bind(SocketAddr::new(bind_ip, 0))
                .map_err(|e| anyhow::anyhow!("failed to bind proxy listener on {bind_ip}: {e}"))?;
            args.extend(["--listen-fd".into(), listener.as_raw_fd().to_string()]);
            Some(listener)
        } else {
            None
        };
        // A fixed port can be reused on distinct workspace gateway IPs. A clash
        // on an explicitly shared network is reported before the container starts.
        let deferred_addr = SocketAddr::new(bind_ip, 18080);
        let rootless = deferred && crate::podman::is_rootless().await?;
        *self.namespaced.lock().await = rootless;
        if deferred {
            args.extend(["--listen".into(), deferred_addr.to_string()]);
        }
        let mut command = if rootless {
            let private_socket = ctx.enclo_dir.join("host-store.sock");
            let mut host = Command::new(&binary_path);
            host.args(["serve", "--store-only", "--config", &config_str, "--socket"])
                .arg(&private_socket)
                .process_group(0)
                .kill_on_drop(true);
            self.host_store.store_group(host.spawn()?).await;
            support::wait_for_unix_socket(
                &private_socket,
                SOCKET_POLL_ATTEMPTS,
                Duration::from_millis(SOCKET_POLL_INTERVAL_MS),
                "host credential service",
                Some(&self.host_store),
            )
            .await?;
            args.extend([
                "--store-socket".into(),
                private_socket.to_string_lossy().into_owned(),
            ]);
            let mut proxy = Command::new("podman");
            proxy
                .args([
                    "unshare",
                    "--rootless-netns",
                    "sh",
                    "-c",
                    "echo $$ > \"$1\"; shift; exec \"$@\"",
                    "enclo-proxy",
                ])
                .arg(ctx.enclo_dir.join("secrets.pid"))
                .arg(&binary_path);
            proxy
        } else {
            Command::new(&binary_path)
        };
        command.args(&args).process_group(0).kill_on_drop(true);
        if let Some(listener) = &proxy_listener {
            let fd = listener.as_raw_fd();
            // SAFETY: fcntl is async-signal-safe. Clearing close-on-exec in the
            // forked child lets only this sidecar inherit the listener.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let proxy_addr = match &proxy_listener {
            Some(listener) => Some(listener.local_addr()?),
            None if deferred => Some(deferred_addr),
            None => None,
        };
        if deferred {
            *self.pending.lock().await = Some(command);
        } else {
            tracing::info!("starting enclo-secrets");
            self.child.store_group(command.spawn()?).await;
            support::wait_for_unix_socket(
                &socket_path,
                SOCKET_POLL_ATTEMPTS,
                Duration::from_millis(SOCKET_POLL_INTERVAL_MS),
                "enclo-secrets",
                Some(&self.child),
            )
            .await?;
        }
        drop(proxy_listener);

        let mut out = ModuleOutput::default();
        out.requires_container_init = deferred;

        match ctx.platform {
            Platform::Linux => {
                // Mount the socket and in-container client binary directly.
                if deferred {
                    // The socket is created after container init. Mount its existing
                    // parent directory so the later socket appears in the container.
                    out.volumes.push(
                        VolumeMount::new(&socket_dir, "/run/enclo/secrets").with_options("ro"),
                    );
                } else {
                    out.volumes.push(VolumeMount::new(
                        &socket_path,
                        "/run/enclo/secrets/secrets.sock",
                    ));
                }
                let container_binary = support::resolve_container_binary_path("enclo-secrets")?;
                out.volumes.push(
                    VolumeMount::new(&container_binary, "/usr/local/bin/enclo-secrets")
                        .with_options("ro"),
                );
            }
            Platform::MacOs => {
                // The Podman VM does not pass Unix socket mounts through; mount the
                // bridge binary instead and let post_start tunnel the socket via podman exec.
                let bridge_binary =
                    support::resolve_container_binary_path("enclo-container-bridge")?;
                out.volumes.push(
                    VolumeMount::new(&bridge_binary, BRIDGE_CONTAINER_PATH).with_options("ro"),
                );
                // /run is root-owned inside the container; create /run/enclo/secrets as root
                // and make it world-writable so the bridge (running as the container
                // user) can create its socket there.
                out.post_start_cmds.push(PostStartCmd {
                    args: vec!["mkdir".into(), "-p".into(), "/run/enclo/secrets".into()],
                    user: Some("root".into()),
                });
                out.post_start_cmds.push(PostStartCmd {
                    args: vec!["chmod".into(), "1777".into(), "/run/enclo/secrets".into()],
                    user: Some("root".into()),
                });
            }
        }

        // Proxy output: CA cert, update-ca-trust, and HTTP_PROXY env vars.
        if let Some(password) = proxy_password {
            let tls = preview.proxy.tls.ok_or_else(|| {
                anyhow::anyhow!("proxy.tls is required when proxy.enabled = true")
            })?;
            let listen_addr = proxy_addr.expect("proxy listener is bound when proxy is enabled");

            let ca_cert = if tls.ca_cert.is_absolute() {
                tls.ca_cert
            } else {
                config_dir.join(&tls.ca_cert)
            };

            out.volumes.push(
                VolumeMount::new(
                    &ca_cert,
                    "/etc/pki/ca-trust/source/anchors/enclo-secrets-ca.pem",
                )
                .with_options("ro"),
            );
            out.post_start_cmds.push(PostStartCmd {
                args: vec!["update-ca-trust".into()],
                user: Some("root".into()),
            });

            let proxy_url = format!(
                "http://enclo:{password}@{}:{}",
                container_proxy_host,
                listen_addr.port(),
            );

            match self.config.proxy_mode {
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

    async fn activate(&self, ctx: &Context) -> anyhow::Result<()> {
        if let Some(mut command) = self.pending.lock().await.take() {
            tracing::info!("activating namespaced proxy with host credential service");
            self.child.store_group(command.spawn()?).await;
            support::wait_for_unix_socket(
                &ctx.enclo_dir.join("secrets-service/secrets.sock"),
                SOCKET_POLL_ATTEMPTS,
                Duration::from_millis(SOCKET_POLL_INTERVAL_MS),
                "enclo-secrets",
                Some(&self.child),
            )
            .await?;
        }
        Ok(())
    }

    async fn post_start(&self, ctx: &Context) -> anyhow::Result<()> {
        if ctx.platform != Platform::MacOs {
            return Ok(());
        }

        let host_socket = ctx.enclo_dir.join("secrets.sock");
        let container_socket = self
            .config
            .bridge_socket_path
            .clone()
            .unwrap_or_else(|| "/run/enclo/secrets/secrets.sock".into());

        let exec_ids_before = crate::podman::exec_ids(&ctx.container_name)
            .await
            .unwrap_or_default();

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
        .await?;

        let exec_ids_after = crate::podman::exec_ids(&ctx.container_name)
            .await
            .unwrap_or_default();
        let new_id = exec_ids_after
            .into_iter()
            .find(|id| !exec_ids_before.contains(id));
        if let Some(id) = new_id {
            tracing::debug!("bridge exec session id: {id}");
            *self.bridge_exec_id.lock().await = Some(id);
        } else {
            tracing::warn!("could not determine bridge exec session id");
        }

        Ok(())
    }

    fn can_reload(&self) -> bool {
        true
    }

    async fn module_status(&self) -> ModuleStatus {
        let managed_exec_ids = self.bridge_exec_id.lock().await.iter().cloned().collect();
        ModuleStatus {
            process_alive: if self.host_store.process_alive().await == Some(false) {
                Some(false)
            } else {
                self.child.process_alive().await
            },
            managed_exec_ids,
        }
    }

    async fn reload(&self, ctx: &Context) -> anyhow::Result<()> {
        if *self.namespaced.lock().await {
            self.host_store.send_signal(libc::SIGHUP).await?;
            signal_service(ctx, libc::SIGHUP).await
        } else {
            self.child.send_signal(libc::SIGHUP).await
        }
    }

    async fn teardown(&self, ctx: &Context) -> anyhow::Result<()> {
        self.bridge_cancel.notify_one();
        self.pending.lock().await.take();
        if *self.namespaced.lock().await && self.child.process_alive().await == Some(true) {
            let _ = signal_service(ctx, libc::SIGTERM).await;
        }
        // Stop credentials immediately; proxy connection drain cannot delay this.
        self.host_store.stop_group(Duration::from_secs(5)).await?;
        self.child.stop_group(Duration::from_secs(5)).await
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
            "exec",
            "-i",
            container_name,
            binary_str.as_ref(),
            "--socket",
            container_socket,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn podman exec: {e}"))?;

    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = child.stdout.take().unwrap();

    let fwd_requests =
        tokio::spawn(async move { tokio::io::copy(&mut child_stdout, &mut sock_write).await });

    let fwd_responses =
        tokio::spawn(async move { tokio::io::copy(&mut sock_read, &mut child_stdin).await });

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

async fn signal_service(ctx: &Context, signal: libc::c_int) -> anyhow::Result<()> {
    let pid: i32 = tokio::fs::read_to_string(ctx.enclo_dir.join("secrets.pid"))
        .await?
        .trim()
        .parse()?;
    anyhow::ensure!(pid > 1, "invalid namespaced proxy PID");
    if unsafe { libc::kill(pid, signal) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
