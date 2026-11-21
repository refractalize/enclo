//! Foreground OpenSSH connections, independent of Podman and the installed daemon.
use crate::lifecycle::{Environment, Machine, MachineOwnership, Services, Sessions};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
pub mod host_services;
pub mod supervisor;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::process::{Child, Command};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionProfile {
    #[serde(skip_deserializing, default, skip_serializing_if = "Option::is_none")]
    pub local_desktop: Option<crate::desktop::Plan>,
    pub target: Target,
    #[serde(default)]
    pub forwards: Vec<Forward>,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub services: host_services::ServiceConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    #[serde(default)]
    pub ssh: String,
    pub host: Option<String>,
    /// Optional OpenSSH config; relative paths are relative to the profile.
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    pub direction: Direction,
    /// OpenSSH forwarding specification, passed as one argument.
    pub spec: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Local,
    Remote,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    pub workdir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl ConnectionProfile {
    pub fn load(path: &Path) -> Result<Self> {
        let mut profile: Self = toml::from_str(&fs::read_to_string(path).with_context(|| {
            format!(
                "read connection profile {}; check --config-dir or ENCLO_CONFIG_DIR",
                path.display()
            )
        })?)?;
        if let Some(config) = &mut profile.target.config {
            if config.is_relative() {
                *config = path.parent().unwrap_or(Path::new(".")).join(&config);
            }
            *config = config.canonicalize().context("resolve SSH config")?;
        }
        if let Some(secrets) = &mut profile.services.secrets {
            for file in std::iter::once(&mut secrets.config).chain(secrets.client_binary.iter_mut())
            {
                if file.is_relative() {
                    *file = path.parent().unwrap_or(Path::new(".")).join(&file);
                }
                *file = file
                    .canonicalize()
                    .context("resolve secrets service configuration/client")?;
            }
        }
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<()> {
        if let Some(secrets) = &self.services.secrets {
            ensure!(
                secrets.remote_proxy_port > 0,
                "remote_proxy_port must be nonzero"
            );
        }

        ensure!(
            self.target.host.is_some() == self.target.ssh.is_empty(),
            "target must select exactly one of ssh or host"
        );
        if let Some(name) = &self.target.host {
            crate::profiles::validate_name(name)?;
            ensure!(
                self.target.config.is_none(),
                "target.config applies only to external SSH targets"
            );
        } else {
            let host = &self.target.ssh;
            ensure!(
                !host.is_empty()
                    && !host.starts_with('-')
                    && !host.chars().any(|c| c.is_whitespace() || c.is_control()),
                "target.ssh must be an SSH destination, not options"
            );
        }
        for forward in &self.forwards {
            ensure!(
                !forward.spec.is_empty() && !forward.spec.chars().any(char::is_control),
                "forward specification must not be empty or contain control characters"
            );
        }
        for (name, value) in &self.session.env {
            ensure!(
                !name.is_empty()
                    && name.bytes().enumerate().all(|(i, c)| c == b'_'
                        || c.is_ascii_alphabetic()
                        || (i > 0 && c.is_ascii_digit())),
                "invalid environment variable name: {name}"
            );
            ensure!(
                !value.contains('\0'),
                "environment values must not contain NUL"
            );
        }
        ensure!(
            !self
                .session
                .workdir
                .as_ref()
                .is_some_and(|p| p.contains('\0')),
            "working directory must not contain NUL"
        );
        Ok(())
    }

    pub async fn resolve_host(
        &mut self,
        config_root: &Path,
        runtime_root: Option<&Path>,
        state_root: Option<&Path>,
        start: bool,
    ) -> Result<()> {
        if let Some(name) = &self.target.host {
            let machine = crate::local_container::LocalContainer::new(
                name,
                config_root,
                runtime_root,
                state_root,
            )?;
            if start {
                machine.start().await?;
            }
            machine.resolve(self).await?;
        }
        Ok(())
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("ssh");
        if let Some(config) = &self.target.config {
            cmd.arg("-F").arg(config);
        }
        // Authentication/host selection still come from normal SSH config.
        // Lifecycle, trust, and forwarding behavior are explicitly controlled.
        cmd.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "ControlPersist=no",
            "-o",
            "ForkAfterAuthentication=no",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ForwardX11=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "RemoteCommand=none",
        ]);
        cmd.kill_on_drop(true);
        cmd
    }

    fn independent(&self) -> Command {
        let mut cmd = self.command();
        cmd.args([
            "-S",
            "none",
            "-o",
            "ControlMaster=no",
            "-o",
            "ClearAllForwardings=yes",
        ]);
        cmd
    }
}

/// Each connection name gets a private, stable control socket namespace.
/// No files are created by merely resolving a connection for a shell.
#[derive(Clone)]
struct Runtime {
    dir: PathBuf,
}
impl Runtime {
    fn resolve(profile: &Path, root: Option<&Path>) -> Result<Self> {
        let dir = crate::connection_runtime::directory(profile, root)?;
        Ok(Self { dir })
    }
    fn socket(&self) -> PathBuf {
        self.dir.join("ssh-control")
    }
    fn check_directory(&self) -> Result<()> {
        let meta = fs::symlink_metadata(&self.dir)
            .context("services are not started; run enclo services run --profile <file> first")?;
        ensure!(
            meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "connection runtime directory must be owned by you with mode 0700"
        );
        Ok(())
    }
    fn lock(&self) -> Result<File> {
        crate::connection_runtime::prepare(&self.dir)?;
        self.check_directory()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.dir.join("lock"))?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "services already have a foreground supervisor"
        );
        Ok(file)
    }
    async fn check(&self, profile: &ConnectionProfile) -> Result<()> {
        self.check_directory()?;
        ensure!(
            self.dir.join("ready").is_file(),
            "services are not ready; run enclo services run --profile <file> first"
        );
        ensure!(
            fs::read_to_string(self.dir.join("ready"))? == profile_revision(profile)?,
            "profile differs from running services; reload services first"
        );
        self.transport_check(profile).await
    }
    async fn transport_check(&self, profile: &ConnectionProfile) -> Result<()> {
        self.check_directory()?;
        let mut cmd = self.control_command("check");
        cmd.arg(&profile.target.ssh);
        let output = tokio::time::timeout(Duration::from_secs(3), cmd.output())
            .await?
            .context("launch ssh for transport check; ensure OpenSSH is available on PATH")?;
        ensure!(
            output.status.success(),
            "service transport is unavailable; run enclo services run --profile <file> first"
        );
        Ok(())
    }

    fn control_command(&self, operation: &str) -> Command {
        // Control requests use the existing master, not host configuration.
        // In particular, don't accidentally add forwards from ~/.ssh/config.
        let mut cmd = Command::new("ssh");
        cmd.args(["-F", "none", "-S"])
            .arg(self.socket())
            .args(["-O", operation]);
        cmd.kill_on_drop(true);
        cmd
    }
}

pub struct SshMachine(ConnectionProfile);
#[async_trait]
impl Machine for SshMachine {
    fn ownership(&self) -> MachineOwnership {
        MachineOwnership::External
    }
    async fn ensure_available(&mut self) -> Result<()> {
        let mut cmd = self.0.independent();
        cmd.args(["-T", "-n"]).arg(&self.0.target.ssh).arg("true");
        ensure!(
            tokio::time::timeout(Duration::from_secs(15), cmd.status())
                .await??
                .success(),
            "SSH target is not accessible"
        );
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        bail!("external SSH machines are not managed by enclo")
    }
}

pub struct SshServices {
    profile: ConnectionProfile,
    runtime: Runtime,
    owner: bool,
    child: Option<Child>,
    lock: Option<File>,
    owns_socket: bool,
    prepared: Option<host_services::Prepared>,
    host: Option<host_services::HostServices>,
    source_revision: Option<String>,
    desktop: Option<crate::desktop::Desktop>,
}
#[async_trait]
impl Services for SshServices {
    async fn ensure_ready(&mut self) -> Result<()> {
        if !self.owner {
            return self.runtime.check(&self.profile).await;
        }
        if self.child.is_some() {
            return self.runtime.check(&self.profile).await;
        }
        self.lock = Some(self.runtime.lock()?);
        let result = self.start().await;
        if result.is_err() {
            self.stop().await?;
        }
        result
    }
    async fn stop(&mut self) -> Result<()> {
        if !self.owner {
            return Ok(());
        }
        if self.owns_socket {
            let _ = fs::remove_file(self.runtime.dir.join("ready"));
        }
        if let Some(child) = &mut self.child {
            terminate(child).await?;
        }
        self.child = None;
        if let Some(desktop) = &mut self.desktop {
            desktop.stop().await?;
        }
        self.desktop = None;
        if let Some(mut host) = self.host.take() {
            host.stop(&self.profile).await?;
        }
        if self.owns_socket {
            let _ = fs::remove_file(self.runtime.dir.join("services-env"));
        }
        // Keep the directory and lock inode stable to avoid lock replacement races.
        if self.owns_socket {
            let _ = fs::remove_file(self.runtime.dir.join("ready"));
            match fs::remove_file(self.runtime.socket()) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        self.owns_socket = false;
        self.lock = None;
        Ok(())
    }
    async fn wait(&mut self) -> Result<()> {
        if let Some(child) = &mut self.child {
            let status = child.wait().await?;
            bail!("SSH service transport exited: {status}");
        }
        loop {
            self.runtime.check(&self.profile).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
impl SshServices {
    async fn start(&mut self) -> Result<()> {
        // Under our exclusive lock, stale sockets are safe to remove, but don't
        // steal an orphaned live master left behind by an older supervisor.
        ensure!(
            self.runtime.transport_check(&self.profile).await.is_err(),
            "a service transport is already running"
        );
        self.owns_socket = true;
        let _ = fs::remove_file(self.runtime.dir.join("ready"));
        if self.runtime.socket().exists() {
            fs::remove_file(self.runtime.socket())?;
        }
        if let Some(prepared) = &self.prepared {
            self.host = Some(host_services::HostServices::new(
                prepared.clone(),
                &self.runtime.dir,
            )?);
            self.host.as_mut().unwrap().start().await?;
        }
        if let Some(plan) = &self.profile.local_desktop {
            self.desktop = Some(crate::desktop::Desktop::start(plan.clone()).await?);
        }
        let mut cmd = self.profile.command();
        cmd.args(["-M", "-N", "-T", "-n", "-o", "ExitOnForwardFailure=yes"])
            .arg("-S")
            .arg(self.runtime.socket());
        // Start the master without any inherited forwards, then add exactly
        // the profile's requested forwards through its control interface.
        cmd.args(["-o", "ClearAllForwardings=yes"])
            .arg(&self.profile.target.ssh);
        self.child = Some(cmd.spawn()?);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
                bail!("SSH transport startup failed: {status}");
            }
            if self.runtime.transport_check(&self.profile).await.is_ok() {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "SSH transport readiness timed out"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut forwards = self.profile.forwards.clone();
        if let Some(host) = &mut self.host {
            forwards.extend(host.prepare_remote(&self.profile).await?);
        }
        for forward in &forwards {
            let mut cmd = self.runtime.control_command("forward");
            cmd.arg(match forward.direction {
                Direction::Local => "-L",
                Direction::Remote => "-R",
            })
            .arg(&forward.spec)
            .arg(&self.profile.target.ssh);
            ensure!(
                tokio::time::timeout(Duration::from_secs(10), cmd.status())
                    .await??
                    .success(),
                "failed to establish forward: {}",
                forward.spec
            );
        }
        if let Some(host) = &mut self.host {
            ensure!(
                !host.exited()?,
                "host secrets service exited during startup; see host-services.log"
            );
            host.publish(&self.profile).await?;
            if let Some(file) = host.environment_file() {
                host_services::private_write(
                    &self.runtime.dir.join("services-env"),
                    file.as_bytes(),
                )?;
            }
        }
        // Socket existence alone isn't service readiness: all forwards must succeed.
        fs::write(
            self.runtime.dir.join("ready"),
            profile_revision(&self.profile)?.as_bytes(),
        )?;
        Ok(())
    }
}

pub struct SshSessions {
    profile: ConnectionProfile,
    args: Vec<String>,
    environment_file: Option<String>,
}
#[async_trait]
impl Sessions for SshSessions {
    type Handle = Child;
    async fn open(&mut self) -> Result<Child> {
        let mut cmd = self.profile.independent();
        cmd.arg(if self.args.is_empty() { "-t" } else { "-T" })
            .arg(&self.profile.target.ssh);
        let mut script = session_command(
            &self.profile.session,
            &self.args,
            self.environment_file.is_none(),
        )?;
        if let Some(file) = &self.environment_file {
            let command = script.unwrap_or_else(|| "exec \"${SHELL:-/bin/sh}\"".into());
            script = Some(format!(". {} || exit; {command}", quote(file)));
        }
        if let Some(script) = script {
            cmd.arg(script);
        }
        Ok(cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("launch SSH session; ensure OpenSSH is available on PATH")?)
    }
    async fn close(&mut self, handle: &mut Child) -> Result<()> {
        terminate(handle).await
    }
    async fn wait(&mut self, handle: &mut Child) -> Result<i32> {
        Ok(exit_code(handle.wait().await?))
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn session_command(config: &SessionConfig, args: &[String], login: bool) -> Result<Option<String>> {
    ensure!(
        !args.iter().any(|s| s.contains('\0')),
        "command arguments must not contain NUL"
    );
    if args.is_empty() && config.workdir.is_none() && config.env.is_empty() {
        return Ok(None);
    }
    let mut script = String::new();
    for (key, value) in &config.env {
        script.push_str(&format!("export {key}={}; ", quote(value)));
    }
    if let Some(dir) = &config.workdir {
        script.push_str(&format!("cd -- {} || exit; ", quote(dir)));
    }
    script.push_str("exec ");
    if args.is_empty() {
        script.push_str(if login {
            "\"${SHELL:-/bin/sh}\" -l"
        } else {
            "\"${SHELL:-/bin/sh}\""
        });
    } else {
        script.push_str(&args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" "));
    }
    Ok(Some(script))
}
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}
async fn terminate(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    child.wait().await?;
    Ok(())
}

fn environment(
    profile: ConnectionProfile,
    path: &Path,
    root: Option<&Path>,
    owner: bool,
    args: Vec<String>,
) -> Result<Environment<SshMachine, SshServices, SshSessions>> {
    let runtime = Runtime::resolve(path, root)?;
    let environment_file = if profile.services.secrets.is_some() {
        Some(
            fs::read_to_string(runtime.dir.join("services-env"))
                .context("services environment is not ready")?,
        )
    } else {
        None
    };
    Ok(Environment::new(
        SshMachine(profile.clone()),
        SshServices {
            profile: profile.clone(),
            runtime,
            owner,
            child: None,
            lock: None,
            owns_socket: false,
            prepared: None,
            host: None,
            source_revision: None,
            desktop: None,
        },
        SshSessions {
            profile,
            args,
            environment_file,
        },
    ))
}

async fn interrupted() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => () }
    Ok(())
}

fn profile_revision(profile: &ConnectionProfile) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(profile)?)
    ))
}

pub async fn shell(
    path: &Path,
    root: Option<&Path>,
    config_root: &Path,
    state_root: Option<&Path>,
    args: Vec<String>,
) -> Result<i32> {
    ConnectionProfile::load(path)?;
    supervisor::ensure_running(path, root, config_root, state_root).await?;
    let mut profile = ConnectionProfile::load(path)?;
    profile
        .resolve_host(config_root, root, state_root, false)
        .await?;
    let runtime = Runtime::resolve(path, root)?;
    runtime.check(&profile).await?;
    let mut env = environment(profile.clone(), path, root, false, args)?;
    let registry = crate::session_registry::Registry::resolve(path, root)?;
    let id = env.open_session().await?;
    let registration = match registry.register(profile.target.ssh) {
        Ok(registration) => registration,
        Err(error) => {
            env.close_session(id).await?;
            return Err(error);
        }
    };
    eprintln!("Session {}", registration.info.id);
    let result = loop {
        tokio::select! {
            result = env.wait_session(id) => break result,
            result = interrupted() => break result.map(|_| 130),
            request = registration.next_request() => {
                match request {
                    Ok(Some(mut reply)) => {
                        match env.close_session(id).await {
                            Ok(()) => { let _ = reply.confirm().await; break Ok(143); }
                            Err(error) => { let _ = reply.reject(&error.to_string()).await; }
                        }
                    }
                    Ok(None) => (),
                    Err(error) => tracing::warn!("session control request failed: {error}"),
                }
            }
        }
    };
    env.close_session(id).await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_target_schema_is_exclusive() {
        let profile: ConnectionProfile = toml::from_str("[target]\nhost='dev'").unwrap();
        profile.validate().unwrap();

        for target in [
            "ssh='host'\nhost='dev'",
            "host='../dev'",
            "host='dev'\nconfig='ssh_config'",
            "",
        ] {
            let profile: ConnectionProfile =
                toml::from_str(&format!("[target]\n{target}")).unwrap();
            assert!(profile.validate().is_err());
        }
    }
    #[test]
    fn profile_rejects_unknown_fields_and_option_destinations() {
        for text in [
            "[target]\nssh='-oProxyCommand=bad'",
            "[target]\nssh='host name'",
            "[target]\nssh='host'\nimage='wrong'",
        ] {
            let result = toml::from_str::<ConnectionProfile>(text)
                .and_then(|p| p.validate().map(|_| p).map_err(serde::de::Error::custom));
            assert!(result.is_err());
        }
    }
    #[test]
    fn remote_command_quotes_arguments_and_environment() {
        let config = SessionConfig {
            workdir: Some("/tmp/a b".into()),
            env: BTreeMap::from([("VALUE".into(), "$(touch bad)'".into())]),
        };
        let script = session_command(
            &config,
            &["printf".into(), "%s".into(), "a' b;$(false)".into()],
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(script, "export VALUE='$(touch bad)'\\'''; cd -- '/tmp/a b' || exit; exec 'printf' '%s' 'a'\\'' b;$(false)'");
    }
}
