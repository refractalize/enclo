//! Local Podman machines. No host systemd, D-Bus, or persistent enclo daemon.
mod features;
mod init;
pub mod managed_home;
mod ssh_export;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    container: Container,
    ssh: Ssh,
    #[serde(default)]
    systemd: Systemd,
    #[serde(default, skip_serializing_if = "managed_home::HomeConfig::disabled")]
    home: managed_home::HomeConfig,
    #[serde(default, skip_serializing_if = "features::NestedPodman::disabled")]
    nested_podman: features::NestedPodman,
    #[serde(default, skip_serializing_if = "features::Gpu::disabled")]
    gpu: features::Gpu,
    #[serde(
        default,
        skip_serializing_if = "crate::desktop::DesktopConfig::disabled"
    )]
    desktop: crate::desktop::DesktopConfig,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Container {
    #[serde(default, skip_serializing)]
    init: init::InitConfig,
    image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    devices: Vec<String>,
    #[serde(default)]
    keep_groups: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ssh {
    user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_port: Option<std::num::NonZeroU16>,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Systemd {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    mask: Vec<String>,
}
impl Systemd {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.enabled || self.mask.is_empty(),
            "systemd.mask requires systemd.enabled = true"
        );
        for unit in &self.mask {
            ensure!(
                !unit.starts_with('-')
                    && !unit.starts_with('.')
                    && unit.len() <= 255
                    && unit.contains('.')
                    && unit
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.@:".contains(&b)),
                "invalid systemd mask unit: {unit}"
            );
            ensure!(
                unit != "enclo-sshd.service",
                "cannot mask enclo-sshd.service: SSH access is required"
            );
        }
        Ok(())
    }
    fn bootstrap(&self) -> String {
        if self.enabled {
            let mut script = String::from("set -eu\n");
            // Enclo owns port 22 and its authorized-key configuration. Images may
            // enable a distro SSH daemon (or socket activation) by default.
            script.push_str("systemctl --root=/ mask -- 'sshd.service' 'sshd.socket' 'ssh.service' 'ssh.socket'");
            for unit in &self.mask {
                script.push_str(&format!(" '{unit}'"));
            }
            script.push('\n');
            script.push_str("exec /sbin/init\n");
            script
        } else {
            String::from("set -eu\nssh-keygen -A\nmkdir -p /run/sshd\ntouch /run/enclo-ssh-prepared\nexec /usr/sbin/sshd -D -e -f /run/enclo-ssh/sshd_config\n")
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Endpoint {
    pub name: String,
    pub container_id: String,
    pub running: bool,
    pub port: Option<u16>,
    pub state_directory: PathBuf,
    pub home: Option<managed_home::HomeSpec>,
}

pub struct LocalContainer {
    name: String,
    podman_name: String,
    scope: String,
    config: PathBuf,
    dir: PathBuf,
    state_root: PathBuf,
    state: PathBuf,
}
impl LocalContainer {
    pub fn new(
        name: &str,
        config_root: &Path,
        runtime_root: Option<&Path>,
        state_root: Option<&Path>,
    ) -> Result<Self> {
        ensure!(
            unsafe { libc::geteuid() } != 0,
            "local machines require rootless Podman; run enclo as your normal user"
        );
        crate::profiles::validate_name(name)?;
        let root = if config_root.is_absolute() {
            config_root.to_owned()
        } else {
            std::env::current_dir()?.join(config_root)
        };
        let root = root
            .canonicalize()
            .context("resolve container configuration directory")?;
        let scope = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        let dir = crate::connection_runtime::connections_dir(runtime_root)
            .parent()
            .unwrap()
            .join("containers")
            .join(name);
        let mut state_root = crate::state::directory(state_root);
        if state_root.is_relative() {
            state_root = std::env::current_dir()?.join(state_root);
        }
        let state = state_root.join("containers").join(&scope[..12]).join(name);
        Ok(Self {
            state_root,
            state,
            name: name.into(),
            podman_name: format!("enclo-{}-{name}", &scope[..12]),
            scope,
            config: root.join("containers").join(format!("{name}.toml")),
            dir,
        })
    }
    fn lock(&self) -> Result<File> {
        use std::os::unix::fs::DirBuilderExt;
        // Create the chosen application root, then validate every managed level.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.state_root)?;
        crate::connection_runtime::validate(&self.state_root)?;
        for path in [
            self.state_root.join("containers"),
            self.state.parent().unwrap().to_owned(),
            self.state.clone(),
        ] {
            private_dir(&path)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.state.join("lifecycle.lock"))?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "container lifecycle operation already in progress"
        );
        Ok(file)
    }
    fn config(&self) -> Result<Config> {
        let config: Config = toml::from_str(&fs::read_to_string(&self.config).with_context(|| format!("read {}", self.config.display()))?).context("local SSH container config requires [container] and [ssh]; legacy modules are not supported by this backend")?;
        ensure!(
            !config.container.image.is_empty() && !config.container.image.starts_with('-'),
            "invalid container image"
        );
        ensure!(
            !config.ssh.user.is_empty()
                && config
                    .ssh
                    .user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid SSH user"
        );
        config.systemd.validate()?;
        if let Some(hostname) = &config.container.hostname {
            ensure!(
                hostname.len() <= 64 && hostname.split('.').all(|label| {
                    !label.is_empty() && label.len() <= 63
                        && label.as_bytes()[0].is_ascii_alphanumeric()
                        && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                        && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                }),
                "container.hostname must be a hostname of at most 64 ASCII characters, with alphanumeric labels separated by dots and optional internal hyphens"
            );
        }
        config.home.validate()?;
        config.container.init.validate()?;
        config.validate_features()?;
        Ok(config)
    }
    async fn inspect(&self) -> Result<Option<serde_json::Value>> {
        let exists = podman()
            .args(["container", "exists", &self.podman_name])
            .status()
            .await?;
        if exists.code() == Some(1) {
            return Ok(None);
        }
        ensure!(
            exists.success(),
            "Podman could not check container existence"
        );
        let output = output(podman().args(["inspect", &self.podman_name])).await?;
        let items: Vec<serde_json::Value> = serde_json::from_slice(&output)?;
        let value = items
            .into_iter()
            .next()
            .context("Podman returned no container")?;
        ensure!(
            value["Config"]["Labels"]["io.enclo.scope"].as_str() == Some(&self.scope)
                && value["Config"]["Labels"]["io.enclo.container"].as_str() == Some(&self.name),
            "refusing to use a container not owned by this configuration"
        );
        if let Some(state) = value["Config"]["Labels"]["io.enclo.state"].as_str() {
            let selected = self
                .state
                .canonicalize()
                .unwrap_or_else(|_| self.state.clone());
            ensure!(Some(state) == selected.to_str(), "container belongs to another persistent state directory; use its original --state-dir");
        }
        Ok(Some(value))
    }
    pub async fn status(&self) -> Result<Option<Endpoint>> {
        self.inspect().await?.map(|v| self.endpoint(&v)).transpose()
    }
    fn endpoint(&self, value: &serde_json::Value) -> Result<Endpoint> {
        let port = match value["NetworkSettings"]["Ports"]["22/tcp"].as_array() {
            Some(bindings) => {
                ensure!(
                    bindings.len() == 1 && bindings[0]["HostIp"] == "127.0.0.1",
                    "container SSH must be published only on IPv4 loopback"
                );
                Some(
                    bindings[0]["HostPort"]
                        .as_str()
                        .context("missing SSH port")?
                        .parse()?,
                )
            }
            None => None,
        };
        let id = value["Id"].as_str().context("missing container ID")?;
        ensure!(
            !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid container ID"
        );
        Ok(Endpoint {
            name: self.name.clone(),
            container_id: value["Id"].as_str().context("missing container ID")?.into(),
            running: value["State"]["Running"] == true,
            port,
            state_directory: self.state.clone(),
            home: value["Config"]["Labels"]["io.enclo.home"]
                .as_str()
                .map(serde_json::from_str)
                .transpose()?,
        })
    }
    pub async fn start(&self) -> Result<Endpoint> {
        // Parse before touching Podman, including for already-existing machines.
        let config = self.config()?;
        let _lock = self.lock()?;
        self.start_locked(config).await
    }
    async fn start_locked(&self, config: Config) -> Result<Endpoint> {
        let revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&config)?));
        crate::connection_runtime::prepare(&self.dir)?;
        self.migrate_identity()?;
        let bootstrap = self.state.join("bootstrap");
        private_dir(&bootstrap)?;
        let existing = self.inspect().await?;
        if let Some(value) = &existing {
            ensure!(value["Config"]["Labels"]["io.enclo.config"] == revision, "container configuration changed; existing machines are not automatically recreated (use container recreate to apply changes; this discards the writable layer)");
            // A runtime directory can be deleted between starts. The bind mount
            // must still refer to our bootstrap directory, not another namespace.
            let expected = bootstrap.canonicalize()?;
            ensure!(
                value["Mounts"].as_array().is_some_and(|mounts| mounts
                    .iter()
                    .any(|m| m["Destination"] == "/run/enclo-ssh"
                        && m["Source"].as_str() == expected.to_str())),
                "container bootstrap is not in this persistent state directory; use its original --state-dir or container recreate to migrate"
            );
        }
        let home = self.plan_home(&config, existing.as_ref()).await?;
        if existing.is_none() {
            if let Some(home) = &home {
                self.prepare_home(&config, home).await?;
            }
        }
        let key = self.state.join("client_ed25519");
        if !key.exists() {
            output(
                Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(&key),
            )
            .await?;
        }
        let public = output(Command::new("ssh-keygen").arg("-y").arg("-f").arg(&key)).await?;
        atomic_write(bootstrap.join("authorized_keys"), public)?;
        atomic_write(bootstrap.join("sshd_config"), format!("Port 22\nListenAddress 0.0.0.0\nHostKey /etc/ssh/ssh_host_ed25519_key\nAuthorizedKeysFile /run/enclo-ssh/authorized_keys\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin prohibit-password\nAllowUsers {}\nAllowTcpForwarding yes\nAllowStreamLocalForwarding yes\nGatewayPorts no\nStrictModes yes\nPidFile /run/enclo-sshd.pid\nSubsystem sftp internal-sftp\n", config.ssh.user))?;
        atomic_write(bootstrap.join("boot.sh"), config.systemd.bootstrap())?;
        if let Some(value) = &existing {
            if value["State"]["Running"] != true {
                output(podman().args(["start", &self.podman_name])).await?;
            }
        } else {
            let mut cmd = podman();
            cmd.args([
                "run",
                "--detach",
                "--name",
                &self.podman_name,
                "--label",
                &format!("io.enclo.scope={}", self.scope),
                "--label",
                &format!("io.enclo.container={}", self.name),
                "--label",
                &format!("io.enclo.config={revision}"),
                "--label",
                &format!("io.enclo.state={}", self.state.canonicalize()?.display()),
                "--network=bridge",
                "--userns=keep-id",
                "--user=0:0",
                "--cgroupns=private",
                "--security-opt=label=disable",
                "--volume",
            ])
            .arg(format!(
                "{}:/run/enclo-ssh:ro",
                bootstrap.canonicalize()?.display()
            ));
            cmd.arg(format!(
                "--publish=127.0.0.1:{}:22",
                config
                    .ssh
                    .host_port
                    .map(|p| p.to_string())
                    .unwrap_or_default()
            ));
            if let Some(home) = &home {
                cmd.arg("--volume")
                    .arg(format!("{}:{}:nocopy", home.volume, home.path));
                cmd.arg("--label")
                    .arg(format!("io.enclo.home={}", serde_json::to_string(home)?));
            }
            if let Some(hostname) = &config.container.hostname {
                cmd.arg("--hostname").arg(hostname);
            }
            if config.systemd.enabled {
                cmd.arg("--systemd=always");
            } else {
                cmd.args(["--init", "--systemd=false", "--stop-signal=SIGTERM"]);
            }
            self.feature_args(&config, &mut cmd)?;
            // Override image startup so the selected mode is unambiguous.
            cmd.args(["--entrypoint", "/bin/sh"]);
            if config.container.keep_groups {
                cmd.arg("--group-add=keep-groups");
            }
            for volume in &config.container.volumes {
                cmd.arg("--volume").arg(volume);
            }
            for env in &config.container.env {
                cmd.arg("--env").arg(env);
            }
            for device in &config.container.devices {
                cmd.arg("--device").arg(device);
            }
            cmd.arg(&config.container.image)
                .arg("/run/enclo-ssh/boot.sh");
            output(&mut cmd)
                .await
                .with_context(|| match config.ssh.host_port {
                    Some(port) => format!(
                        "start container with SSH on 127.0.0.1:{port}; the port must be available"
                    ),
                    None => "start container with an allocated SSH port".into(),
                })?;
        }
        let value = self
            .inspect()
            .await?
            .context("container disappeared during start")?;
        let endpoint = self.endpoint(&value)?;
        let id = &endpoint.container_id;
        if config.systemd.enabled {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                let result = output(podman().args([
                    "exec",
                    "--user=0",
                    id,
                    "systemctl",
                    "is-system-running",
                ]))
                .await;
                if result
                    .as_ref()
                    .is_ok_and(|v| String::from_utf8_lossy(v).trim() == "running")
                {
                    break;
                }
                // Degraded boot is usable when the system manager is responsive.
                if output(podman().args([
                    "exec",
                    "--user=0",
                    id,
                    "systemctl",
                    "show",
                    "--property=SystemState",
                    "--value",
                ]))
                .await
                .is_ok_and(|v| String::from_utf8_lossy(&v).trim() == "degraded")
                {
                    break;
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "container did not boot systemd within 60 seconds; inspect it with Podman"
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        } else {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                if output(podman().args([
                    "exec",
                    "--user=0",
                    id,
                    "test",
                    "-f",
                    "/run/enclo-ssh-prepared",
                ]))
                .await
                .is_ok()
                {
                    break;
                }
                ensure!(
                    self.status().await?.is_some_and(|e| e.running),
                    "SSH bootstrap exited; inspect podman logs for this container"
                );
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "SSH bootstrap timed out; inspect podman logs for this container"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let uid = output(podman().args(["exec", "--user=0", id, "id", "-u", &config.ssh.user]))
            .await
            .context("SSH user must already exist in the image")?;
        ensure!(
            String::from_utf8_lossy(&uid).trim() == unsafe { libc::geteuid() }.to_string(),
            "SSH user must have the host user's UID for keep-id mounts"
        );
        if config.systemd.enabled {
            output(podman().args(["exec", "--user=0", id, "ssh-keygen", "-A"]))
                .await
                .context("image must contain OpenSSH server and ssh-keygen")?;
            let unit = b"[Unit]\nDescription=Enclo SSH access\nAfter=network.target\n[Service]\nExecStartPre=/usr/bin/mkdir -p /run/sshd\nExecStart=/usr/sbin/sshd -D -e -f /run/enclo-ssh/sshd_config\nRestart=on-failure\nKillMode=process\n[Install]\nWantedBy=multi-user.target\n";
            let mut cmd = podman();
            cmd.args([
                "exec",
                "--interactive",
                "--user=0",
                id,
                "sh",
                "-c",
                "cat > /etc/systemd/system/enclo-sshd.service",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            child.stdin.take().unwrap().write_all(unit).await?;
            let result = child.wait_with_output().await?;
            ensure!(
                result.status.success(),
                "could not provision SSH systemd unit: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            output(podman().args(["exec", "--user=0", id, "systemctl", "daemon-reload"])).await?;
            output(podman().args([
                "exec",
                "--user=0",
                id,
                "systemctl",
                "enable",
                "--now",
                "enclo-sshd.service",
            ]))
            .await?;
        }
        self.setup_nested(&config, home.as_ref(), id).await?;
        let host_key = output(podman().args([
            "exec",
            "--user=0",
            id,
            "cat",
            "/etc/ssh/ssh_host_ed25519_key.pub",
        ]))
        .await?;
        let host_key = String::from_utf8(host_key)?;
        let parts: Vec<_> = host_key.split_whitespace().collect();
        ensure!(
            parts.len() >= 2 && parts[0] == "ssh-ed25519",
            "invalid container host key"
        );
        let instance = self.state.join("instances").join(id);
        private_dir(&self.state.join("instances"))?;
        private_dir(&instance)?;
        if let Some(home) = &home {
            self.initialize_home(&config, home, &value, false).await?;
        }
        self.initialize_container(&config, &value, false).await?;
        let known_hosts = instance.join("known_hosts");
        let pinned = format!("enclo-{id} {} {}\n", parts[0], parts[1]);
        if known_hosts.exists() {
            ensure!(fs::read_to_string(&known_hosts)? == pinned, "container host key changed for the same container ID; refusing to replace pinned trust");
        } else {
            atomic_write(&known_hosts, pinned)?;
        }
        atomic_write(instance.join("user"), &config.ssh.user)?;
        // Verify authentication before reporting readiness; never accept a key
        // obtained over the forwarded TCP port as the trust bootstrap.
        let mut profile = crate::connection::ConnectionProfile {
            target: crate::connection::Target {
                ssh: String::new(),
                container: Some(self.name.clone()),
                config: None,
            },
            forwards: vec![],
            session: Default::default(),
            services: Default::default(),
            local_desktop: None,
        };
        profile.target.ssh = format!("enclo-{id}");
        profile.target.config = Some(self.write_ssh_config(&endpoint)?.canonicalize()?);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if output(
                Command::new("ssh")
                    .arg("-F")
                    .arg(profile.target.config.as_ref().unwrap())
                    .args(["-S", "none", "-o", "ConnectTimeout=2", "-T", "-n"])
                    .arg(&profile.target.ssh)
                    .arg("true"),
            )
            .await
            .is_ok()
            {
                break;
            }
            ensure!(tokio::time::Instant::now() < deadline, "container SSH authentication failed; check the image's SSH user and container logs (enclo-sshd.service in systemd mode)");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        self.refresh_export_trust(&endpoint)?;
        atomic_write(
            self.dir.join("endpoint.json"),
            serde_json::to_vec_pretty(&endpoint)?,
        )?;
        Ok(endpoint)
    }
    pub async fn resolve(&self, profile: &mut crate::connection::ConnectionProfile) -> Result<()> {
        let endpoint = self
            .status()
            .await?
            .context("container does not exist; start it first")?;
        ensure!(endpoint.running, "container is stopped; start it first");
        let _lock = self.lock()?;
        crate::connection_runtime::prepare(&self.dir)?;
        let config = self.write_ssh_config(&endpoint)?;
        // Port mapping is stable for a Podman container ID. Re-creation gets a
        // new config and known-hosts file and cannot reuse an old endpoint.
        profile.target.ssh = format!("enclo-{}", endpoint.container_id);
        profile.target.config = Some(config.canonicalize()?);
        let value = self
            .inspect()
            .await?
            .context("container disappeared during resolution")?;
        self.resolve_features(&value, profile)?;
        Ok(())
    }
    fn migrate_identity(&self) -> Result<()> {
        let destination = self.state.join("client_ed25519");
        let legacy = self.dir.join("client_ed25519");
        if !destination.exists() && legacy.exists() {
            use std::os::unix::fs::MetadataExt;
            crate::connection_runtime::validate(&self.dir)?;
            let meta = fs::symlink_metadata(&legacy)?;
            ensure!(
                meta.is_file()
                    && meta.uid() == unsafe { libc::geteuid() }
                    && meta.mode() & 0o077 == 0,
                "legacy SSH key must be a private regular file owned by you"
            );
            atomic_write(destination, fs::read(legacy)?)?;
        }
        Ok(())
    }
    fn write_ssh_config(&self, endpoint: &Endpoint) -> Result<PathBuf> {
        let id = &endpoint.container_id;
        let port = endpoint.port.context("Podman did not publish SSH port")?;
        let text = self.ssh_stanza(endpoint, &format!("enclo-{id}"), false)?;
        let path = self.dir.join(format!("ssh_config-{id}-{port}"));
        atomic_write(&path, text)?;
        Ok(path)
    }
    fn ssh_stanza(&self, endpoint: &Endpoint, alias: &str, stable: bool) -> Result<String> {
        let id = &endpoint.container_id;
        let instance = self.state.join("instances").join(id);
        crate::connection_runtime::validate(&instance)
            .context("SSH trust state is missing; run container start first")?;
        let known_hosts = instance.join("known_hosts");
        let key = self.state.join("client_ed25519");
        let user = fs::read_to_string(instance.join("user"))?;
        ensure!(
            !user.is_empty()
                && user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid stored SSH user"
        );
        let port = endpoint.port.context("Podman did not publish SSH port")?;
        let (known_hosts, host_alias) = if stable {
            (self.state.join("known_hosts"), self.podman_name.clone())
        } else {
            (known_hosts, format!("enclo-{id}"))
        };
        let text = format!("Host {alias}\n HostName 127.0.0.1\n Port {port}\n User {user}\n IdentityFile {}\n IdentitiesOnly yes\n UserKnownHostsFile {}\n GlobalKnownHostsFile /dev/null\n HostKeyAlias {host_alias}\n StrictHostKeyChecking yes\n BatchMode yes\n", ssh_path(&key.canonicalize()?)?, ssh_path(&known_hosts.canonicalize()?)?);
        Ok(text)
    }
    pub async fn remove(&self, force: bool) -> Result<()> {
        let _lock = self.lock()?;
        self.remove_locked(force).await
    }
    async fn remove_locked(&self, force: bool) -> Result<()> {
        if let Some(value) = self.inspect().await? {
            let endpoint = self.endpoint(&value)?;
            ensure!(!endpoint.running || force, "container is running; stop it first or use --force (disconnects sessions and discards the writable layer)");
            let mut cmd = podman();
            cmd.arg("rm");
            if force {
                cmd.arg("--force");
            }
            // Never pass --volumes: preserve named volumes and bind-mounted data.
            cmd.arg(&endpoint.container_id);
            output(&mut cmd).await?;
        }
        let _ = fs::remove_file(self.dir.join("endpoint.json"));
        Ok(())
    }
    pub async fn recreate(&self, force: bool) -> Result<Endpoint> {
        let config = self.config()?;
        let _lock = self.lock()?;
        // Preflight local image before discarding the existing writable layer.
        output(podman().args(["image", "inspect", &config.container.image])).await
            .context("recreate requires an available local image; build or pull it first (existing container retained)")?;
        self.migrate_identity()?;
        self.plan_home(&config, None).await?;
        self.remove_locked(force).await?;
        self.start_locked(config).await
    }
    pub async fn stop(&self) -> Result<()> {
        let _lock = self.lock()?;
        if let Some(value) = self.inspect().await? {
            let endpoint = self.endpoint(&value)?;
            if endpoint.running {
                output(podman().args(["stop", &endpoint.container_id])).await?;
            }
        }
        let _ = fs::remove_file(self.dir.join("endpoint.json"));
        Ok(())
    }
    pub async fn list(config_root: &Path) -> Result<serde_json::Value> {
        let root = config_root.canonicalize()?;
        let scope = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        let bytes = output(podman().args([
            "ps",
            "--all",
            "--filter",
            &format!("label=io.enclo.scope={scope}"),
            "--format=json",
        ]))
        .await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    crate::connection_runtime::validate(path)
}
fn podman() -> Command {
    let mut command = Command::new("podman");
    command.arg("--remote=false").kill_on_drop(true);
    command
}
async fn output(cmd: &mut Command) -> Result<Vec<u8>> {
    cmd.kill_on_drop(true).stdin(Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .context("command timed out")??;
    if !output.status.success() {
        bail!(
            "command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}
fn ssh_path(path: &Path) -> Result<String> {
    let value = path.to_str().context("SSH paths must be UTF-8")?;
    ensure!(
        !value.chars().any(char::is_control),
        "SSH paths must not contain control characters"
    );
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

fn atomic_write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> Result<()> {
    use rand_core::RngCore;
    use std::io::Write;
    let path = path.as_ref();
    let temporary = path.with_extension(format!("tmp-{:x}", rand_core::OsRng.next_u64()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes.as_ref())?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
