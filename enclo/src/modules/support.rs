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
    group: Arc<Mutex<Option<u32>>>,
}

impl ManagedChild {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            inner: Arc::new(Mutex::new(None)),
            group: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn store(&self, child: Child) {
        *self.inner.lock().await = Some(child);
    }

    /// The caller must spawn this child with process_group(0).
    pub async fn store_group(&self, child: Child) {
        *self.group.lock().await = child.id();
        self.store(child).await;
    }

    /// Terminate the leader gracefully, then remove any surviving descendants.
    pub async fn stop_group(&self, grace: Duration) -> anyhow::Result<()> {
        let group = self.group.lock().await.take();
        if group.is_none() {
            return self.kill().await;
        }
        let group = group.unwrap() as libc::pid_t;
        if let Some(mut child) = self.inner.lock().await.take() {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
                let _ = tokio::time::timeout(grace, child.wait()).await;
            }
            // kill_on_drop cannot run after SIGKILL. The isolated process group
            // also covers providers and their in-flight credential commands.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            let _ = child.wait().await;
        } else {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        Ok(())
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

/// Like `resolve_binary_path`, but on macOS looks in the `linux/` subdirectory
/// alongside the host binary first, so the correct Linux build is mounted into
/// containers rather than the macOS build.
pub fn resolve_container_binary_path(binary: &str) -> anyhow::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("linux").join(binary);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    resolve_binary_path(binary)
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

pub async fn ensure_unix_socket_connectable(path: &Path, description: &str) -> anyhow::Result<()> {
    tokio::net::UnixStream::connect(path)
        .await
        .map(|_| ())
        .map_err(|e| {
            anyhow::anyhow!(
                "{description} exists at {} but is not connectable: {}",
                path.display(),
                e
            )
        })
}

pub async fn wait_for_container_socket(
    container: &str,
    socket_path: &str,
    attempts: u32,
    interval: Duration,
    description: &str,
) -> anyhow::Result<()> {
    for _ in 0..attempts {
        let ready = tokio::process::Command::new("podman")
            .args(["exec", container, "test", "-S", socket_path])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ready {
            return Ok(());
        }
        tokio::time::sleep(interval).await;
    }
    anyhow::bail!(
        "{description} never appeared at {socket_path} in container {container} after {}ms",
        attempts as u64 * interval.as_millis() as u64,
    )
}

#[cfg(all(test, target_os = "linux"))]
mod group_tests {
    use super::*;
    #[tokio::test]
    async fn forced_cleanup_kills_descendants_even_if_the_leader_has_exited() {
        for exit_leader in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "enclo-group-{}-{}",
                std::process::id(),
                exit_leader
            ));
            std::fs::create_dir_all(&root).unwrap();
            let pidfile = root.join("child.pid");
            let code = r#"import os,sys,signal,time
from pathlib import Path
if os.fork()==0:
 signal.signal(signal.SIGTERM,signal.SIG_IGN)
 Path(sys.argv[1]).write_text(str(os.getpid()))
 time.sleep(60)
else:
 if sys.argv[2]=='true':os._exit(0)
 signal.signal(signal.SIGTERM,signal.SIG_IGN)
 time.sleep(60)
"#;
            let child = tokio::process::Command::new("/usr/bin/python3")
                .args(["-c", code])
                .arg(&pidfile)
                .arg(exit_leader.to_string())
                .process_group(0)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            struct Cleanup(i32, std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    unsafe {
                        libc::kill(-self.0, libc::SIGKILL);
                    }
                    let _ = std::fs::remove_dir_all(&self.1);
                }
            }
            let _cleanup = Cleanup(child.id().unwrap() as i32, root);
            let managed = ManagedChild::new("fixture");
            managed.store_group(child).await;
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !pidfile.exists() {
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let pid = std::fs::read_to_string(&pidfile).unwrap();
            if exit_leader {
                while managed.process_alive().await != Some(false) {
                    assert!(std::time::Instant::now() < deadline);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            managed.stop_group(Duration::from_millis(30)).await.unwrap();
            loop {
                let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .is_some_and(|s| s.split_whitespace().nth(2) != Some("Z"));
                if !alive {
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}
