//! Enclo control API, distinct from OpenSSH's private multiplexing protocol.
use super::*;
#[path = "supervisor_start.rs"]
mod detached;
pub use detached::{run_with_startup, start};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

#[derive(Serialize)]
pub struct Status {
    pub supervisor_running: bool,
    pub transport_ready: bool,
    pub profile: PathBuf,
    pub loaded_revision: Option<String>,
    pub configured_revision: Option<String>,
    pub target: Option<String>,
    pub forwards: Vec<Forward>,
    pub last_error: Option<String>,
    pub secrets: Option<host_services::Status>,
    pub desktop_running: bool,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    status: Option<serde_json::Value>,
    error: Option<String>,
}
struct ListenerGuard {
    listener: UnixListener,
    path: PathBuf,
    _lock: File,
}
impl Drop for ListenerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
impl ListenerGuard {
    fn bind(runtime: &Runtime) -> Result<Self> {
        crate::connection_runtime::prepare(&runtime.dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(runtime.dir.join("supervisor-lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "services already have a foreground supervisor"
        );
        let path = runtime.dir.join("supervisor.sock");
        match fs::remove_file(&path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(Self {
            listener: UnixListener::bind(&path)?,
            path,
            _lock: lock,
        })
    }
}
fn stopped(path: &Path) -> Status {
    Status {
        supervisor_running: false,
        transport_ready: false,
        profile: path.to_path_buf(),
        loaded_revision: None,
        configured_revision: None,
        target: None,
        forwards: vec![],
        last_error: None,
        secrets: None,
        desktop_running: false,
    }
}

/// Addressed by profile filename, so malformed edited configuration can still
/// be inspected, rejected by reload, or stopped without losing the supervisor.
pub async fn request(
    path: &Path,
    root: Option<&Path>,
    operation: &str,
) -> Result<serde_json::Value> {
    request_timeout(path, root, operation, Duration::from_secs(120)).await
}
async fn request_timeout(
    path: &Path,
    root: Option<&Path>,
    operation: &str,
    timeout: Duration,
) -> Result<serde_json::Value> {
    ensure!(
        ["status", "stop", "reload"].contains(&operation),
        "unknown supervisor operation"
    );
    let runtime = Runtime::resolve(path, root)?;
    if !runtime.dir.exists() && operation == "status" {
        return Ok(serde_json::to_value(stopped(path))?);
    }
    runtime.check_directory()?;
    let mut stream = match UnixStream::connect(runtime.dir.join("supervisor.sock")).await {
        Ok(stream) => stream,
        Err(e)
            if operation == "status"
                && matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
        {
            return Ok(serde_json::to_value(stopped(path))?)
        }
        Err(e) => return Err(e).context("services supervisor unavailable; start services first"),
    };
    let response = tokio::time::timeout(timeout, async {
        stream.write_all(operation.as_bytes()).await?;
        stream.shutdown().await?;
        let mut bytes = Vec::new();
        (&mut stream)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= 1024 * 1024, "supervisor response too large");
        Ok::<_, anyhow::Error>(serde_json::from_slice::<Reply>(&bytes)?)
    })
    .await
    .context("supervisor request timed out; inspect status before retrying")??;
    if let Some(error) = response.error {
        bail!("{error}");
    }
    response.status.context("supervisor returned no status")
}
async fn send(stream: &mut UnixStream, result: Result<Status>) -> Result<()> {
    let reply = match result {
        Ok(status) => Reply {
            status: Some(serde_json::to_value(status)?),
            error: None,
        },
        Err(error) => Reply {
            status: None,
            error: Some(format!("{error:#}")),
        },
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        stream.write_all(&serde_json::to_vec(&reply)?).await?;
        stream.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}
fn snapshot(path: &Path, services: &SshServices, error: &Option<String>) -> Result<Status> {
    Ok(Status {
        supervisor_running: true,
        transport_ready: services.child.is_some() && services.runtime.dir.join("ready").is_file(),
        profile: path.to_path_buf(),
        loaded_revision: Some(profile_revision(&services.profile)?),
        configured_revision: services.source_revision.clone(),
        target: Some(services.profile.target.ssh.clone()),
        forwards: services.profile.forwards.clone(),
        last_error: error.clone(),
        desktop_running: services.desktop.is_some(),
        secrets: services
            .host
            .as_ref()
            .map(host_services::HostServices::status),
    })
}
async fn reload(
    path: &Path,
    services: &mut SshServices,
    config_root: &Path,
    root: Option<&Path>,
    state_root: Option<&Path>,
) -> Result<()> {
    // Validate completely before disturbing the currently working transport.
    let mut desired = ConnectionProfile::load(path)?;
    let revision = profile_revision(&desired)?;
    desired
        .resolve_container(config_root, root, state_root, true)
        .await?;
    let prepared = host_services::Prepared::load(&desired, &services.runtime.dir).await?;
    let old_revision = services.source_revision.clone();
    let old_prepared = services.prepared.clone();
    let old = services.profile.clone();
    services.stop().await?;
    services.profile = desired;
    services.source_revision = Some(revision);
    services.prepared = prepared;
    if let Err(error) = services.ensure_ready().await {
        services.profile = old;
        services.source_revision = old_revision;
        services.prepared = old_prepared;
        if let Err(rollback) = services.ensure_ready().await {
            bail!(
                "reload failed: {error:#}; restoring previous transport also failed: {rollback:#}"
            );
        }
        bail!("reload failed; previous configuration restored: {error:#}");
    }
    Ok(())
}

pub async fn run(
    path: &Path,
    root: Option<&Path>,
    config_root: &Path,
    state_root: Option<&Path>,
) -> Result<()> {
    run_with_startup(path, root, config_root, state_root, None).await
}

async fn run_inner(
    path: &Path,
    root: Option<&Path>,
    config_root: &Path,
    state_root: Option<&Path>,
    startup: &mut Option<UnixStream>,
) -> Result<()> {
    let runtime = Runtime::resolve(path, root)?;
    let path = path.canonicalize()?;
    let mut profile = ConnectionProfile::load(&path)?;
    let source_revision = Some(profile_revision(&profile)?);
    // Own the supervisor namespace before provisioning a machine.
    let listener = ListenerGuard::bind(&runtime)?;
    let signal = interrupted();
    tokio::pin!(signal);
    let prepared = tokio::select! {
        result = async {
            profile.resolve_container(config_root, root, state_root, true).await?;
            host_services::Prepared::load(&profile, &runtime.dir).await
        } => result?,
        _ = &mut signal => bail!("services startup interrupted"),
    };
    let mut services = SshServices {
        profile,
        runtime,
        owner: true,
        child: None,
        lock: None,
        owns_socket: false,
        prepared,
        host: None,
        source_revision,
        desktop: None,
    };
    let started = tokio::select! { result = services.ensure_ready() => result, result = &mut signal => result.and_then(|_| Err(anyhow::anyhow!("connection startup interrupted"))) };
    if let Err(error) = started {
        services.stop().await?;
        return Err(error);
    }
    if let Some(mut stream) = startup.take() {
        let reply = Reply {
            status: Some(serde_json::to_value(snapshot(&path, &services, &None)?)?),
            error: None,
        };
        if let Err(error) = stream.write_all(&serde_json::to_vec(&reply)?).await {
            services.stop().await?;
            return Err(error).context("services launcher disconnected before readiness");
        }
    }
    eprintln!(
        "Services ready. Open sessions with enclo session --profile {}",
        path.display()
    );
    let mut last_error = None;
    let mut poll = tokio::time::interval(Duration::from_millis(250));
    let outcome = loop {
        tokio::select! {
            result = &mut signal => break result,
            _ = poll.tick() => {
                if let Some(desktop) = &mut services.desktop {
                    match desktop.exited() {
                        Ok(true) => {
                            last_error = Some("desktop proxy exited; inspect dbus.log and reload to reconnect".into());
                            if let Err(error) = services.stop().await { break Err(error); }
                        }
                        Ok(false) => (),
                        Err(error) => break Err(error),
                    }
                }
                if let Some(host) = &mut services.host {
                    match host.exited() {
                        Ok(true) => {
                            last_error = Some("host secrets service exited; inspect host-services.log and reload to reconnect".into());
                            if let Err(error) = services.stop().await { break Err(error); }
                        }
                        Ok(false) => (),
                        Err(error) => break Err(error),
                    }
                }
                if let Some(child) = services.child.as_mut() {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            last_error = Some(format!("SSH transport exited: {status}; reload to reconnect"));
                            if let Err(error) = services.stop().await { break Err(error); }
                        }
                        Ok(None) => (),
                        Err(error) => break Err(error.into()),
                    }
                }
            }
            accepted = listener.listener.accept() => {
                let (mut stream, _) = match accepted { Ok(value) => value, Err(error) => break Err(error.into()) };
                let mut bytes = Vec::new();
                if !matches!(tokio::time::timeout(Duration::from_secs(1), (&mut stream).take(32).read_to_end(&mut bytes)).await, Ok(Ok(_))) { continue; }
                match bytes.as_slice() {
                    b"status" => { let _ = send(&mut stream, snapshot(&path, &services, &last_error)).await; }
                    b"reload" => {
                        let result = reload(&path, &mut services, config_root, root, state_root).await;
                        last_error = result.as_ref().err().map(|e| format!("{e:#}"));
                        let reply = result.and_then(|_| snapshot(&path, &services, &last_error));
                        let _ = send(&mut stream, reply).await;
                    }
                    b"stop" => {
                        match services.stop().await {
                            Ok(()) => { let _ = send(&mut stream, Ok(stopped(&path))).await; break Ok(()); }
                            Err(error) => { last_error = Some(format!("{error:#}")); let _ = send(&mut stream, Err(error)).await; }
                        }
                    }
                    _ => { let _ = send(&mut stream, Err(anyhow::anyhow!("unknown supervisor request"))).await; }
                }
            }
        }
    };
    services.stop().await?;
    outcome
}

/// Serialize session-triggered startup/recovery across concurrent callers.
/// This lock is distinct from the supervisor's ownership lock.
pub async fn ensure_running(
    path: &Path,
    root: Option<&Path>,
    config_root: &Path,
    state_root: Option<&Path>,
) -> Result<()> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve connection profile {}", path.display()))?;
    let runtime = Runtime::resolve(&path, root)?;
    crate::connection_runtime::prepare(&runtime.dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(runtime.dir.join("ensure.lock"))?;
    let timeout = Duration::from_secs(600);
    let deadline = tokio::time::Instant::now() + timeout;
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.kind() == std::io::ErrorKind::WouldBlock,
            "lock services startup: {error}"
        );
        ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for another session to start services"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut status = request_timeout(&path, root, "status", timeout).await?;
    if status["supervisor_running"] != true {
        if let Err(error) = start(&path, root, config_root, state_root, timeout).await {
            // A manually launched supervisor may have won the ownership race.
            status = request_timeout(&path, root, "status", timeout)
                .await
                .unwrap_or(serde_json::Value::Null);
            if status["supervisor_running"] != true {
                return Err(error);
            }
        } else {
            return Ok(());
        }
    }
    ensure!(status["profile"].as_str() == path.to_str(), "connection name is already owned by another profile; use a different connection name or runtime directory");
    let mut desired = ConnectionProfile::load(&path)?;
    if let Some(revision) = status["configured_revision"].as_str() {
        ensure!(
            revision == profile_revision(&desired)?,
            "profile differs from running services; reload services explicitly first"
        );
    }
    let resolved = desired
        .resolve_container(config_root, root, state_root, false)
        .await;
    if status["configured_revision"].is_null()
        && status["transport_ready"] == true
        && resolved.is_ok()
    {
        ensure!(
            status["loaded_revision"].as_str() == Some(&profile_revision(&desired)?),
            "profile differs from running services; reload services explicitly first"
        );
    }
    if status["transport_ready"] != true
        || (resolved.is_ok()
            && status["loaded_revision"].as_str() != Some(&profile_revision(&desired)?))
        || runtime.transport_check(&desired).await.is_err()
        || resolved.is_err()
    {
        eprintln!("Reconnecting services for {}", path.display());
        let status = request_timeout(&path, root, "reload", timeout).await?;
        ensure!(
            status["transport_ready"] == true,
            "services did not become ready after reconnecting"
        );
    }
    Ok(())
}
