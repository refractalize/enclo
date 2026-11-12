//! Detached launch uses a private inherited socket, never a stale ready file.
use super::*;
use std::os::unix::{io::FromRawFd, net::UnixStream as StdStream};

struct Launch {
    child: Child,
    ready: bool,
}
impl Drop for Launch {
    fn drop(&mut self) {
        if !self.ready {
            let _ = self.child.start_kill();
        }
    }
}

pub async fn start(
    path: &Path,
    root: Option<&Path>,
    config_root: &Path,
    state_root: Option<&Path>,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let path = path.canonicalize().with_context(|| {
        format!(
            "resolve connection profile {}; check --config-dir or ENCLO_CONFIG_DIR",
            path.display()
        )
    })?;
    ConnectionProfile::load(&path)?;
    let runtime = Runtime::resolve(&path, root)?;
    crate::connection_runtime::prepare(&runtime.dir)?;
    let log_path = runtime.dir.join("supervisor.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&log_path)?;
    let (parent, child_socket) = StdStream::pair()?;
    parent.set_nonblocking(true)?;
    let mut parent = UnixStream::from_std(parent)?;
    let fd = child_socket.as_raw_fd();
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--config-dir")
        .arg(if config_root.is_absolute() {
            config_root.to_path_buf()
        } else {
            std::env::current_dir()?.join(config_root)
        });
    if let Some(state) = state_root {
        command.arg("--state-dir").arg(state);
    }
    command
        .args(["services", "run", "--profile"])
        .arg(&path)
        .arg("--runtime-dir")
        .arg(
            runtime
                .dir
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .canonicalize()?,
        )
        .arg("--startup-fd")
        .arg(fd.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut launch = Launch {
        child: command.spawn()?,
        ready: false,
    };
    drop(child_socket);
    let startup = async {
        let mut bytes = Vec::new();
        (&mut parent)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= 1024 * 1024, "startup response too large");
        let reply: Reply = serde_json::from_slice(&bytes)
            .context("supervisor exited before reporting readiness")?;
        if let Some(error) = reply.error {
            bail!("{error}");
        }
        reply.status.context("supervisor did not report readiness")
    };
    let signal = interrupted();
    tokio::pin!(signal);
    let result = tokio::select! {
        result = tokio::time::timeout(timeout, startup) => result.context("services startup timed out").and_then(|r| r),
        _ = &mut signal => Err(anyhow::anyhow!("services startup interrupted")),
    };
    match result {
        Ok(status) => {
            launch.ready = true;
            eprintln!("Services started; log: {}", log_path.display());
            Ok(status)
        }
        Err(error) => {
            if let Some(pid) = launch.child.id() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
            if tokio::time::timeout(Duration::from_secs(10), launch.child.wait())
                .await
                .is_err()
            {
                let _ = launch.child.kill().await;
            }
            Err(error)
                .with_context(|| format!("could not start services; log: {}", log_path.display()))
        }
    }
}

/// The FD is supplied only by our launcher. Mark it close-on-exec immediately so
/// SSH, Podman and credential providers cannot keep the startup channel open.
pub async fn run_with_startup(
    path: &Path,
    root: Option<&Path>,
    config_root: &Path,
    state_root: Option<&Path>,
    fd: Option<i32>,
) -> Result<()> {
    let mut startup = match fd {
        Some(fd) => {
            ensure!(fd > 2, "invalid startup FD");
            ensure!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != -1,
                "invalid startup FD"
            );
            let stream = unsafe { StdStream::from_raw_fd(fd) };
            stream
                .peer_addr()
                .context("startup FD is not a Unix socket")?;
            stream.set_nonblocking(true)?;
            Some(UnixStream::from_std(stream)?)
        }
        None => None,
    };
    let result = run_inner(path, root, config_root, state_root, &mut startup).await;
    if let (Some(mut stream), Err(error)) = (startup.take(), &result) {
        let reply = Reply {
            status: None,
            error: Some(format!("{error:#}")),
        };
        let _ = stream.write_all(&serde_json::to_vec(&reply)?).await;
    }
    result
}
