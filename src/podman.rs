use std::ffi::OsString;
use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::debug;

pub async fn run(args: &[OsString]) -> Result<tokio::process::Child> {
    debug!("podman run {:?}", args);
    let child = Command::new("podman")
        .arg("run")
        .args(args)
        .spawn()
        .context("failed to spawn podman run")?;
    Ok(child)
}

pub async fn wait_running(container: &str, timeout_secs: u64) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let state = container_state(container).await?;
        if state == "running" {
            return Ok(());
        }
        if state == "exited" || state == "stopped" {
            anyhow::bail!("container {} exited before becoming ready", container);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("container {} did not reach running state within {}s", container, timeout_secs);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub async fn exec_as(container: &str, user: &str, args: &[String]) -> Result<()> {
    debug!("podman exec --user {} {} {:?}", user, container, args);
    let status = Command::new("podman")
        .arg("exec")
        .arg(format!("--user={}", user))
        .arg(container)
        .args(args)
        .status()
        .await
        .context("failed to spawn podman exec")?;
    if !status.success() {
        anyhow::bail!("podman exec exited with {}", status);
    }
    Ok(())
}

pub async fn exec(container: &str, shell: &str, env_file: &std::path::Path, workdir: &str) -> Result<()> {
    debug!("podman exec into {}", container);
    let status = Command::new("podman")
        .arg("exec")
        .arg("--interactive")
        .arg("--tty")
        .arg(format!("--env-file={}", env_file.display()))
        .arg(format!("--workdir={}", workdir))
        .arg(container)
        .arg(shell)
        .status()
        .await
        .context("failed to spawn podman exec")?;
    if !status.success() {
        anyhow::bail!("podman exec exited with {}", status);
    }
    Ok(())
}

pub async fn stop(container: &str) -> Result<()> {
    debug!("podman stop {}", container);
    let status = Command::new("podman")
        .args(["stop", container])
        .status()
        .await
        .context("failed to spawn podman stop")?;
    if !status.success() {
        anyhow::bail!("podman stop exited with {}", status);
    }
    Ok(())
}

pub async fn container_state(container: &str) -> Result<String> {
    let out = Command::new("podman")
        .args(["inspect", "--format", "{{.State.Status}}", container])
        .output()
        .await
        .context("failed to spawn podman inspect")?;

    if !out.status.success() {
        return Ok("not found".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn rm_if_exists(container: &str) -> Result<()> {
    let state = container_state(container).await?;
    if state != "not found" {
        Command::new("podman")
            .args(["rm", "--force", container])
            .status()
            .await
            .context("failed to spawn podman rm")?;
    }
    Ok(())
}
