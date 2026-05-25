use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::{process::Child, sync::Mutex};

#[derive(Clone)]
pub struct ManagedChild {
    name: &'static str,
    inner: Arc<Mutex<Option<Child>>>,
}

impl ManagedChild {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            inner: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn store(&self, child: Child) {
        *self.inner.lock().await = Some(child);
    }

    pub async fn process_alive(&self) -> Option<bool> {
        self.inner
            .lock()
            .await
            .as_mut()
            .map(|child| child.try_wait().ok().flatten().is_none())
    }

    pub async fn send_signal(&self, signal: libc::c_int) -> anyhow::Result<()> {
        let guard = self.inner.lock().await;
        let child = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{} is not running", self.name))?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("{} has no PID (already exited?)", self.name))?;
        let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
        if rc != 0 {
            anyhow::bail!(
                "failed to send signal {} to {} (pid {}): {}",
                signal,
                self.name,
                pid,
                std::io::Error::last_os_error()
            );
        }
        tracing::info!("sent signal {} to {} (pid {})", signal, self.name, pid);
        Ok(())
    }

    pub async fn kill(&self) -> anyhow::Result<()> {
        if let Some(mut child) = self.inner.lock().await.take() {
            tracing::info!("stopping {}", self.name);
            child.kill().await?;
        }
        Ok(())
    }
}

pub fn resolve_binary_path(binary: &str) -> anyhow::Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    which::which(binary)
        .map_err(|_| anyhow::anyhow!("{binary} not found (checked sibling directory and $PATH)"))
}

pub async fn wait_for_path(
    path: &Path,
    attempts: u32,
    interval: Duration,
    description: &str,
) -> anyhow::Result<()> {
    for _ in 0..attempts {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(interval).await;
    }

    anyhow::bail!(
        "{description} did not appear at {} after {}ms",
        path.display(),
        attempts as u64 * interval.as_millis() as u64,
    )
}

pub async fn wait_for_unix_socket(
    path: &Path,
    attempts: u32,
    interval: Duration,
    description: &str,
    child: Option<&ManagedChild>,
) -> anyhow::Result<()> {
    for _ in 0..attempts {
        if let Some(child) = child {
            if let Some(false) = child.process_alive().await {
                anyhow::bail!("{description} exited before becoming ready");
            }
        }
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(interval).await;
    }

    anyhow::bail!(
        "{description} socket never became connectable at {} after {}ms",
        path.display(),
        attempts as u64 * interval.as_millis() as u64,
    )
}

pub async fn wait_for_tcp(
    addr: std::net::SocketAddr,
    attempts: u32,
    interval: Duration,
    description: &str,
) -> anyhow::Result<()> {
    for _ in 0..attempts {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(interval).await;
    }

    anyhow::bail!("{description} did not become ready at {addr}")
}

pub async fn ensure_unix_socket_connectable(path: &Path, description: &str) -> anyhow::Result<()> {
    tokio::net::UnixStream::connect(path)
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{description} exists at {} but is not connectable: {}", path.display(), e))
}
